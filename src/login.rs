//! Pre-login dashboard (ESXi DCUI style) and login form.
//!
//! Login goes through POST /access/ticket (any PVE realm, TFA supported). When the API is
//! unreachable, root@pam can still log in with the local password (pwcheck) into rescue mode,
//! which talks to PVE through `pvesh`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::{Api, LoginResult};
use crate::ctx::{NodeInfo, Session};
use crate::i18n::tr;
use crate::loader::Job;
use crate::ui::{popup_block, centered, style_bad, style_good, style_label, style_sel, style_warn, ACCENT};
use crate::util::{jarr, jb, js};

const LOCK_THRESHOLD: usize = 3;
const LOCK_SECS: u64 = 60;

pub struct SessionInit {
    pub api: Arc<Api>,
    pub session: Session,
    pub nodes: Vec<NodeInfo>,
    pub cluster: String,
}

enum Step {
    Done(SessionInit),
    Tfa { user: String, challenge: String },
}

#[derive(PartialEq)]
enum Stage {
    Creds,
    Tfa { user: String, challenge: String },
    Trust { fp: String },
}

pub struct Login {
    api: Arc<Api>,
    remote: bool,
    pub user: String,
    realms: Vec<(String, String)>,
    realm: usize,
    password: String,
    otp: String,
    field: usize,
    stage: Stage,
    job: Option<Job<Step>>,
    realms_job: Option<Job<Vec<(String, String)>>>,
    api_ok: Option<bool>,
    pub message: Option<(String, bool)>,
    attempts: usize,
    locked_until: Option<Instant>,
    last_realm_check: Instant,
}

pub enum LoginEvent {
    Back,
    Trusted(String),
}

impl Login {
    pub fn new(api: Arc<Api>, remote: bool, default_realm: &str, trust_fp: Option<String>) -> Self {
        let mut l = Self {
            api,
            remote,
            user: "root".into(),
            realms: vec![(default_realm.to_string(), String::new())],
            realm: 0,
            password: String::new(),
            otp: String::new(),
            field: 2,
            stage: Stage::Creds,
            job: None,
            realms_job: None,
            api_ok: None,
            message: None,
            attempts: 0,
            locked_until: None,
            last_realm_check: Instant::now() - Duration::from_secs(60),
        };
        if let Some(fp) = trust_fp {
            l.stage = Stage::Trust { fp };
        }
        l
    }

    pub fn set_api(&mut self, api: Arc<Api>) {
        self.api = api;
        self.api_ok = None;
        self.last_realm_check = Instant::now() - Duration::from_secs(60);
    }

    pub fn reset(&mut self) {
        scrub(&mut self.password);
        scrub(&mut self.otp);
        self.stage = Stage::Creds;
        self.field = 2;
        self.job = None;
        self.message = None;
    }

    fn realm_name(&self) -> String {
        self.realms.get(self.realm).map(|r| r.0.clone()).unwrap_or_else(|| "pam".into())
    }

