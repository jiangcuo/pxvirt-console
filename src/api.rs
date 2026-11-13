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
                    Err(ureq::Error::Status(code, r)) => {
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
                            bail!("{code} {reason}")
                        } else {
                            bail!("{code} {reason} ({errors})")
                        }
                    }
                    Err(e) => Err(anyhow!("connection failed: {e}")),
                }
            }
            Transport::Pvesh => pvesh(method, path, params),
        }
    }

    pub fn base_url(&self) -> String {
        format!("https://{}:{}", host_for_url(&self.host), self.port)
    }
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
