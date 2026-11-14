//! PVE API client.
//!
//! Two transports:
//! - `Http`: https://host:8006/api2/json with a ticket cookie + CSRF token. TLS uses
//!   fingerprint pinning (TOFU for remote hosts, accept-any for 127.0.0.1).
//! - `Pvesh`: runs `pvesh` locally (rescue mode, when pveproxy is down). Requires root.

use anyhow::{anyhow, bail, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::util::urlenc;

#[derive(Clone, Debug)]
pub struct Auth {
    pub user: String,
    pub ticket: String,
    pub csrf: String,
    pub at: Instant,
}

pub enum LoginResult {
    Ok(Auth),
    NeedTfa { user: String, challenge: String },
}

// ---------------------------------------------------------------------------
// TLS: fingerprint pinning verifier
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct PinVerifier {
    /// None = accept any certificate (localhost, or first contact to learn the fingerprint).
    expected: Option<String>,
    pub seen: Arc<Mutex<Option<String>>>,
    provider: Arc<CryptoProvider>,
}

pub fn fingerprint(der: &[u8]) -> String {
    let d = Sha256::digest(der);
    d.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity.as_ref());
        *self.seen.lock().unwrap() = Some(fp.clone());
        match &self.expected {
            None => Ok(ServerCertVerified::assertion()),
            Some(e) if e.eq_ignore_ascii_case(&fp) => Ok(ServerCertVerified::assertion()),
            Some(_) => Err(rustls::Error::General(format!(
                "certificate fingerprint changed: {fp}"
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub fn tls_config(expected: Option<String>) -> (Arc<ClientConfig>, Arc<Mutex<Option<String>>>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let seen = Arc::new(Mutex::new(None));
    let verifier = PinVerifier { expected, seen: seen.clone(), provider: provider.clone() };
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("tls versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    (Arc::new(cfg), seen)
}

/// Connect once and return the server certificate fingerprint (for TOFU prompts).
pub fn probe_fingerprint(host: &str, port: u16) -> Result<String> {
    let (cfg, seen) = tls_config(None);
    let agent = ureq::AgentBuilder::new()
        .tls_config(cfg)
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build();
    let url = format!("https://{}:{port}/api2/json/access/domains", host_for_url(host));
    let _ = agent.get(&url).call();
    let fp = seen.lock().unwrap().clone();
    fp.ok_or_else(|| anyhow!("could not connect to {host}:{port}"))
}

fn host_for_url(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

pub fn known_fingerprint(host: &str) -> Option<String> {
    let s = std::fs::read_to_string(crate::config::known_hosts_path()).ok()?;
    s.lines()
        .filter_map(|l| l.split_once(' '))
        .find(|(h, _)| *h == host)
        .map(|(_, fp)| fp.trim().to_string())
}

pub fn save_fingerprint(host: &str, fp: &str) -> Result<()> {
    let path = crate::config::known_hosts_path();
    let path = path.as_str();
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = old
        .lines()
        .filter(|l| l.split_once(' ').map(|(h, _)| h != host).unwrap_or(true))
        .map(|s| s.to_string())
        .collect();
    lines.push(format!("{host} {fp}"));
    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

enum Transport {
    Http {
        agent: ureq::Agent,
        base: String,
        auth: Mutex<Option<Auth>>,
    },
    Pvesh,
}

pub struct Api {
    transport: Transport,
    pub host: String,
    pub port: u16,
    pub tls: Arc<ClientConfig>,
}

impl Api {
    pub fn http(host: &str, port: u16, fingerprint: Option<String>) -> Self {
        let (tls, _) = tls_config(fingerprint);
        let agent = ureq::AgentBuilder::new()
            .tls_config(tls.clone())
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build();
        Self {
            transport: Transport::Http {
                agent,
                base: format!("https://{}:{port}/api2/json", host_for_url(host)),
                auth: Mutex::new(None),
            },
            host: host.to_string(),
            port,
            tls,
        }
    }

    pub fn local() -> Self {
        Self::http("127.0.0.1", 8006, None)
    }

    pub fn pvesh() -> Self {
        let (tls, _) = tls_config(None);
        Self { transport: Transport::Pvesh, host: "127.0.0.1".into(), port: 8006, tls }
    }

    pub fn is_pvesh(&self) -> bool {
        matches!(self.transport, Transport::Pvesh)
    }

    pub fn auth(&self) -> Option<Auth> {
        match &self.transport {
            Transport::Http { auth, .. } => auth.lock().unwrap().clone(),
            Transport::Pvesh => None,
        }
    }

    pub fn set_auth(&self, a: Auth) {
        if let Transport::Http { auth, .. } = &self.transport {
            *auth.lock().unwrap() = Some(a);
        }
    }

    pub fn clear_auth(&self) {
        if let Transport::Http { auth, .. } = &self.transport {
            *auth.lock().unwrap() = None;
        }
    }

    /// Unauthenticated reachability check (also returns the realm list).
    pub fn domains(&self) -> Result<Vec<(String, String)>> {
        let v = self.get("/access/domains")?;
        Ok(crate::util::jarr(&v)
            .iter()
            .map(|d| (crate::util::js(d, "realm"), crate::util::js(d, "comment")))
            .collect())
    }

    pub fn login(&self, user: &str, password: &str) -> Result<LoginResult> {
        let v = self.raw("POST", "/access/ticket", &[("username", user.into()), ("password", password.into())], false)?;
        let d = v.get("data").cloned().unwrap_or(Value::Null);
        let ticket = crate::util::js(&d, "ticket");
        if ticket.is_empty() {
            bail!("login failed");
        }
        let username = crate::util::js(&d, "username");
        if crate::util::jb(&d, "NeedTFA") {
            return Ok(LoginResult::NeedTfa { user: username, challenge: ticket });
        }
        Ok(LoginResult::Ok(Auth {
            user: username,
            ticket,
            csrf: crate::util::js(&d, "CSRFPreventionToken"),
            at: Instant::now(),
        }))
    }

    /// Second factor: `code` is a TOTP code, or "recovery:XXXX".
    pub fn login_tfa(&self, user: &str, challenge: &str, code: &str) -> Result<Auth> {
        let pw = if code.contains(':') { code.to_string() } else { format!("totp:{code}") };
        let v = self.raw(
            "POST",
            "/access/ticket",
            &[("username", user.into()), ("tfa-challenge", challenge.into()), ("password", pw)],
            false,
        )?;
        let d = v.get("data").cloned().unwrap_or(Value::Null);
        let ticket = crate::util::js(&d, "ticket");
        if ticket.is_empty() || ticket.contains("!tfa!") {
            bail!("second factor rejected");
        }
        Ok(Auth {
            user: crate::util::js(&d, "username"),
            ticket,
            csrf: crate::util::js(&d, "CSRFPreventionToken"),
            at: Instant::now(),
        })
    }

    /// Renew the ticket (tickets are valid for 2 hours).
    pub fn renew(&self) -> Result<()> {
        let Some(a) = self.auth() else { return Ok(()) };
        match self.login(&a.user, &a.ticket)? {
            LoginResult::Ok(n) => {
                self.set_auth(n);
                Ok(())
            }
            LoginResult::NeedTfa { .. } => bail!("renew requires TFA"),
        }
    }

    pub fn needs_renew(&self) -> bool {
        self.auth().map(|a| a.at.elapsed() > Duration::from_secs(3600)).unwrap_or(false)
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        Ok(self.get_full(path)?.get("data").cloned().unwrap_or(Value::Null))
    }

    /// Whole response body (some endpoints return extra top-level keys, e.g. network `changes`).
    pub fn get_full(&self, path: &str) -> Result<Value> {
        self.raw("GET", path, &[], true)
    }

    pub fn post(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        Ok(self.raw("POST", path, params, true)?.get("data").cloned().unwrap_or(Value::Null))
    }

    pub fn put(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        Ok(self.raw("PUT", path, params, true)?.get("data").cloned().unwrap_or(Value::Null))
    }

    pub fn delete(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        Ok(self.raw("DELETE", path, params, true)?.get("data").cloned().unwrap_or(Value::Null))
    }

    fn raw(&self, method: &str, path: &str, params: &[(&str, String)], authed: bool) -> Result<Value> {
        match &self.transport {
            Transport::Http { agent, base, auth } => {
                let mut url = format!("{base}{path}");
                let body_params = matches!(method, "POST" | "PUT");
                if !body_params && !params.is_empty() {
                    url.push(if url.contains('?') { '&' } else { '?' });
                    url.push_str(
                        &params
                            .iter()
                            .map(|(k, v)| format!("{}={}", urlenc(k), urlenc(v)))
                            .collect::<Vec<_>>()
                            .join("&"),
                    );
                }
                let mut req = agent.request(method, &url);
                if authed {
                    if let Some(a) = auth.lock().unwrap().as_ref() {
                        req = req.set("Cookie", &format!("PVEAuthCookie={}", a.ticket));
                        if method != "GET" {
                            req = req.set("CSRFPreventionToken", &a.csrf);
                        }
                    }
                }
                let resp = if body_params {
                    let form: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
                    req.send_form(&form)
                } else {
                    req.call()
                };
                match resp {
                    Ok(r) => {
                        let text = r.into_string().context("reading response")?;
                        if text.trim().is_empty() {
                            return Ok(Value::Null);
                        }
                        serde_json::from_str(&text).context("invalid JSON from API")
                    }
                    Err(e) => Err(http_error(e)),
                }
            }
            Transport::Pvesh => pvesh(method, path, params),
        }
    }

    /// Upload a file as multipart/form-data, streamed from disk (ISO images are several GB).
    /// `fields` come first, the file part is named `filename` (like the PVE web UI).
    pub fn upload(&self, path: &str, fields: &[(&str, String)], file: &std::path::Path, name: &str, prog: Arc<UploadProgress>) -> Result<Value> {
        use std::io::{Cursor, Read};
        let Transport::Http { base, auth, .. } = &self.transport else {
            bail!("uploads need the PVE API (not available in rescue mode)");
        };
        let size = std::fs::metadata(file).with_context(|| format!("{}", file.display()))?.len();
        let fh = std::fs::File::open(file).with_context(|| format!("{}", file.display()))?;
        let boundary = format!("----pxvirt{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        // pveproxy extracts the fields in this fixed order before the file part
        let order = |k: &str| ["content", "checksum-algorithm", "checksum"].iter().position(|x| *x == k).unwrap_or(9);
        let mut fields = fields.to_vec();
        fields.sort_by_key(|(k, _)| order(k));
        let (head, tail) = multipart_frame(&boundary, &fields, name);
        prog.total.store(size, Ordering::Relaxed);
        let len = head.len() as u64 + size + tail.len() as u64;
        let body = Cursor::new(head).chain(ProgressReader { inner: fh, prog: prog.clone() }).chain(Cursor::new(tail));
        // own agent: no overall timeout, the transfer may take long
        let agent = ureq::AgentBuilder::new()
            .tls_config(self.tls.clone())
            .timeout_connect(Duration::from_secs(10))
            .timeout_write(Duration::from_secs(120))
            .timeout_read(Duration::from_secs(900))
            .build();
        let mut req = agent
            .post(&format!("{base}{path}"))
            .set("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
            .set("Content-Length", &len.to_string());
        if let Some(a) = auth.lock().unwrap().as_ref() {
            req = req.set("Cookie", &format!("PVEAuthCookie={}", a.ticket)).set("CSRFPreventionToken", &a.csrf);
        }
        match req.send(body) {
            Ok(r) => {
                let text = r.into_string().context("reading response")?;
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                Ok(v.get("data").cloned().unwrap_or(Value::Null))
            }
            Err(_) if prog.cancel.load(Ordering::Relaxed) => bail!("upload cancelled"),
            Err(e) => Err(http_error(e)),
        }
    }

    pub fn base_url(&self) -> String {
        format!("https://{}:{}", host_for_url(&self.host), self.port)
    }
}

fn http_error(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, r) => {
            let reason = r.status_text().to_string();
            let body = r.into_string().unwrap_or_default();
            let errors = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("errors").cloned())
                .map(|e| match e {
                    Value::Object(m) => m
                        .iter()
                        .map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or(&v.to_string()).trim()))
                        .collect::<Vec<_>>()
                        .join("; "),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            if errors.is_empty() {
                anyhow!("{code} {reason}")
            } else {
                anyhow!("{code} {reason} ({errors})")
            }
        }
        e => anyhow!("connection failed: {e}"),
    }
}

/// Progress of a running upload, shared with the UI.
#[derive(Default)]
pub struct UploadProgress {
    pub sent: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

struct ProgressReader<R> {
    inner: R,
    prog: Arc<UploadProgress>,
}

impl<R: std::io::Read> std::io::Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.prog.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("upload cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.prog.sent.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Multipart body around the file data: (fields + file part header, closing boundary).
fn multipart_frame(boundary: &str, fields: &[(&str, String)], name: &str) -> (Vec<u8>, Vec<u8>) {
    let mut head = String::new();
    for (k, v) in fields {
        head.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"));
    }
    let safe = name.replace(['"', '\r', '\n'], "_");
    head.push_str(&format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"filename\"; filename=\"{safe}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    ));
    (head.into_bytes(), format!("\r\n--{boundary}--\r\n").into_bytes())
}

fn pvesh(method: &str, path: &str, params: &[(&str, String)]) -> Result<Value> {
    let cmd = match method {
        "GET" => "get",
        "POST" => "create",
        "PUT" => "set",
        "DELETE" => "delete",
        _ => bail!("unsupported method {method}"),
    };
    let (p, query) = path.split_once('?').unwrap_or((path, ""));
    let mut args: Vec<String> = vec![cmd.into(), p.into(), "--output-format".into(), "json".into()];
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, "1"));
        args.push(format!("--{k}"));
        args.push(urldecode(v));
    }
    for (k, v) in params {
        args.push(format!("--{k}"));
        args.push(v.clone());
    }
    let out = Command::new("pvesh").args(&args).output().context("running pvesh")?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{}", err.trim());
    }
    // pvesh prints the bare result (not wrapped in {"data":..}); wrap it to match HTTP.
    let data = serde_json::from_str::<Value>(stdout.trim())
        .unwrap_or_else(|_| Value::String(stdout.trim().to_string()));
    Ok(serde_json::json!({ "data": data }))
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Create a root@pam ticket locally via PVE's Perl library (requires root + pve-cluster).
pub fn forge_root_auth() -> Result<Auth> {
    forge_root_auth_with(&crate::exec::Exec::Local)
}

/// Same, on the node reached through `exec` (SSH mode with root).
pub fn forge_root_auth_with(exec: &crate::exec::Exec) -> Result<Auth> {
    let run = |expr: &str| -> Result<String> { Ok(exec.output("perl", &["-MPVE::AccessControl", "-e", expr])?.trim().to_string()) };
    let ticket = run(r#"print PVE::AccessControl::assemble_ticket("root\@pam")"#)?;
    let csrf = run(r#"print PVE::AccessControl::assemble_csrf_prevention_token("root\@pam")"#)?;
    Ok(Auth { user: "root@pam".into(), ticket, csrf, at: Instant::now() })
}

/// Extract the node name from a UPID ("UPID:node:pid:pstart:starttime:type:id:user:").
pub fn upid_node(upid: &str) -> Option<String> {
    upid.split(':').nth(1).map(|s| s.to_string())
}

pub fn is_upid(v: &Value) -> Option<String> {
    v.as_str().filter(|s| s.starts_with("UPID:")).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upid() {
        assert_eq!(upid_node("UPID:pve1:0001:02:03:qmstart:100:root@pam:").as_deref(), Some("pve1"));
    }

    #[test]
    fn decode() {
        assert_eq!(urldecode("a%2Fb+c"), "a/b c");
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn multipart_body_layout() {
        let (h, t) = multipart_frame("B", &[("content", "iso".into())], "a\"b.iso");
        let mut body = String::from_utf8(h).unwrap();
        body.push_str("DATA");
        body.push_str(&String::from_utf8(t).unwrap());
        assert_eq!(
            body,
            "--B\r\nContent-Disposition: form-data; name=\"content\"\r\n\r\niso\r\n--B\r\nContent-Disposition: form-data; name=\"filename\"; filename=\"a_b.iso\"\r\nContent-Type: application/octet-stream\r\n\r\nDATA\r\n--B--\r\n"
        );
    }

    /// Real PVE: `sudo UP_LIVE=storage UP_NODE=node test-binary live_upload --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_upload() {
        let (Ok(storage), Ok(node)) = (std::env::var("UP_LIVE"), std::env::var("UP_NODE")) else { return };
        let api = Api::local();
        api.set_auth(forge_root_auth().expect("root ticket"));
        let dir = std::env::temp_dir().join(format!("pxup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("data.iso");
        let mut x: u64 = 0x9E3779B97F4A7C15;
        let data: Vec<u8> = (0..40 << 20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        std::fs::write(&file, &data).unwrap();
        let sha = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect::<String>();
        let path = format!("/nodes/{node}/storage/{storage}/upload");
        let wait = |upid: &str| -> String {
            loop {
                let st = api.get(&format!("/nodes/{node}/tasks/{}/status", urlenc(upid))).unwrap();
                if crate::util::js(&st, "status") == "stopped" {
                    return crate::util::js(&st, "exitstatus");
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        };
        let content = || crate::util::jarr(&api.get(&format!("/nodes/{node}/storage/{storage}/content?content=iso")).unwrap()).iter().map(|c| crate::util::js(c, "volid")).collect::<Vec<_>>();
        let vol = |n: &str| format!("{storage}:iso/{n}");

        // 1. upload with the right checksum
        let prog = Arc::new(UploadProgress::default());
        let t = Instant::now();
        let fields = [("content", "iso".to_string()), ("checksum", sha.clone()), ("checksum-algorithm", "sha256".to_string())];
        let v = api.upload(&path, &fields, &file, "pxvirt-upload-test.iso", prog.clone()).expect("upload");
        let upid = is_upid(&v).expect("UPID");
        let exit = wait(&upid);
        println!("ok upload: {} bytes in {:.1}s, task {exit}", prog.sent.load(Ordering::Relaxed), t.elapsed().as_secs_f64());
        assert_eq!(exit, "OK");
        assert_eq!(prog.sent.load(Ordering::Relaxed), data.len() as u64);
        assert!(content().contains(&vol("pxvirt-upload-test.iso")));

        // 2. wrong checksum: rejected, nothing left behind
        let bad = [("content", "iso".to_string()), ("checksum", "0".repeat(64)), ("checksum-algorithm", "sha256".to_string())];
        let r = api.upload(&path, &bad, &file, "pxvirt-upload-bad.iso", Arc::new(UploadProgress::default()));
        let res = match r {
            Ok(v) => wait(&is_upid(&v).expect("UPID")),
            Err(e) => format!("{e:#}"),
        };
        println!("ok bad checksum -> {res}");
        assert_ne!(res, "OK");
        assert!(!content().contains(&vol("pxvirt-upload-bad.iso")));

        // 3. cancel halfway
        let prog = Arc::new(UploadProgress::default());
        let p2 = prog.clone();
        std::thread::spawn(move || {
            while p2.sent.load(Ordering::Relaxed) < 8 << 20 {
                std::thread::sleep(Duration::from_millis(5));
            }
            p2.cancel.store(true, Ordering::Relaxed);
        });
        let r = api.upload(&path, &[("content", "iso".to_string())], &file, "pxvirt-upload-cancel.iso", prog.clone());
        println!("ok cancel at {} bytes -> {:?}", prog.sent.load(Ordering::Relaxed), r.as_ref().err().map(|e| e.to_string()));
        assert!(r.is_err());
        std::thread::sleep(Duration::from_secs(1));
        assert!(!content().contains(&vol("pxvirt-upload-cancel.iso")));

        // cleanup
        api.delete(&format!("/nodes/{node}/storage/{storage}/content/{}", urlenc(&vol("pxvirt-upload-test.iso"))), &[]).expect("delete");
        assert!(!content().contains(&vol("pxvirt-upload-test.iso")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_reader_counts_and_cancels() {
        let prog = Arc::new(UploadProgress::default());
        let mut r = ProgressReader { inner: std::io::Cursor::new(vec![0u8; 1000]), prog: prog.clone() };
        let mut buf = [0u8; 300];
        assert_eq!(r.read(&mut buf).unwrap(), 300);
        assert_eq!(prog.sent.load(Ordering::Relaxed), 300);
        prog.cancel.store(true, Ordering::Relaxed);
        assert!(r.read(&mut buf).is_err());
    }
}