    fn lock_remaining(&self) -> Option<u64> {
        self.locked_until.and_then(|t| t.checked_duration_since(Instant::now())).map(|d| d.as_secs().max(1))
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Poll background jobs; returns a session when login completed.
    pub fn tick(&mut self) -> Option<SessionInit> {
        if self.realms_job.is_none() && self.last_realm_check.elapsed() > Duration::from_secs(15) && !matches!(self.stage, Stage::Trust { .. }) {
            self.last_realm_check = Instant::now();
            let api = self.api.clone();
            self.realms_job = Some(Job::spawn(move || api.domains()));
        }
        if let Some(j) = &self.realms_job {
            if let Some(r) = j.poll() {
                self.realms_job = None;
                match r {
                    Ok(list) if !list.is_empty() => {
                        let cur = self.realm_name();
                        self.realms = list;
                        self.realm = self.realms.iter().position(|r| r.0 == cur).unwrap_or(0);
                        self.api_ok = Some(true);
                    }
                    Ok(_) => self.api_ok = Some(true),
                    Err(_) => self.api_ok = Some(false),
                }
            }
        }
        let job = self.job.as_ref()?;
        let r = job.poll()?;
        self.job = None;
        match r {
            Ok(Step::Done(init)) => {
                self.attempts = 0;
                self.reset();
                return Some(init);
            }
            Ok(Step::Tfa { user, challenge }) => {
                self.stage = Stage::Tfa { user, challenge };
                self.message = Some((tr("Enter the second factor (TOTP code or recovery:CODE)").to_string(), false));
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let msg = if msg.contains("401") { tr("Invalid user name or password").to_string() } else { msg };
                if matches!(self.stage, Stage::Tfa { .. }) {
                    self.stage = Stage::Creds;
                }
                self.attempts += 1;
                if self.attempts >= LOCK_THRESHOLD {
                    self.locked_until = Some(Instant::now() + Duration::from_secs(LOCK_SECS));
                    self.attempts = 0;
                    self.message = Some((format!("{msg}. Login locked for {LOCK_SECS}s"), true));
                } else {
                    self.message = Some((format!("{msg} ({}/{LOCK_THRESHOLD})", self.attempts), true));
                }
            }
        }
        None
    }

    fn submit(&mut self) {
        if self.job.is_some() {
            return;
        }
        if let Some(r) = self.lock_remaining() {
            self.message = Some((format!("Too many failed attempts. Try again in {r}s"), true));
            return;
        }
        let api = self.api.clone();
        let remote = self.remote;
        match &self.stage {
            Stage::Trust { .. } => {}
            Stage::Tfa { user, challenge } => {
                let (user, challenge) = (user.clone(), challenge.clone());
                let mut code = std::mem::take(&mut self.otp);
                self.job = Some(Job::spawn(move || {
                    let r = api.login_tfa(&user, &challenge, code.trim());
                    scrub(&mut code);
                    let auth = r?;
                    api.set_auth(auth);
                    Ok(Step::Done(init_session(api, false)?))
                }));
                self.message = Some((tr("Logging in...").to_string(), false));
            }
            Stage::Creds => {
                if self.password.is_empty() {
                    self.message = Some((tr("Password cannot be empty").to_string(), true));
                    return;
                }
                let (mut user, mut realm) = (self.user.trim().to_string(), self.realm_name());
                if let Some((u, r)) = user.clone().split_once('@') {
                    user = u.to_string();
                    realm = r.to_string();
                }
                let mut password = std::mem::take(&mut self.password);
                self.job = Some(Job::spawn(move || {
                    let r = do_login(api, remote, &user, &realm, &password);
                    scrub(&mut password);
                    r
                }));
                self.message = Some((tr("Logging in...").to_string(), false));
            }
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> Option<LoginEvent> {
        if let Stage::Trust { fp } = &self.stage {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let fp = fp.clone();
                    self.stage = Stage::Creds;
                    return Some(LoginEvent::Trusted(fp));
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return Some(LoginEvent::Back),
                _ => return None,
            }
        }
        if self.job.is_some() {
            return None;
        }
        let tfa = matches!(self.stage, Stage::Tfa { .. });
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            match (tfa, self.field) {
                (true, _) => scrub(&mut self.otp),
                (_, 0) => self.user.clear(),
                (_, 2) => scrub(&mut self.password),
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                if tfa {
                    self.reset();
                } else {
                    self.reset();
                    return Some(LoginEvent::Back);
                }
            }
            KeyCode::Up | KeyCode::BackTab if !tfa => self.field = (self.field + 2) % 3,
            KeyCode::Down | KeyCode::Tab if !tfa => self.field = (self.field + 1) % 3,
            KeyCode::Left | KeyCode::Right if !tfa && self.field == 1 && !self.realms.is_empty() => {
                let n = self.realms.len();
                self.realm = if key.code == KeyCode::Left { (self.realm + n - 1) % n } else { (self.realm + 1) % n };
            }
            KeyCode::Enter => {
                if !tfa && self.field < 2 {
                    self.field += 1;
                } else {
                    self.submit();
                }
            }
            KeyCode::Backspace => {
                if tfa {
                    self.otp.pop();
                } else if self.field == 0 {
                    self.user.pop();
                } else if self.field == 2 {
                    self.password.pop();
                }
            }
            KeyCode::Char(c) => {
                if tfa {
                    self.otp.push(c);
                } else if self.field == 0 {
                    self.user.push(c);
                } else if self.field == 2 {
                    self.password.push(c);
                } else if c == ' ' && !self.realms.is_empty() {
                    self.realm = (self.realm + 1) % self.realms.len();
                }
            }
            _ => {}
        }
        None
    }

    pub fn draw(&self, f: &mut Frame, area: Rect, host: &str) {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(format!("  {host}"), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))));
        lines.push(Line::from(""));
        if let Stage::Trust { fp } = &self.stage {
            lines.push(Line::from(Span::styled(tr("Unknown server certificate. SHA-256 fingerprint:"), style_warn())));
            lines.push(Line::from(fp.clone()));
            lines.push(Line::from(""));
            lines.push(Line::from(tr("Trust this certificate? [y/N]")));
        } else {
            let tfa = matches!(self.stage, Stage::Tfa { .. });
            let field = |idx: usize, label: &str, value: String| -> Line {
                let focused = !tfa && self.field == idx;
                Line::from(vec![
                    Span::styled(format!("  {} ", crate::ui::pad(tr(label), 10)), if focused { Style::default().fg(ACCENT) } else { style_label() }),
                    Span::styled(format!("{value}{}", if focused && idx != 1 { "_" } else { "" }), if focused { style_sel(true) } else { Style::default() }),
                ])
            };
            lines.push(field(0, "User", self.user.clone()));
            let realm_label = self
                .realms
                .get(self.realm)
                .map(|(r, c)| if c.is_empty() { r.clone() } else { format!("{r} ({})", crate::util::truncate(c, 30)) })
                .unwrap_or_default();
            lines.push(field(1, "Realm", format!("< {realm_label} >")));
            lines.push(field(2, "Password", "*".repeat(self.password.chars().count())));
            if tfa {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:<10} ", "TFA"), Style::default().fg(ACCENT)),
                    Span::styled(format!("{}_", self.otp), style_sel(true)),
                ]));
            }
            lines.push(Line::from(""));
            match self.api_ok {
                Some(false) if !self.remote => lines.push(Line::from(Span::styled(
                    tr("PVE API unreachable: root@pam can log in to rescue mode"),
                    style_warn(),
                ))),
                Some(false) => lines.push(Line::from(Span::styled(tr("PVE API unreachable"), style_bad()))),
                _ => {}
            }
        }
        if let Some((m, err)) = &self.message {
            lines.push(Line::from(Span::styled(m.clone(), if *err { style_bad() } else { style_good() })));
        }
        let r = centered(60, lines.len() as u16 + 2, area);
        crate::ui::clear(f, r);
        f.render_widget(Paragraph::new(lines).block(popup_block(&format!("{} {}", tr("Log in"), host_tag(self.remote)))), r);
    }
}

fn host_tag(remote: bool) -> &'static str {
    if remote {
        "(remote)"
    } else {
        ""
    }
}

pub fn scrub(s: &mut String) {
    unsafe {
        for b in s.as_bytes_mut() {
            std::ptr::write_volatile(b, 0);
        }
    }
    s.clear();
}

fn do_login(api: Arc<Api>, remote: bool, user: &str, realm: &str, password: &str) -> anyhow::Result<Step> {
    match api.domains() {
        Ok(_) => match api.login(&format!("{user}@{realm}"), password)? {
            LoginResult::Ok(auth) => {
                api.set_auth(auth);
                Ok(Step::Done(init_session(api, false)?))
            }
            LoginResult::NeedTfa { user, challenge } => Ok(Step::Tfa { user, challenge }),
        },
        Err(e) => {
            if !remote && user == "root" && realm == "pam" && crate::util::is_root_process() {
                if !matches!(pwcheck::pwcheck("root", password), pwcheck::PwcheckResult::Ok) {
                    anyhow::bail!("Invalid password");
                }
                let api = Arc::new(Api::pvesh());
                Ok(Step::Done(init_session(api, true)?))
            } else {
                anyhow::bail!("PVE API unreachable: {e:#}")
            }
        }
    }
}

/// Load permissions and cluster nodes for a fresh session.
pub fn init_session(api: Arc<Api>, rescue: bool) -> anyhow::Result<SessionInit> {
    let user = if rescue { "root@pam".to_string() } else { api.auth().map(|a| a.user).unwrap_or_default() };
    let mut perms: HashMap<String, HashSet<String>> = HashMap::new();
    if !rescue && user != "root@pam" {
        if let Ok(serde_json::Value::Object(m)) = api.get("/access/permissions") {
            for (path, privs) in m {
                if let serde_json::Value::Object(p) = privs {
                    perms.insert(path, p.keys().cloned().collect());
                }
            }
        }
    }
    let (nodes, cluster) = load_nodes(&api);
    Ok(SessionInit { api, session: Session { user, perms, rescue }, nodes, cluster })
}

pub fn load_nodes(api: &Api) -> (Vec<NodeInfo>, String) {
    let mut nodes = Vec::new();
    let mut cluster = String::new();
    if let Ok(v) = api.get("/cluster/status") {
        for e in jarr(&v) {
            match js(&e, "type").as_str() {
                "cluster" => cluster = js(&e, "name"),
                "node" => nodes.push(NodeInfo {
                    name: js(&e, "name"),
                    ip: js(&e, "ip"),
                    online: jb(&e, "online"),
                    local: jb(&e, "local"),
                }),
                _ => {}
            }
        }
    }
    if nodes.is_empty() {
        if let Ok(v) = api.get("/nodes") {
            for e in jarr(&v) {
                nodes.push(NodeInfo { name: js(&e, "node"), ip: String::new(), online: js(&e, "status") == "online", local: false });
            }
        }
    }
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    (nodes, cluster)
}

// ---------------------------------------------------------------------------
// Pre-login dashboard
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct HostInfo {
    pub hostname: String,
    pub pve_version: String,
    pub kernel: String,
    pub cpu_model: String,
    pub addrs: Vec<(String, String)>,
    pub arch: String,
}

pub fn host_info() -> HostInfo {
    let pve_version = std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", "pve-manager"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let cpuinfo = crate::util::read_file("/proc/cpuinfo");
    let cpu_model = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name") || l.starts_with("Model Name") || l.starts_with("cpu model"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    let arch = if unsafe { libc::uname(&mut uts) } == 0 {
        unsafe { std::ffi::CStr::from_ptr(uts.machine.as_ptr()) }.to_string_lossy().to_string()
    } else {
        String::new()
    };
    HostInfo {
        hostname: crate::util::local_hostname(),
        pve_version,
        kernel: crate::util::read_file("/proc/sys/kernel/osrelease").trim().to_string(),
        cpu_model,
        addrs: crate::util::local_addresses(),
        arch,
    }
}

pub fn draw_dashboard(f: &mut Frame, area: Rect, info: &HostInfo, m: &crate::metrics::MetricsData) {
    let dim = Style::default().fg(ratatui::style::Color::DarkGray);
    let lw = 12;
    let kv = |k: &str, v: String| Line::from(vec![Span::styled(crate::ui::pad(tr(k), lw), dim), Span::raw(v)]);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("● PXVIRT", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!("   {}", info.pve_version), dim),
    ]));
    lines.push(Line::from(Span::styled(info.hostname.clone(), Style::default().add_modifier(Modifier::BOLD))));
    lines.push(Line::from(""));
    // IPv4 first, then at most one IPv6; skip the rest to keep the screen calm
    let mut urls: Vec<(String, String)> = Vec::new();
    for (iface, addr) in info.addrs.iter().filter(|(_, a)| !a.contains(':')) {
        let ip = addr.split('/').next().unwrap_or("");
        urls.push((format!("https://{ip}:8006"), iface.clone()));
    }
    if let Some((iface, addr)) = info.addrs.iter().find(|(_, a)| a.contains(':')) {
        let ip = addr.split('/').next().unwrap_or("");
        urls.push((format!("https://[{ip}]:8006"), iface.clone()));
    }
    if urls.is_empty() {
        lines.push(Line::from(vec![Span::styled(crate::ui::pad(tr("Management URL"), lw), dim), Span::styled(tr("no IP address configured"), style_bad())]));
    }
    for (i, (url, iface)) in urls.iter().take(4).enumerate() {
        lines.push(Line::from(vec![
            Span::styled(crate::ui::pad(if i == 0 { tr("Management URL") } else { "" }, lw), dim),
            Span::styled(url.clone(), Style::default().fg(ACCENT)),
            Span::styled(format!("  {iface}"), dim),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(kv("CPU", format!("{} · {} {} · {}", info.cpu_model, m.cpus, tr("threads"), info.arch)));
    lines.push(kv("Memory", format!("{} / {}", crate::util::fmt_bytes(m.mem_used), crate::util::fmt_bytes(m.mem_total))));
    lines.push(kv("Kernel", info.kernel.clone()));
    lines.push(kv("Uptime", crate::util::fmt_duration(m.uptime)));
    lines.push(Line::from(""));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled("Enter ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::styled(tr("Log in"), Style::default().fg(ratatui::style::Color::Gray))]));
    let h = lines.len() as u16;
    let w = lines.iter().map(|l| l.width()).max().unwrap_or(40) as u16;
    let r = centered(w.max(50).min(area.width.saturating_sub(4)), h, area);
    f.render_widget(Paragraph::new(lines), r);
}
