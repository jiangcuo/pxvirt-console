//! Application state, key routing, layout and the main frame.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::{self, Api};
use crate::config::Config;
use crate::ctx::{ActionOutcome, Cmd, Ctx, Session, Suspend, VncRequest};
use crate::i18n::{self, tr};
use crate::loader::{Job, Loader};
use crate::login::{self, HostInfo, Login, LoginEvent, SessionInit};
use crate::metrics::{Metrics, MetricsData, Target};
use crate::modules::{build_modules, Module};
use crate::ui::popup::{item, MenuItem, Outcome, Popup};
use crate::ui::task::TaskView;
use crate::ui::{self, hint_spans, hsplit, sparkline, style_bad, usage_style, vsplit, ACCENT};
use crate::util::{fmt_bytes, fmt_rate, jarr, jf, js};

pub const APP_VERSION: &str = env!("APP_VERSION");

#[derive(Clone, Copy, PartialEq, Debug)]
enum Focus {
    List,
    Detail,
}

#[derive(Clone, Copy, PartialEq)]
enum Origin {
    View(usize, usize),
    Global,
}

#[derive(PartialEq)]
enum Screen {
    Dashboard,
    Login,
    Main,
}

pub struct RemoteTarget {
    pub host: String,
    pub port: u16,
}

/// SSH mode connection (established before the TUI starts).
pub struct SshTarget {
    pub target: String,
    pub label: String,
    pub node: String,
    pub api_port: u16,
    pub user: String,
}

/// What a mouse click on the main screen lands on.
#[derive(Clone, Copy, Debug)]
enum Hit {
    Tab(usize),
    Sub(usize),
    Action(char),
    More,
    List,
    Detail,
}

pub struct App {
    pub ctx: Ctx,
    modules: Vec<Module>,
    cur: usize,
    focus: Focus,
    popups: Vec<(Popup, Origin)>,
    filter_edit: bool,
    toast: Option<(String, bool, Instant)>,
    actions: Vec<(String, Job<ActionOutcome>, Origin)>,
    bg_tasks: Vec<(TaskView, Origin)>,
    pub metrics: Metrics,
    last_refresh: HashMap<(usize, usize), Instant>,
    auto_refresh: bool,
    last_activity: Instant,
    pub should_exit: bool,
    pub pending_suspend: Option<Suspend>,
    pub pending_vnc: Option<VncRequest>,
    screen: Screen,
    login: Login,
    host: HostInfo,
    host_at: Instant,
    cluster: String,
    remote: Option<RemoteTarget>,
    alerts: Loader<Vec<String>>,
    renew: Option<Job<()>>,
    last_renew_check: Instant,
    recover: Option<Job<Arc<Api>>>,
    last_recover: Instant,
    narrow_detail: bool,
    width: u16,
    arch_job: Option<Job<(String, String)>>,
    /// Quitting is allowed in ssh / terminal emulators, not on tty1 or a serial console.
    pub can_quit: bool,
    /// "Quit pxvirt-console?" is showing (works on every screen).
    confirm_quit: bool,
    /// Clickable areas of the last frame.
    hits: Vec<(Rect, Hit)>,
    last_click: Option<(Instant, u16, u16)>,
    ssh: Option<SshTarget>,
    /// logged in automatically (root in a terminal or over SSH): logging out quits
    auto_session: bool,
    /// how the session was started, for the audit log ("terminal, sudo by x", "ssh root@y")
    login_origin: String,
}

impl App {
    pub fn new(cfg: Config, remote: Option<RemoteTarget>, ssh: Option<SshTarget>) -> Self {
        i18n::init(&cfg.lang);
        let mut trust = None;
        let api = match (&remote, &ssh) {
            // the API is reached through the ssh port forward; trust comes from the ssh host key
            (_, Some(t)) => Arc::new(Api::http("127.0.0.1", t.api_port, None)),
            (Some(r), None) => {
                let fp = api::known_fingerprint(&r.host);
                if fp.is_none() {
                    trust = api::probe_fingerprint(&r.host, r.port).ok();
                }
                Arc::new(Api::http(&r.host, r.port, fp))
            }
            (None, None) => Arc::new(Api::local()),
        };
        let local_mode = remote.is_none() && ssh.is_none();
        let local_node = match (&remote, &ssh) {
            (_, Some(t)) => Some(t.node.clone()),
            (Some(_), None) => None,
            (None, None) => Some(crate::util::local_hostname()),
        };
        let metrics = Metrics::new();
        let ctx = Ctx {
            metrics: metrics.data.clone(),
            api: api.clone(),
            session: Session::default(),
            node: local_node.clone().unwrap_or_default(),
            nodes: Vec::new(),
            local_node,
            remote: remote.is_some() && ssh.is_none(),
            ssh: ssh.as_ref().map(|t| t.target.clone()),
            cmds: Vec::new(),
            node_arch: HashMap::new(),
            cfg: cfg.clone(),
        };
        let login = Login::new(api, !local_mode, &cfg.default_realm, trust);
        let screen = if local_mode && cfg.prelogin_dashboard {
            metrics.start(Target::Local);
            Screen::Dashboard
        } else {
            Screen::Login
        };
        Self {
            ctx,
            modules: build_modules(),
            cur: 0,
            focus: Focus::List,
            popups: Vec::new(),
            filter_edit: false,
            toast: None,
            actions: Vec::new(),
            bg_tasks: Vec::new(),
            metrics,
            last_refresh: HashMap::new(),
            auto_refresh: true,
            last_activity: Instant::now(),
            should_exit: false,
            pending_suspend: None,
            pending_vnc: None,
            screen,
            login,
            host: if local_mode { login::host_info() } else { HostInfo::default() },
            host_at: Instant::now(),
            cluster: String::new(),
            remote,
            alerts: Loader::default(),
            renew: None,
            last_renew_check: Instant::now(),
            recover: None,
            last_recover: Instant::now(),
            narrow_detail: false,
            width: 120,
            arch_job: None,
            can_quit: !crate::util::on_system_console(),
            confirm_quit: false,
            hits: Vec::new(),
            last_click: None,
            ssh,
            auto_session: false,
            login_origin: String::new(),
        }
    }

    /// SSH mode: connected as root -> create a root@pam ticket on the node, skip the login form.
    /// The operator was already authenticated by the OS: root over SSH, or root in a terminal
    /// on the node (sudo). Log in as root@pam without asking. The system console (tty1, serial)
    /// and API-only mode always use the login screen.
    pub fn auto_login(&mut self) -> anyhow::Result<()> {
        if let Some((user, target)) = self.ssh.as_ref().map(|t| (t.user.clone(), t.target.clone())) {
            if user != "root" {
                return Ok(());
            }
            let exec = crate::exec::Exec::Ssh(target.clone());
            let auth = api::forge_root_auth_with(&exec)?;
            self.ctx.api.set_auth(auth);
            let init = login::init_session(self.ctx.api.clone(), false)?;
            self.auto_session = true;
            self.login_origin = format!("ssh {target}");
            self.start_session(init);
            return Ok(());
        }
        if self.remote.is_some() || !crate::util::is_root_process() || crate::util::on_system_console() {
            return Ok(());
        }
        let who = std::env::var("SUDO_USER").map(|u| format!("terminal, sudo by {u}")).unwrap_or_else(|_| "terminal".into());
        // PVE API: a locally signed ticket; API down: rescue mode through pvesh
        let init = match api::forge_root_auth() {
            Ok(auth) => {
                self.ctx.api.set_auth(auth);
                login::init_session(self.ctx.api.clone(), false)
            }
            Err(e) => Err(e),
        };
        let init = match init {
            Ok(i) => i,
            Err(_) => {
                let api = Arc::new(Api::pvesh());
                self.ctx.api = api.clone();
                login::init_session(api, true)?
            }
        };
        self.auto_session = true;
        self.login_origin = who;
        self.start_session(init);
        Ok(())
    }

    /// Debug helper: log in as root@pam with a locally created ticket (root only).
    pub fn debug_login_root(&mut self) -> anyhow::Result<()> {
        let api = Arc::new(Api::local());
        api.set_auth(api::forge_root_auth()?);
        let init = login::init_session(api, false)?;
        self.start_session(init);
        Ok(())
    }

    /// Debug helper: simulate mouse clicks on the recorded areas and report what happened.
    pub fn debug_mouse_selftest(&mut self) -> String {
        use crossterm::event::{KeyModifiers as KM, MouseButton as MB, MouseEvent as ME, MouseEventKind as MK};
        let ev = |kind: MK, x: u16, y: u16| ME { kind, column: x, row: y, modifiers: KM::NONE };
        let mut out = String::new();
        let frame = |app: &mut App| {
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 40)).expect("test backend");
            for _ in 0..30 {
                app.tick();
                let _ = t.draw(|f| app.draw(f));
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        frame(self);
        let find = |app: &App, pred: &dyn Fn(&Hit) -> bool| app.hits.iter().find(|(_, h)| pred(h)).map(|(r, _)| (r.x + 1, r.y));
        // module tab F3
        if let Some((x, y)) = find(self, &|h| matches!(h, Hit::Tab(2))) {
            self.on_mouse(ev(MK::Down(MB::Left), x, y));
            out.push_str(&format!("click tab F3 -> module {}\n", self.cur + 1));
        }
        frame(self);
        // submenu 2 of that module
        if let Some((x, y)) = find(self, &|h| matches!(h, Hit::Sub(1))) {
            self.on_mouse(ev(MK::Down(MB::Left), x, y));
            out.push_str(&format!("click submenu 2 -> view {}\n", self.modules[self.cur].sel + 1));
        }
        // System > Processes: click the 3rd row, then right click it
        self.goto(0, 2);
        frame(self);
        let before = self.modules[0].views[2].table().and_then(|t| t.selected().map(|s| s.to_string()));
        if let Some((r, _)) = self.hits.iter().find(|(_, h)| matches!(h, Hit::List)).cloned() {
            let (x, y) = (r.x + 4, r.y + 4);
            self.on_mouse(ev(MK::Down(MB::Left), x, y));
            let after = self.modules[0].views[2].table().and_then(|t| t.selected().map(|s| s.to_string()));
            out.push_str(&format!("click process row 3 -> selection {before:?} -> {after:?}\n"));
            self.on_mouse(ev(MK::Down(MB::Right), x, y));
            out.push_str(&format!("right click row -> popups open: {}\n", self.popups.len()));
            frame(self);
            self.on_mouse(ev(MK::Down(MB::Left), 0, 0));
            out.push_str(&format!("click outside menu -> popups open: {}\n", self.popups.len()));
            self.on_mouse(ev(MK::ScrollDown, x, y));
            let scrolled = self.modules[0].views[2].table().and_then(|t| t.selected().map(|s| s.to_string()));
            out.push_str(&format!("wheel down -> selection {scrolled:?}\n"));
        }
        frame(self);
        let chips: Vec<String> = self.hits.iter().filter_map(|(_, h)| if let Hit::Action(c) = h { Some(c.to_string()) } else { None }).collect();
        out.push_str(&format!("action bar chips: {}\n", chips.join(" ")));
        out
    }

    /// Debug helper: render the current screen as text.
    pub fn debug_render(&mut self, w: u16, h: u16) -> String {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test backend");
        let _ = t.draw(|f| self.draw(f));
        let buf = t.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Debug helper: render the current screen as HTML (with colors) for visual review.
    pub fn debug_render_html(&mut self, w: u16, h: u16) -> String {
        use ratatui::style::Color as C;
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("test backend");
        let _ = t.draw(|f| self.draw(f));
        let buf = t.backend().buffer().clone();
        let col = |c: C, fg: bool| -> &'static str {
            match c {
                C::Black => "#000000",
                C::Red => "#cd3131",
                C::Green => "#0dbc79",
                C::Yellow => "#e5e510",
                C::Blue => "#2472c8",
                C::Magenta => "#bc3fbc",
                C::Cyan => "#11a8cd",
                C::Gray => "#cccccc",
                C::DarkGray => "#767676",
                C::White => "#ffffff",
                _ => if fg { "#222222" } else { "#ffffff" },
            }
        };
        let mut out = String::from("<html><head><meta charset='utf-8'></head><body style='margin:0;background:#fff'><pre style='font:13px/1.25 \"Noto Sans Mono CJK SC\",monospace;margin:8px'>");
        for y in 0..h {
            let mut skip = 0;
            for x in 0..w {
                let c = &buf[(x, y)];
                let sym = c.symbol();
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if sym.is_empty() {
                    continue;
                }
                skip = ui::unicode_width(sym).saturating_sub(1);
                let bold = if c.modifier.contains(ratatui::style::Modifier::BOLD) { "font-weight:bold;" } else { "" };
                let ul = if c.modifier.contains(ratatui::style::Modifier::UNDERLINED) { "text-decoration:underline;" } else { "" };
                let esc = sym.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
                out.push_str(&format!("<span style='color:{};background:{};{bold}{ul}'>{esc}</span>", col(c.fg, true), col(c.bg, false)));
            }
            out.push('\n');
        }
        out.push_str("</pre></body></html>");
        out
    }

    /// Debug helper: render every view into text files (sizes WxH).
    pub fn debug_dump(&mut self, dir: &str, sizes: &[(u16, u16)]) -> anyhow::Result<()> {
        use ratatui::backend::TestBackend;
        std::fs::create_dir_all(dir)?;
        let dump = |app: &mut App, name: &str| -> anyhow::Result<()> {
            for (w, h) in sizes {
                let mut t = ratatui::Terminal::new(TestBackend::new(*w, *h))?;
                t.draw(|f| app.draw(f))?;
                let buf = t.backend().buffer().clone();
                let mut out = String::new();
                for y in 0..*h {
                    for x in 0..*w {
                        out.push_str(buf[(x, y)].symbol());
                    }
                    out.push('\n');
                }
                std::fs::write(format!("{dir}/{name}_{w}x{h}.txt"), out)?;
                if *w == 110 {
                    std::fs::write(format!("{dir}/{name}.html"), app.debug_render_html(*w, *h))?;
                }
            }
            Ok(())
        };
        let settle = |app: &mut App, ms: u64| {
            let end = Instant::now() + Duration::from_millis(ms);
            let mut t = ratatui::Terminal::new(TestBackend::new(160, 45)).expect("test backend");
            while Instant::now() < end {
                app.tick();
                let _ = t.draw(|f| app.draw(f));
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        for m in 0..self.modules.len() {
            for v in 0..self.modules[m].views.len() {
                self.goto(m, v);
                // the RAID scan runs many vendor commands
                settle(self, if self.modules[m].views[v].title() == "Hardware RAID" { 20000 } else { 3000 });
                let name = format!("{}_{}_{}", m + 1, v + 1, self.modules[m].views[v].title().replace([' ', '/', '&'], ""));
                dump(self, &name)?;
                // context menu
                self.open_context_menu();
                if !self.popups.is_empty() {
                    dump(self, &format!("{name}_menu"))?;
                    self.popups.clear();
                }
                // upload file browser of the first storage
                if self.modules[m].views[v].title() == "Storage" && self.modules[m].views[v].table().map(|t| t.selected().is_some()).unwrap_or(false) {
                    use crossterm::event::{KeyCode, KeyEvent};
                    self.on_key(KeyEvent::from(KeyCode::Char('U')));
                    settle(self, 300);
                    dump(self, &format!("{name}_upload"))?;
                    self.open_context_menu();
                    dump(self, &format!("{name}_upload_menu"))?;
                    self.popups.clear();
                    self.on_key(KeyEvent::from(KeyCode::Esc));
                    settle(self, 300);
                }
                // hardware RAID: menus of a controller, a virtual drive and a physical drive
                if self.modules[m].views[v].title() == "Hardware RAID" {
                    use crossterm::event::{KeyCode, KeyEvent};
                    for (i, steps) in [0usize, 1, 4].iter().enumerate() {
                        for _ in 0..*steps {
                            self.on_key(KeyEvent::from(KeyCode::Down));
                        }
                        settle(self, 200);
                        dump(self, &format!("{name}_sel{i}"))?;
                        self.open_context_menu();
                        dump(self, &format!("{name}_sel{i}_menu"))?;
                        self.popups.clear();
                    }
                }
                // guest configuration page of the first guest: page, menu, add menu, first edit form
                if self.modules[m].views[v].title() == "Guests" && self.modules[m].views[v].table().map(|t| t.selected().is_some()).unwrap_or(false) {
                    use crossterm::event::{KeyCode, KeyEvent};
                    self.on_key(KeyEvent::from(KeyCode::Char('e')));
                    settle(self, 3000);
                    dump(self, &format!("{name}_config"))?;
                    self.open_context_menu();
                    dump(self, &format!("{name}_config_menu"))?;
                    self.popups.clear();
                    self.on_key(KeyEvent::from(KeyCode::Char('+')));
                    settle(self, 300);
                    dump(self, &format!("{name}_config_add"))?;
                    self.popups.clear();
                    for (i, k) in ['e', 'e'].iter().enumerate() {
                        if i == 1 {
                            self.on_key(KeyEvent::from(KeyCode::Down));
                        }
                        self.on_key(KeyEvent::from(KeyCode::Char(*k)));
                        settle(self, 300);
                        dump(self, &format!("{name}_config_edit{i}"))?;
                        self.popups.clear();
                    }
                    self.on_key(KeyEvent::from(KeyCode::Esc));
                    settle(self, 300);
                }
                // create VM: node picker (clusters) or the form
                if self.modules[m].views[v].title() == "Guests" {
                    use crossterm::event::{KeyCode, KeyEvent};
                    self.on_key(KeyEvent::from(KeyCode::Char('N')));
                    settle(self, 3000);
                    dump(self, &format!("{name}_create"))?;
                    if self.popups.last().map(|p| matches!(p.0, Popup::Menu(_))).unwrap_or(false) {
                        self.on_key(KeyEvent::from(KeyCode::Char('1')));
                        settle(self, 4000);
                        dump(self, &format!("{name}_create_form"))?;
                    }
                    self.popups.clear();
                }
            }
        }
        self.open_global_menu();
        dump(self, "global_menu")?;
        self.popups.clear();
        self.open_help();
        dump(self, "help")?;
        self.popups.clear();
        Ok(())
    }

    fn host_label(&self) -> String {
        if let Some(t) = &self.ssh {
            return t.node.clone();
        }
        match &self.remote {
            Some(r) => r.host.clone(),
            None => self.host.hostname.clone(),
        }
    }

    // -----------------------------------------------------------------------
    // session lifecycle
    // -----------------------------------------------------------------------

    fn start_session(&mut self, init: SessionInit) {
        self.ctx.api = init.api.clone();
        self.ctx.session = init.session;
        self.ctx.nodes = init.nodes;
        self.cluster = init.cluster;
        if self.ctx.local_node.is_none() || self.ctx.node_info(self.ctx.local_node.as_deref().unwrap_or("")).is_none() {
            // remote mode, or this host is not (yet) a PVE node name we know
            if let Some(n) = self.ctx.nodes.iter().find(|n| n.local).or_else(|| self.ctx.nodes.first()) {
                self.ctx.node = n.name.clone();
            }
        } else {
            self.ctx.node = self.ctx.local_node.clone().unwrap_or_default();
        }
        self.modules = build_modules();
        if self.ctx.remote {
            for m in &mut self.modules {
                m.views.retain(|v| !v.needs_host());
            }
        }
        self.cur = 0;
        self.focus = Focus::List;
        self.popups.clear();
        self.screen = Screen::Main;
        self.last_refresh.clear();
        self.last_activity = Instant::now();
        self.start_metrics();
        self.fetch_arch();
        self.alerts.clear();
        let mut what = String::from("login");
        if self.ctx.session.rescue {
            what.push_str(" (rescue mode)");
        }
        if !self.login_origin.is_empty() {
            what.push_str(&format!(" ({}, automatic)", self.login_origin));
        }
        crate::audit(&self.ctx.session.user, &self.ctx.node, &what);
        if self.ctx.session.rescue {
            self.toast(tr("RESCUE MODE: PVE API unreachable, local root access only").to_string(), true);
        }
        self.refresh_current(true);
    }

    /// Learn the architecture of the viewed node (shown in the header).
    fn fetch_arch(&mut self) {
        if self.ctx.node.is_empty() || self.ctx.node_arch.contains_key(&self.ctx.node) {
            return;
        }
        let api = self.ctx.api.clone();
        let node = self.ctx.node.clone();
        self.arch_job = Some(Job::spawn(move || {
            let s = api.get(&format!("/nodes/{node}/status"))?;
            let a = s.get("current-kernel").map(|k| js(k, "machine")).unwrap_or_default();
            Ok((node, crate::modules::pve::arch_norm(&a)))
        }));
    }

    fn start_metrics(&self) {
        if self.ssh.is_some() {
            match self.ctx.exec() {
                Some(e) => self.metrics.start(Target::Exec(e)),
                None => self.metrics.start(Target::Api(self.ctx.api.clone(), self.ctx.node.clone())),
            }
        } else if self.ctx.is_local_node() {
            self.metrics.start(Target::Local);
        } else if self.ctx.node.is_empty() {
            self.metrics.start(Target::None);
        } else {
            self.metrics.start(Target::Api(self.ctx.api.clone(), self.ctx.node.clone()));
        }
    }

    pub fn logout(&mut self) {
        if self.screen == Screen::Main {
            crate::audit(&self.ctx.session.user, &self.ctx.node, "logout");
        }
        if self.ssh.is_some() || self.auto_session {
            // automatic sessions have no login screen to go back to: logging out = quitting
            self.should_exit = true;
            return;
        }
        self.ctx.api.clear_auth();
        self.ctx.session = Session::default();
        self.modules = build_modules();
        self.popups.clear();
        self.actions.clear();
        self.bg_tasks.clear();
        self.filter_edit = false;
        self.login.reset();
        // back to the local HTTP API (rescue sessions used pvesh)
        if self.remote.is_none() {
            let api = Arc::new(Api::local());
            self.ctx.api = api.clone();
            self.login.set_api(api);
            self.ctx.node = self.ctx.local_node.clone().unwrap_or_default();
        }
        if self.remote.is_none() && self.ctx.cfg.prelogin_dashboard {
            self.metrics.start(Target::Local);
            self.screen = Screen::Dashboard;
        } else {
            self.metrics.start(Target::None);
            self.screen = Screen::Login;
        }
    }

    fn switch_node(&mut self, node: String) {
        if node == self.ctx.node {
            return;
        }
        self.ctx.node = node;
        for m in &mut self.modules {
            for v in &mut m.views {
                v.reset();
            }
        }
        self.last_refresh.clear();
        self.start_metrics();
        self.fetch_arch();
        self.alerts.clear();
        self.refresh_current(true);
        self.toast(format!("{}: {}", tr("node"), self.ctx.node), false);
    }

    fn toast(&mut self, msg: String, err: bool) {
        self.toast = Some((msg, err, Instant::now()));
    }

    pub fn note_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    // -----------------------------------------------------------------------
    // ticking
    // -----------------------------------------------------------------------

    pub fn tick(&mut self) {
        match self.screen {
            Screen::Dashboard => {
                if self.host_at.elapsed() > Duration::from_secs(30) {
                    self.host = login::host_info();
                    self.host_at = Instant::now();
                }
                return;
            }
            Screen::Login => {
                if let Some(init) = self.login.tick() {
                    self.start_session(init);
                }
                if self.remote.is_none() && !self.login.busy() && self.last_activity.elapsed() > Duration::from_secs(120) && self.ctx.cfg.prelogin_dashboard {
                    self.login.reset();
                    self.screen = Screen::Dashboard;
                }
                return;
            }
            Screen::Main => {}
        }

        if self.last_activity.elapsed() >= Duration::from_secs(self.ctx.cfg.idle_timeout_secs.max(30)) {
            self.logout();
            return;
        }

        for (p, _) in &mut self.popups {
            p.tick();
        }
        // countdown confirms answer themselves when time runs out
        if let Some(tag) = self.popups.last().and_then(|(p, _)| p.expired()) {
            if let Some((_, origin)) = self.popups.pop() {
                if let Origin::View(m, v) = origin {
                    self.modules[m].views[v].on_confirm(&tag, &mut self.ctx);
                    self.process_cmds(origin);
                }
            }
        }

        // background tasks
        let mut finished = Vec::new();
        for (i, (t, _)) in self.bg_tasks.iter_mut().enumerate() {
            t.tick();
            if t.done {
                finished.push(i);
            }
        }
        for i in finished.into_iter().rev() {
            let (t, origin) = self.bg_tasks.remove(i);
            let ok = t.ok();
            self.toast(format!("{}: {}", t.title, if ok { "OK" } else { "failed" }), !ok);
            self.refresh_origin(origin);
        }

        // finished actions
        let mut i = 0;
        while i < self.actions.len() {
            if let Some(r) = self.actions[i].1.poll() {
                let (title, _, origin) = self.actions.remove(i);
                match r {
                    Ok(ActionOutcome::Upid(upid)) => {
                        let node = api::upid_node(&upid).unwrap_or_else(|| self.ctx.node.clone());
                        let tv = TaskView::new(self.ctx.api.clone(), &node, &upid, &title);
                        self.popups.push((Popup::Task(Box::new(tv)), origin));
                    }
                    Ok(ActionOutcome::Msg(m)) => {
                        self.toast(m, false);
                        self.refresh_origin(origin);
                    }
                    Ok(ActionOutcome::Text(t, body)) => {
                        let lv = crate::ui::logview::LogViewer::new(&t, crate::ui::logview::Source::Static(body.lines().map(|s| s.to_string()).collect()));
                        self.popups.push((Popup::Log(Box::new(lv)), origin));
                    }
                    Err(e) => {
                        self.popups.push((Popup::error(&title, &format!("{e:#}")), origin));
                        self.refresh_origin(origin);
                    }
                }
            } else {
                i += 1;
            }
        }

        // current view polling + auto refresh
        let (m, v) = (self.cur, self.modules[self.cur].sel);
        {
            let view = &mut self.modules[m].views[v];
            view.poll(&mut self.ctx);
            if self.auto_refresh {
                if let Some(secs) = view.refresh_secs() {
                    let due = self.last_refresh.get(&(m, v)).map(|t| t.elapsed() >= Duration::from_secs(secs.max(self.ctx.cfg.refresh_secs))).unwrap_or(true);
                    if due {
                        view.refresh(&mut self.ctx);
                        self.last_refresh.insert((m, v), Instant::now());
                    }
                }
            }
        }
        self.process_cmds(Origin::View(m, v));

        // ticket renewal
        if self.last_renew_check.elapsed() > Duration::from_secs(300) && self.renew.is_none() {
            self.last_renew_check = Instant::now();
            if self.ctx.api.needs_renew() {
                let api = self.ctx.api.clone();
                let forge = match &self.ssh {
                    Some(t) => Some(crate::exec::Exec::Ssh(t.target.clone())),
                    None if crate::util::is_root_process() && self.remote.is_none() => Some(crate::exec::Exec::Local),
                    None => None,
                };
                self.renew = Some(Job::spawn(move || {
                    if api.renew().is_err() && api.auth().map(|a| a.user == "root@pam").unwrap_or(false) {
                        if let Some(e) = forge {
                            api.set_auth(api::forge_root_auth_with(&e)?);
                        }
                    }
                    Ok(())
                }));
            }
        }
        if let Some(j) = &self.renew {
            if let Some(r) = j.poll() {
                self.renew = None;
                if let Err(e) = r {
                    self.toast(format!("ticket renewal failed: {e:#}"), true);
                }
            }
        }

        // rescue: switch back to the HTTP API once pveproxy is reachable again
        if self.ctx.session.rescue && self.recover.is_none() && self.last_recover.elapsed() > Duration::from_secs(15) {
            self.last_recover = Instant::now();
            self.recover = Some(Job::spawn(|| {
                let api = Api::local();
                api.domains()?;
                api.set_auth(api::forge_root_auth()?);
                api.get("/version")?;
                Ok(Arc::new(api))
            }));
        }
        if let Some(j) = &self.recover {
            if let Some(r) = j.poll() {
                self.recover = None;
                if let Ok(api) = r {
                    self.ctx.api = api;
                    self.ctx.session.rescue = false;
                    self.toast("PVE API is reachable again: left rescue mode".into(), false);
                    self.start_metrics();
                }
            }
        }

        if let Some(j) = &self.arch_job {
            if let Some(r) = j.poll() {
                self.arch_job = None;
                if let Ok((n, a)) = r {
                    if !a.is_empty() {
                        self.ctx.node_arch.insert(n, a);
                    }
                }
            }
        }

        // alerts
        self.alerts.poll();
        if self.auto_refresh && self.alerts.stale(Duration::from_secs(30)) {
            let api = self.ctx.api.clone();
            let node = self.ctx.node.clone();
            let exec = self.ctx.exec();
            self.alerts.load(move || Ok(collect_alerts(&api, &node, exec)));
        }

        if let Some((_, _, t)) = &self.toast {
            if t.elapsed() > Duration::from_secs(6) {
                self.toast = None;
            }
        }
    }

    fn refresh_origin(&mut self, origin: Origin) {
        if let Origin::View(m, v) = origin {
            if m < self.modules.len() && v < self.modules[m].views.len() {
                self.modules[m].views[v].refresh(&mut self.ctx);
                self.last_refresh.insert((m, v), Instant::now());
                self.process_cmds(origin);
            }
        }
    }

    /// F7: pause / resume all refreshing, including the live samplers (IO, traffic, status bar).
    fn toggle_pause(&mut self) {
        self.auto_refresh = !self.auto_refresh;
        self.ctx.metrics.lock().unwrap().paused = !self.auto_refresh;
        self.toast(tr(if self.auto_refresh { "Refreshing resumed" } else { "Display paused (r = refresh once, F7 = resume)" }).to_string(), false);
    }

    fn refresh_current(&mut self, force: bool) {
        let (m, v) = (self.cur, self.modules[self.cur].sel);
        let stale = self.last_refresh.get(&(m, v)).map(|t| t.elapsed() > Duration::from_secs(2)).unwrap_or(true);
        if force || stale {
            self.modules[m].views[v].refresh(&mut self.ctx);
            self.last_refresh.insert((m, v), Instant::now());
            self.process_cmds(Origin::View(m, v));
        }
    }

    fn process_cmds(&mut self, origin: Origin) {
        let cmds = std::mem::take(&mut self.ctx.cmds);
        for c in cmds {
            match c {
                Cmd::Popup(p) => self.popups.push((p, origin)),
                Cmd::Toast(m, e) => self.toast(m, e),
                Cmd::Action { title, job } => {
                    crate::audit(&self.ctx.session.user, &self.ctx.node, &title);
                    self.toast(format!("{title}..."), false);
                    self.actions.push((title, job, origin));
                }
                Cmd::Suspend(s) => self.pending_suspend = Some(s),
                Cmd::Vnc(r) => self.pending_vnc = Some(r),
                Cmd::SwitchNode(n) => self.switch_node(n),
            }
        }
    }

    fn goto(&mut self, m: usize, v: usize) {
        if m < self.modules.len() {
            self.cur = m;
            if v < self.modules[m].views.len() {
                self.modules[m].sel = v;
            }
            self.focus = Focus::List;
            self.narrow_detail = false;
            self.refresh_current(false);
        }
    }

    // -----------------------------------------------------------------------
    // keys
    // -----------------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        self.note_activity();
        // quitting: only Ctrl+Q (outside tty1 / serial console), always confirmed
        if self.confirm_quit {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.should_exit = true,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.confirm_quit = false,
                _ => {}
            }
            return;
        }
        if self.can_quit && key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
            self.confirm_quit = true;
            return;
        }
        match self.screen {
            Screen::Dashboard => {
                match key.code {
                    KeyCode::Enter | KeyCode::F(2) | KeyCode::Char(' ') => self.screen = Screen::Login,
                    _ => {}
                }
                return;
            }
            Screen::Login => {
                match self.login.handle_key(&key) {
                    Some(LoginEvent::Back) => {
                        if self.remote.is_none() && self.ssh.is_none() && self.ctx.cfg.prelogin_dashboard {
                            self.screen = Screen::Dashboard;
                        }
                    }
                    Some(LoginEvent::Trusted(fp)) => {
                        if let Some(r) = &self.remote {
                            if let Err(e) = api::save_fingerprint(&r.host, &fp) {
                                self.login.message = Some((format!("could not save fingerprint: {e}"), true));
                            }
                            let api = Arc::new(Api::http(&r.host, r.port, Some(fp)));
                            self.ctx.api = api.clone();
                            self.login.set_api(api);
                        }
                    }
                    None => {}
                }
                return;
            }
            Screen::Main => {}
        }

        // popups first
        if let Some((popup, origin)) = self.popups.last_mut() {
            let origin = *origin;
            let out = popup.handle_key(&key);
            self.apply_popup_outcome(out, origin);
            return;
        }

        let (m, v) = (self.cur, self.modules[self.cur].sel);
        let origin = Origin::View(m, v);

        // filter input line
        if self.filter_edit {
            if let Some(t) = self.modules[m].views[v].table() {
                match key.code {
                    KeyCode::Esc => {
                        t.filter.clear();
                        self.filter_edit = false;
                    }
                    KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.filter_edit = false,
                    KeyCode::Backspace => {
                        t.filter.pop();
                    }
                    KeyCode::Char(c) => t.filter.push(c),
                    _ => {}
                }
            } else {
                self.filter_edit = false;
            }
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // global keys
        match key.code {
            KeyCode::F(n @ 1..=5) => {
                let i = n as usize - 1;
                return self.goto(i, self.modules[i].sel);
            }
            KeyCode::F(6) => return self.open_node_picker(),
            KeyCode::F(8) => return self.goto_title("Tasks"),
            KeyCode::F(10) => return self.open_shell(),
            KeyCode::F(7) => return self.toggle_pause(),
            KeyCode::Char('r') if ctrl => return self.refresh_current(true),
            // Tab moves between the list and the details (modules are F1-F5)
            KeyCode::Tab | KeyCode::BackTab => {
                if self.modules[m].views[v].has_detail() {
                    if self.focus == Focus::Detail || self.narrow_detail {
                        self.focus = Focus::List;
                        self.narrow_detail = false;
                    } else {
                        self.focus = Focus::Detail;
                        self.narrow_detail = self.width < 110;
                    }
                }
                return;
            }
            KeyCode::Char(c @ '1'..='9') if !ctrl && !alt => {
                let i = c as usize - '1' as usize;
                if i < self.modules[m].views.len() {
                    return self.goto(m, i);
                }
                return;
            }
            KeyCode::Char('[') => {
                let n = self.modules[m].views.len();
                return self.goto(m, (v + n - 1) % n);
            }
            KeyCode::Char(']') => {
                let n = self.modules[m].views.len();
                return self.goto(m, (v + 1) % n);
            }
            KeyCode::Char('/') => {
                if self.modules[m].views[v].table().is_some() {
                    self.filter_edit = true;
                    self.focus = Focus::List;
                }
                return;
            }
            KeyCode::Char('g') if !ctrl => return self.open_global_menu(),
            KeyCode::Char('g') if ctrl => return self.open_global_menu(),
            KeyCode::Char('?') => return self.open_help(),
            // Esc, innermost first: details -> filter -> the page's own "back" -> marks
            KeyCode::Esc => {
                if self.narrow_detail || self.focus == Focus::Detail {
                    self.narrow_detail = false;
                    self.focus = Focus::List;
                    return;
                }
                if let Some(t) = self.modules[m].views[v].table() {
                    if !t.filter.is_empty() {
                        t.filter.clear();
                        return;
                    }
                }
                if self.modules[m].views[v].on_key(&key, &mut self.ctx) {
                    self.process_cmds(origin);
                    return;
                }
                if let Some(t) = self.modules[m].views[v].table() {
                    t.marked.clear();
                }
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Detail => match key.code {
                KeyCode::Up | KeyCode::Char('k') => return self.modules[m].views[v].scroll_detail(-1),
                KeyCode::Down | KeyCode::Char('j') => return self.modules[m].views[v].scroll_detail(1),
                KeyCode::PageUp => return self.modules[m].views[v].scroll_detail(-10),
                KeyCode::PageDown => return self.modules[m].views[v].scroll_detail(10),
                KeyCode::Left | KeyCode::Char('h') => {
                    self.focus = Focus::List;
                    self.narrow_detail = false;
                    return;
                }
                _ => {}
            },
            Focus::List => {}
        }

        // view specific keys
        if self.modules[m].views[v].on_key(&key, &mut self.ctx) {
            self.process_cmds(origin);
            return;
        }

        if key.code == KeyCode::Enter || key.code == KeyCode::Char('m') {
            return self.open_context_menu();
        }

        // menu hotkeys
        if let KeyCode::Char(c) = key.code {
            if !ctrl && !matches!(c, 'h' | 'j' | 'k' | 'l' | 'r' | 'a' | ' ' | 'G') {
                let items = self.view_menu(m, v);
                if let Some(it) = items.iter().find(|i| i.key == c) {
                    if let Some(reason) = &it.disabled {
                        self.toast(format!("{}: {reason}", tr(&it.label)), true);
                    } else {
                        self.modules[m].views[v].on_menu("ctx", c, &mut self.ctx);
                        self.process_cmds(origin);
                    }
                    return;
                }
            }
        }

        if self.focus == Focus::List {
            if let Some(t) = self.modules[m].views[v].table() {
                if t.handle_key(&key) {
                    return;
                }
            }
        }

        match key.code {
            KeyCode::Right | KeyCode::Char('l') => {
                if self.modules[m].views[v].has_detail() {
                    self.focus = Focus::Detail;
                    if self.width < 110 {
                        self.narrow_detail = true;
                    }
                }
            }
            KeyCode::Char('r') => {
                // while paused: one new snapshot of the live data too
                self.ctx.metrics.lock().unwrap().step = true;
                self.refresh_current(true)
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                // views without a table scroll their main pane
                let d = match key.code {
                    KeyCode::Up => -1,
                    KeyCode::Down => 1,
                    KeyCode::PageUp => -10,
                    _ => 10,
                };
                self.modules[m].views[v].scroll_detail(d);
            }
            _ => {}
        }
    }

    /// A view's menu; in API-only remote mode actions that need host commands are hidden.
    fn apply_popup_outcome(&mut self, out: Outcome, origin: Origin) {
        match out {
            Outcome::None => {}
            Outcome::Close => {
                if let Some((p, o)) = self.popups.pop() {
                    if let Popup::Task(t) = p {
                        if !t.done {
                            self.bg_tasks.push((*t, o));
                        } else {
                            self.refresh_origin(o);
                        }
                    }
                }
            }
            Outcome::Menu(tag, k) => {
                self.popups.pop();
                match origin {
                    Origin::Global => self.global_action(&tag, k),
                    Origin::View(m, v) => {
                        self.modules[m].views[v].on_menu(&tag, k, &mut self.ctx);
                        self.process_cmds(origin);
                    }
                }
            }
            Outcome::Confirmed(tag) => {
                self.popups.pop();
                match origin {
                    Origin::Global => self.global_confirm(&tag),
                    Origin::View(m, v) => {
                        self.modules[m].views[v].on_confirm(&tag, &mut self.ctx);
                        self.process_cmds(origin);
                    }
                }
            }
            Outcome::Form(tag, values) => {
                self.popups.pop();
                if let Origin::View(m, v) = origin {
                    self.modules[m].views[v].on_form(&tag, &values, &mut self.ctx);
                    self.process_cmds(origin);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // mouse
    // -----------------------------------------------------------------------

    pub fn on_mouse(&mut self, m: MouseEvent) {
        if matches!(m.kind, MouseEventKind::Moved) {
            return;
        }
        self.note_activity();
        if self.confirm_quit {
            return;
        }
        let left = matches!(m.kind, MouseEventKind::Down(MouseButton::Left));
        let right = matches!(m.kind, MouseEventKind::Down(MouseButton::Right));
        match self.screen {
            Screen::Dashboard => {
                if left {
                    self.screen = Screen::Login;
                }
                return;
            }
            Screen::Login => return,
            Screen::Main => {}
        }
        if let Some((popup, origin)) = self.popups.last_mut() {
            let origin = *origin;
            let out = popup.handle_mouse(&m, Rect::default());
            self.apply_popup_outcome(out, origin);
            return;
        }
        let (mx, my) = (m.column, m.row);
        let hit = self.hits.iter().find(|(r, _)| mx >= r.x && mx < r.x + r.width && my >= r.y && my < r.y + r.height).map(|(_, h)| *h);
        let (cm, cv) = (self.cur, self.modules[self.cur].sel);
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d = if matches!(m.kind, MouseEventKind::ScrollUp) { -3 } else { 3 };
                match hit {
                    Some(Hit::Detail) => self.modules[cm].views[cv].scroll_detail(d),
                    Some(Hit::List) => match self.modules[cm].views[cv].table() {
                        Some(t) => t.scroll(d),
                        None => self.modules[cm].views[cv].scroll_detail(d),
                    },
                    _ => {}
                }
                return;
            }
            _ => {}
        }
        if !left && !right {
            return;
        }
        self.filter_edit = false;
        match hit {
            Some(Hit::Tab(i)) if left => self.goto(i, self.modules[i].sel),
            Some(Hit::Sub(i)) if left => self.goto(cm, i),
            Some(Hit::Action(c)) if left => {
                if let Some(it) = self.view_menu(cm, cv).into_iter().find(|i| i.key == c) {
                    match it.disabled {
                        Some(reason) => self.toast(format!("{}: {reason}", tr(&it.label)), true),
                        None => {
                            self.modules[cm].views[cv].on_menu("ctx", c, &mut self.ctx);
                            self.process_cmds(Origin::View(cm, cv));
                        }
                    }
                }
            }
            Some(Hit::More) if left => self.open_context_menu(),
            Some(Hit::Detail) => {
                if left && self.modules[cm].views[cv].on_click(mx, my, &mut self.ctx) {
                    self.process_cmds(Origin::View(cm, cv));
                } else {
                    self.focus = Focus::Detail;
                }
            }
            Some(Hit::List) => {
                self.focus = Focus::List;
                let on_row = self.modules[cm].views[cv].table().map(|t| t.click(mx, my)).unwrap_or(false);
                if on_row {
                    // right click or double click: open the action menu for that row
                    let double = self.last_click.map(|(t, x, y)| t.elapsed() < Duration::from_millis(400) && x == mx && y == my).unwrap_or(false);
                    if right || double {
                        self.last_click = None;
                        self.open_context_menu();
                    } else {
                        self.last_click = Some((Instant::now(), mx, my));
                    }
                }
            }
            _ => {}
        }
    }

    fn view_menu(&self, m: usize, v: usize) -> Vec<MenuItem> {
        let mut items = self.modules[m].views[v].menu(&self.ctx);
        if self.ctx.remote {
            items.retain(|i| i.disabled.as_deref() != Some(crate::ctx::NEEDS_SSH));
        }
        items
    }

    fn goto_title(&mut self, title: &str) {
        for (m, module) in self.modules.iter().enumerate() {
            if let Some(v) = module.views.iter().position(|x| x.title() == title) {
                return self.goto(m, v);
            }
        }
    }

    fn open_context_menu(&mut self) {
        let (m, v) = (self.cur, self.modules[self.cur].sel);
        let items = self.view_menu(m, v);
        if items.is_empty() {
            return;
        }
        let title = self.modules[m].views[v].menu_title();
        self.popups.push((Popup::menu(&title, "ctx", items), Origin::View(m, v)));
    }

    fn open_global_menu(&mut self) {
        let root = self.ctx.need_root();
        let items: Vec<MenuItem> = vec![
            item('n', "Switch node (F6)").disabled_if(self.ctx.nodes.len() < 2, "single node"),
            item('r', "Refresh all"),
            item('a', "Pause / resume refreshing (F7)"),
            item('t', "Task center (F8)"),
            item('A', "Alerts"),
            item('L', "Language").disabled_if(!i18n::cjk_capable(), "tty: English only"),
            item('s', "Enter shell").require(root.clone()).disabled_if(self.ctx.exec().is_none(), tr("local only")),
            item('?', "Help"),
            item('i', "About"),
            item('o', "Log out"),
            item('Q', "Quit").disabled_if(!self.can_quit, "system console"),
        ];
        self.popups.push((Popup::menu("Global Menu", "global", items), Origin::Global));
    }

    fn open_node_picker(&mut self) {
        if self.ctx.nodes.is_empty() {
            return;
        }
        let keys: Vec<char> = "123456789abcdefhijklmnopqrstuvwxyz".chars().collect();
        let items: Vec<MenuItem> = self
            .ctx
            .nodes
            .iter()
            .enumerate()
            .take(keys.len())
            .map(|(i, n)| {
                let mut label = format!("{:<16} {:<16} {}", n.name, n.ip, if n.online { "online" } else { "OFFLINE" });
                if n.local {
                    label.push_str("  (local)");
                }
                if n.name == self.ctx.node {
                    label.push_str("  *");
                }
                MenuItem { key: keys[i], label, disabled: if n.online { None } else { Some("offline".into()) } }
            })
            .collect();
        self.popups.push((Popup::menu("Switch node", "node", items), Origin::Global));
    }

    fn open_help(&mut self) {
        let zh = i18n::is_zh();
        let text = if zh { HELP_ZH } else { HELP_EN };
        self.popups.push((Popup::text("Help", text), Origin::Global));
    }

    fn open_shell(&mut self) {
        if let Err(e) = self.ctx.need_root() {
            self.toast(format!("{}: {e}", tr("Enter shell")), true);
            return;
        }
        let Some(exec) = self.ctx.exec() else {
            self.toast(format!("{}: {}", tr("Enter shell"), tr("local only")), true);
            return;
        };
        let (prog, args) = match exec {
            crate::exec::Exec::Local => ("/bin/bash".to_string(), vec!["-i".to_string(), "-l".to_string()]),
            _ => ("/bin/bash".to_string(), vec!["-l".to_string()]),
        };
        self.pending_suspend = Some(Suspend {
            banner: format!(
                "Entering shell on {} ({}). Type 'exit' to return to the console.\nShell will auto-logout after {} minutes of inactivity.\n",
                self.ctx.node,
                exec.label(),
                self.ctx.cfg.idle_timeout_secs / 60
            ),
            exec,
            prog,
            args,
            env: vec![("TMOUT".into(), self.ctx.cfg.idle_timeout_secs.to_string())],
            logout_after: self.ssh.is_none() && !self.auto_session,
            pause_after: false,
        });
        crate::audit(&self.ctx.session.user, &self.ctx.node, "enter shell");
    }

    fn global_action(&mut self, tag: &str, k: char) {
        if tag == "node" {
            let keys: Vec<char> = "123456789abcdefhijklmnopqrstuvwxyz".chars().collect();
            if let Some(i) = keys.iter().position(|c| *c == k) {
                if let Some(n) = self.ctx.nodes.get(i) {
                    let name = n.name.clone();
                    self.switch_node(name);
                }
            }
            return;
        }
        match k {
            'n' => self.open_node_picker(),
            'r' => {
                self.last_refresh.clear();
                let api = self.ctx.api.clone();
                let (nodes, cluster) = login::load_nodes(&api);
                if !nodes.is_empty() {
                    self.ctx.nodes = nodes;
                    self.cluster = cluster;
                }
                self.refresh_current(true);
            }
            'a' => {
                self.toggle_pause();
            }
            't' => self.goto_title("Tasks"),
            'A' => {
                let text = match &self.alerts.data {
                    Some(a) if !a.is_empty() => a.join("\n"),
                    _ => tr("No alerts").to_string(),
                };
                self.popups.push((Popup::text("Alerts", &text), Origin::Global));
            }
            'L' => {
                if !i18n::toggle() {
                    self.toast("this terminal cannot display Chinese".into(), true);
                }
            }
            's' => self.open_shell(),
            '?' => self.open_help(),
            'i' => {
                let text = format!(
                    "pxvirt-console {APP_VERSION}\n\nPXVIRT Direct Console User Interface\nhttps://github.com/jiangcuo/pxvirt-console\n\nAPI: {}{}\nUser: {}\nNode: {}\nCluster: {}",
                    self.ctx.api.base_url(),
                    if self.ctx.api.is_pvesh() { " (pvesh)" } else { "" },
                    self.ctx.session.user,
                    self.ctx.node,
                    if self.cluster.is_empty() { "-" } else { &self.cluster }
                );
                self.popups.push((Popup::text("About", &text), Origin::Global));
            }
            'o' => {
                if self.ssh.is_some() || self.auto_session {
                    // automatic sessions: logging out quits, so ask first
                    self.confirm_quit = true;
                } else {
                    self.logout();
                }
            }
            'Q' => self.confirm_quit = true,
            _ => {}
        }
    }

    fn global_confirm(&mut self, tag: &str) {
        if tag == "quit" {
            self.should_exit = true;
        }
    }

    // -----------------------------------------------------------------------
    // drawing
    // -----------------------------------------------------------------------

    pub fn draw(&mut self, f: &mut Frame) {
        self.draw_screen(f);
        if self.confirm_quit {
            let area = f.area();
            let r = ui::centered(44, 6, area);
            ui::clear(f, r);
            let lines = vec![
                Line::from(Span::styled(tr("Quit pxvirt-console?").to_string(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" Y ", Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)),
                    Span::raw(format!(" {}     ", tr("Quit"))),
                    Span::styled(" N ", Style::default().bg(Color::Gray).fg(Color::Black).add_modifier(Modifier::BOLD)),
                    Span::raw(format!(" {}", tr("Cancel"))),
                ]),
            ];
            f.render_widget(Paragraph::new(lines).block(ui::popup_block("Quit")), r);
        }
    }

    fn draw_screen(&mut self, f: &mut Frame) {
        let area = f.area();
        self.width = area.width;
        let mdata = self.metrics.snapshot();
        ui::clear(f, area);
        match self.screen {
            Screen::Dashboard | Screen::Login => {
                let rows = vsplit(area, &[Constraint::Length(1), Constraint::Min(5), Constraint::Length(1), Constraint::Length(1)]);
                self.draw_header(f, rows[0]);
                if self.remote.is_none() && self.screen == Screen::Dashboard {
                    login::draw_dashboard(f, rows[1], &self.host, &mdata);
                }
                if self.screen == Screen::Login {
                    self.login.draw(f, rows[1], &self.host_label());
                }
                if mdata.ok {
                    draw_status(f, rows[2], &mdata, None);
                }
                let mut hints = if self.screen == Screen::Dashboard { vec![("Enter", "Log in")] } else { vec![("Enter", "Log in"), ("Tab", "next field"), ("Esc", "back")] };
                if self.can_quit {
                    hints.push(("Ctrl+Q", "Quit"));
                }
                f.render_widget(Paragraph::new(Line::from(hint_spans(&hints))).style(Style::default().bg(ui::BAR).fg(Color::White)), rows[3]);
                return;
            }
            Screen::Main => {}
        }

        let full = self.popups.last().map(|(p, _)| p.fullscreen()).unwrap_or(false);
        if full {
            if let Some((p, _)) = self.popups.last_mut() {
                p.draw(f, area);
            }
            return;
        }

        // action bar: every action of the current page, always visible
        let (cm, cv) = (self.cur, self.modules[self.cur].sel);
        self.hits.clear();
        let (action_lines, chips) = action_bar_lines(&self.view_menu(cm, cv), area.width);
        let mut cons = vec![Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(3)];
        if self.filter_edit {
            cons.push(Constraint::Length(1));
        }
        if !action_lines.is_empty() {
            cons.push(Constraint::Length(action_lines.len() as u16));
        }
        cons.extend([Constraint::Length(1), Constraint::Length(1)]);
        let rows = vsplit(area, &cons);
        let mut i = 0;
        self.draw_header(f, rows[i]);
        i += 1;
        self.draw_tabs(f, rows[i]);
        i += 1;
        self.draw_subtabs(f, rows[i]);
        i += 1;
        let body = rows[i];
        i += 1;
        self.draw_body(f, body);
        if self.filter_edit {
            let (m, v) = (self.cur, self.modules[self.cur].sel);
            let filter = self.modules[m].views[v].table().map(|t| t.filter.clone()).unwrap_or_default();
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(format!(" {}", tr("Filter: ")), Style::default().fg(ACCENT)), Span::raw(format!("{filter}_"))])),
                rows[i],
            );
            i += 1;
        }
        if !action_lines.is_empty() {
            f.render_widget(Paragraph::new(action_lines).style(ui::base()), rows[i]);
            for (line, x, w, key) in chips {
                let r = Rect::new(rows[i].x + x, rows[i].y + line, w, 1);
                self.hits.push((r, key.map(Hit::Action).unwrap_or(Hit::More)));
            }
            i += 1;
        }
        let alerts = self.alerts.data.as_ref().map(|a| a.len());
        draw_status(f, rows[i], &mdata, Some((alerts, self.bg_tasks.len() + self.actions.len())));
        i += 1;
        self.draw_hints(f, rows[i]);

        for (p, _) in self.popups.iter_mut() {
            p.draw(f, body);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let bar = Style::default().bg(ui::BAR).fg(Color::White);
        f.render_widget(Paragraph::new("").style(bar), area);
        let mut left = vec![Span::styled(" PXVIRT ", bar.add_modifier(Modifier::BOLD)), Span::styled(format!("│ {}", self.host_label()), bar)];
        if self.screen == Screen::Main && !self.cluster.is_empty() {
            left.push(Span::styled(format!(" · {}", self.cluster), bar));
        }
        let mut right: Vec<Span> = Vec::new();
        if self.screen == Screen::Main {
            if self.ctx.session.rescue {
                right.push(Span::styled(" RESCUE ", Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD)));
                right.push(Span::styled(" ", bar));
            }
            if let Some(t) = &self.ssh {
                right.push(Span::styled(format!(" SSH {} ", t.label), Style::default().bg(Color::Magenta).fg(Color::White)));
                right.push(Span::styled(" ", bar));
            }
            if let Some(r) = &self.remote {
                right.push(Span::styled(format!(" remote {} ", r.host), Style::default().bg(Color::Magenta).fg(Color::White)));
                right.push(Span::styled(" ", bar));
            }
            if !self.auto_refresh {
                right.push(Span::styled(format!(" ⏸ {} · F7 ", tr("paused")), Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD)));
                right.push(Span::styled(" ", bar));
            }
            right.push(Span::styled(format!("{}  ", self.ctx.session.user), bar));
            let arch = self.ctx.node_arch.get(&self.ctx.node).map(|a| format!(" {a}")).unwrap_or_default();
            right.push(Span::styled(format!(" {}{arch} ", self.ctx.node), Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)));
        }
        right.push(Span::styled(format!("  {} ", chrono::Local::now().format("%H:%M")), bar));
        let rw: usize = right.iter().map(|s| s.width()).sum();
        let parts = hsplit(area, &[Constraint::Min(10), Constraint::Length(rw as u16)]);
        f.render_widget(Paragraph::new(Line::from(left)).style(bar), parts[0]);
        f.render_widget(Paragraph::new(Line::from(right)).style(bar), parts[1]);
    }

    /// Module tabs: F1..F5, the current one as a cyan block.
    fn draw_tabs(&mut self, f: &mut Frame, area: Rect) {
        let mut spans = vec![Span::raw(" ")];
        let mut x = area.x + 1;
        for (i, m) in self.modules.iter().enumerate() {
            let label = format!(" F{} {} ", i + 1, tr(m.title));
            let w = ui::unicode_width(&label) as u16;
            self.hits.push((Rect::new(x, area.y, w, 1), Hit::Tab(i)));
            x += w + 1;
            if i == self.cur {
                spans.push(Span::styled(label, Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)));
            } else {
                spans.push(Span::styled(label, Style::default().bg(ui::BAR).fg(Color::White)));
            }
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(ui::base()), area);
    }

    /// Submenu: 1..9, the current one as a white block.
    fn draw_subtabs(&mut self, f: &mut Frame, area: Rect) {
        let m = &self.modules[self.cur];
        let mut spans = vec![Span::raw(" ")];
        let mut x = area.x + 1;
        for (i, v) in m.views.iter().enumerate() {
            let label = format!(" {} {} ", i + 1, tr(v.title()));
            let w = ui::unicode_width(&label) as u16;
            self.hits.push((Rect::new(x, area.y, w, 1), Hit::Sub(i)));
            x += w + 1;
            if i == m.sel {
                spans.push(Span::styled(label, Style::default().bg(Color::Gray).fg(Color::Black).add_modifier(Modifier::BOLD)));
            } else {
                spans.push(Span::styled(label, Style::default().fg(Color::Gray)));
            }
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(ui::base()), area);
    }

    fn draw_body(&mut self, f: &mut Frame, area: Rect) {
        let (m, v) = (self.cur, self.modules[self.cur].sel);
        let focus = self.focus;
        let width = area.width;
        let split = |a: Rect, c: &[Constraint]| Layout::default().direction(Direction::Horizontal).constraints(c.to_vec()).spacing(2).split(a).to_vec();
        let view = &mut self.modules[m].views[v];
        if !view.has_detail() {
            self.hits.push((area, Hit::List));
            view.draw_list(f, area, &self.ctx, true);
            return;
        }
        if width < 110 {
            if self.narrow_detail {
                self.hits.push((area, Hit::Detail));
                view.draw_detail(f, area, &self.ctx, true);
            } else {
                self.hits.push((area, Hit::List));
                view.draw_list(f, area, &self.ctx, true);
            }
            return;
        }
        if width >= 200 {
            let parts = split(area, &[Constraint::Percentage(48), Constraint::Percentage(30), Constraint::Percentage(22)]);
            self.hits.push((parts[0], Hit::List));
            self.hits.push((parts[1], Hit::Detail));
            view.draw_list(f, parts[0], &self.ctx, focus == Focus::List);
            view.draw_detail(f, parts[1], &self.ctx, focus == Focus::Detail);
            if !view.draw_extra(f, parts[2], &self.ctx) {
                draw_metrics_panel(f, parts[2], &self.metrics.snapshot());
            }
            return;
        }
        let parts = split(area, &[Constraint::Percentage(58), Constraint::Percentage(42)]);
        self.hits.push((parts[0], Hit::List));
        self.hits.push((parts[1], Hit::Detail));
        view.draw_list(f, parts[0], &self.ctx, focus == Focus::List);
        view.draw_detail(f, parts[1], &self.ctx, focus == Focus::Detail);
    }

    fn draw_hints(&mut self, f: &mut Frame, area: Rect) {
        if let Some((msg, err, _)) = &self.toast {
            let st = if *err { Style::default().fg(Color::Red) } else { Style::default().fg(Color::Green) };
            crate::ui::clear(f, area);
            f.render_widget(Paragraph::new(Line::from(vec![Span::styled(" ● ", st), Span::styled(msg.clone(), st)])).style(ui::base()), area);
            return;
        }
        let (m, v) = (self.cur, self.modules[self.cur].sel);
        // owned (key, label) pairs; the page's own actions come first so they are discoverable
        let mut hints: Vec<(String, String)> = Vec::new();
        if !self.popups.is_empty() {
            hints.extend([("Esc".into(), "close".into()), ("Enter".into(), "select".into())]);
        } else {
            let view = &self.modules[m].views[v];
            hints.extend(view.hints().into_iter().map(|(k, l)| (k.to_string(), l.to_string())));
            hints.push(("Enter".into(), "menu".into()));
            if view.has_detail() && self.width < 110 {
                hints.push(("→".into(), "details".into()));
            }
            hints.extend([("/".into(), "filter".into()), ("F6".into(), "node".into()), ("?".into(), "help".into())]);
            if self.can_quit {
                hints.push(("Ctrl+Q".into(), "Quit".into()));
            }
        }
        let refs: Vec<(&str, &str)> = hints.iter().map(|(k, l)| (k.as_str(), l.as_str())).collect();
        f.render_widget(Paragraph::new(Line::from(hint_spans(&refs))).style(Style::default().bg(ui::BAR).fg(Color::White)), area);
    }
}

/// Action bar label: drop the parenthesised detail ("Kill (SIGKILL)" -> "Kill").
fn short_label(l: &str) -> String {
    let cut = l.find(" (").or_else(|| l.find('（')).or_else(|| l.find('(')).unwrap_or(l.len());
    l[..cut].trim().to_string()
}

/// The page's actions as "[key] label" chips, wrapped onto at most 3 lines.
/// Disabled actions stay visible (dimmed) so users learn what exists.
/// Chip positions: (line, x, width, key) - key None = "Enter more".
type Chips = Vec<(u16, u16, u16, Option<char>)>;

fn action_bar_lines(items: &[MenuItem], width: u16) -> (Vec<Line<'static>>, Chips) {
    let mut chips: Chips = Vec::new();
    if items.is_empty() {
        return (Vec::new(), chips);
    }
    let max_w = width.saturating_sub(1) as usize;
    let mut lines: Vec<Vec<Span<'static>>> = vec![vec![Span::styled(format!(" {} ", tr("Actions")), Style::default().fg(Color::Gray))]];
    let mut cur_w = lines[0][0].width();
    let indent = cur_w;
    for it in items {
        let enabled = it.disabled.is_none();
        let key_st = if enabled { Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD) } else { Style::default().bg(Color::Black).fg(Color::DarkGray) };
        let lab_st = if enabled { Style::default().fg(Color::White) } else { Style::default().fg(Color::DarkGray) };
        let key = Span::styled(format!(" {} ", it.key), key_st);
        let label = Span::styled(format!(" {}  ", short_label(tr(&it.label))), lab_st);
        let w = key.width() + label.width();
        let reserve = if lines.len() == 3 { 10 } else { 0 };
        if cur_w + w + reserve > max_w && cur_w > indent {
            if lines.len() == 3 {
                let more = Span::styled(format!(" Enter {}", tr("more")), Style::default().fg(Color::Gray));
                chips.push((2, cur_w as u16, more.width() as u16, None));
                lines[2].push(more);
                break;
            }
            lines.push(vec![Span::raw(" ".repeat(indent))]);
            cur_w = indent;
        }
        if enabled {
            chips.push(((lines.len() - 1) as u16, cur_w as u16, w as u16, Some(it.key)));
        }
        cur_w += w;
        let last = lines.last_mut().unwrap();
        last.push(key);
        last.push(label);
    }
    (lines.into_iter().map(Line::from).collect(), chips)
}

/// Bottom status line: CPU · memory · network · disk, alerts and running tasks on the right.
fn draw_status(f: &mut Frame, area: Rect, m: &MetricsData, extra: Option<(Option<usize>, usize)>) {
    let lab = Style::default().fg(Color::DarkGray);
    if !m.ok {
        f.render_widget(Paragraph::new(Span::styled(" metrics unavailable", lab)), area);
        return;
    }
    let spark = area.width >= 130;
    let coarse = if m.coarse { "~" } else { "" };
    let mf = if m.mem_total > 0.0 { m.mem_used / m.mem_total } else { 0.0 };
    let mut s: Vec<Span> = vec![Span::styled(" CPU ", lab), Span::styled(format!("{:.0}%", m.cpu * 100.0), usage_style(m.cpu))];
    if spark {
        s.push(Span::styled(format!(" {}", sparkline(&m.cpu_hist, 10, Some(1.0))), Style::default().fg(ACCENT)));
        s.push(Span::styled(format!("  load {:.2}", m.load[0]), lab));
    }
    s.push(Span::styled(format!("   {} ", tr("Memory")), lab));
    s.push(Span::styled(format!("{:.0}%", mf * 100.0), usage_style(mf)));
    if spark {
        s.push(Span::styled(format!(" {}/{}", fmt_bytes(m.mem_used), fmt_bytes(m.mem_total)), lab));
    }
    s.push(Span::styled(format!("   {} ", tr("Network")), lab));
    s.push(Span::raw(if spark { format!("{coarse}↓{} ↑{}", fmt_rate(m.net_rx), fmt_rate(m.net_tx)) } else { format!("↓{} ↑{}", fmt_bytes(m.net_rx), fmt_bytes(m.net_tx)) }));
    s.push(Span::styled(format!("   {} ", tr("Disk")), lab));
    s.push(Span::raw(if spark { format!("{coarse}R {} W {}", fmt_rate(m.disk_r), fmt_rate(m.disk_w)) } else { format!("R {} W {}", fmt_bytes(m.disk_r), fmt_bytes(m.disk_w)) }));
    if spark && !m.coarse {
        s.push(Span::styled(format!("   {} proc · {} thr", m.procs, m.threads), lab));
    }
    let mut right: Vec<Span> = Vec::new();
    if let Some((alerts, tasks)) = extra {
        if tasks > 0 {
            right.push(Span::styled(format!("◷ {tasks} {}  ", tr("Tasks")), Style::default().fg(ACCENT)));
        }
        if let Some(n) = alerts.filter(|n| *n > 0) {
            right.push(Span::styled(format!("● {n} {} ", tr("Alerts")), style_bad()));
        }
    }
    let rw: usize = right.iter().map(|x| x.width()).sum();
    let parts = hsplit(area, &[Constraint::Min(10), Constraint::Length(rw as u16)]);
    f.render_widget(Paragraph::new(Line::from(s)), parts[0]);
    f.render_widget(Paragraph::new(Line::from(right)), parts[1]);
}

/// Extra column on very wide terminals: bigger live graphs.
fn draw_metrics_panel(f: &mut Frame, area: Rect, m: &MetricsData) {
    let w = (area.width as usize).saturating_sub(2);
    let mut lines = Vec::new();
    let mut graph = |title: String, h: &[f64], max: Option<f64>| {
        lines.push(Line::from(Span::styled(title, ui::style_label())));
        lines.push(Line::from(Span::styled(sparkline(h, w, max), Style::default().fg(ACCENT))));
        lines.push(Line::from(""));
    };
    graph(format!("CPU {:.0}%  iowait {:.0}%", m.cpu * 100.0, m.iowait * 100.0), &m.cpu_hist, Some(1.0));
    graph(format!("{} {} / {}", tr("Memory"), fmt_bytes(m.mem_used), fmt_bytes(m.mem_total)), &m.mem_hist, Some(1.0));
    graph(format!("{} ↓{} ↑{}", tr("Network"), fmt_rate(m.net_rx), fmt_rate(m.net_tx)), &m.net_hist, None);
    graph(format!("{} R {} W {}", tr("Disk"), fmt_rate(m.disk_r), fmt_rate(m.disk_w)), &m.disk_hist, None);
    if m.arc_size > 0.0 {
        lines.push(Line::from(format!("ZFS ARC {}", fmt_bytes(m.arc_size))));
    }
    f.render_widget(Paragraph::new(lines).block(ui::block("Live", false)), area);
}

/// Cluster/node health problems shown as the alert count in the status bar.
fn collect_alerts(api: &Api, node: &str, exec: Option<crate::exec::Exec>) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(v) = api.get("/cluster/status") {
        for e in jarr(&v) {
            match js(&e, "type").as_str() {
                "cluster" if jf(&e, "quorate") == 0.0 => out.push(format!("cluster {} is NOT quorate", js(&e, "name"))),
                "node" if jf(&e, "online") == 0.0 => out.push(format!("node {} is offline", js(&e, "name"))),
                _ => {}
            }
        }
    }
    if let Ok(v) = api.get(&format!("/nodes/{node}/disks/zfs")) {
        for p in jarr(&v) {
            let h = js(&p, "health");
            if h != "ONLINE" {
                out.push(format!("ZFS pool {} is {h}", js(&p, "name")));
            }
        }
    }
    if let Ok(v) = api.get(&format!("/nodes/{node}/ceph/status")) {
        if let Some(h) = v.get("health") {
            let s = js(h, "status");
            if !s.is_empty() && s != "HEALTH_OK" {
                out.push(format!("Ceph {s}"));
            }
        }
    }
    if let Ok(v) = api.get(&format!("/nodes/{node}/storage")) {
        for s in jarr(&v) {
            let frac = jf(&s, "used_fraction");
            if frac >= 0.9 && jf(&s, "active") != 0.0 {
                out.push(format!("storage {} is {:.0}% full", js(&s, "storage"), frac * 100.0));
            }
        }
    }
    if let Some(exec) = exec {
        // hardware RAID / NVMe health (full scan at most every 5 minutes)
        out.extend(crate::raidhost::problems_cached(&exec));
        if let Ok(o) = exec.output("systemctl", &["--failed", "--no-legend", "--plain", "--type=service"]) {
            for l in o.lines() {
                if let Some(u) = l.split_whitespace().next() {
                    out.push(format!("unit {u} failed"));
                }
            }
        }
    }
    out
}

const HELP_EN: &str = "\
Global keys
  F1-F5               switch module (System, Network, Disks, Services & Logs, PVE)
  Tab                 move between the list and the details
  1-9 / [ ]           switch submenu
  Left/Right (h/l)    move between columns (menu, list, details)
  Up/Down (j/k)       move in lists, PgUp/PgDn, Home/End
  Enter / m           context menu with all actions for the selection
  letters             direct actions (shown in the menu); UPPERCASE = forced/dangerous variant
  Space               mark multiple rows (batch actions)
  /                   filter the list (each page remembers its filter), Esc clears
  r / Ctrl-R          refresh (once, also while paused)
  F7                  pause / resume all refreshing, live graphs included
  F6                  switch node (cluster)
  F8                  task center
  F10                 shell (root@pam)
  g                   global menu
  mouse               click tabs, submenus, rows and action buttons; right click or
                      double click a row for its menu; wheel scrolls
  g then o            log out (in SSH mode: disconnect)
  Ctrl+Q              quit (only in ssh / terminal emulators, not on tty1)

Log viewer
  Space follow/pause, / search, n/N next/prev, & show only matches,
  p priority, t time range, + load more, w export, Esc close

Forms
  Tab/Up/Down move, Left/Right change choice, Enter on a list opens a picker,
  Ctrl-S submit, Esc cancel";

const HELP_ZH: &str = "\
全局按键
  F1-F5               切换模块（系统、网络、磁盘、服务日志、PVE）
  Tab                 在列表和详情之间切换
  1-9 / [ ]           切换子菜单
  左右 (h/l)          在栏之间移动（菜单、列表、详情）
  上下 (j/k)          列表移动，PgUp/PgDn、Home/End
  Enter / m           对选中项打开操作菜单
  字母                直接执行菜单里的操作；大写 = 强制/危险版本
  空格                多选（批量操作）
  /                   过滤列表（每个页面单独记住），Esc 清除
  r / Ctrl-R          刷新一次（暂停时也可用）
  F7                  暂停 / 恢复全部刷新，包括实时曲线
  F6                  切换节点（集群）
  F8                  任务中心
  F10                 Shell（仅 root@pam）
  g                   全局菜单
  鼠标                点击模块、子菜单、列表行、操作按钮；右键或双击行打开菜单；滚轮滚动
  g 然后 o            登出（SSH 模式下是断开连接）
  Ctrl+Q              退出程序（仅 SSH / 终端里可用，tty1 上不能退出）

日志查看器
  空格 跟随/暂停，/ 搜索，n/N 下一个/上一个，& 只显示匹配行，
  p 优先级，t 时间范围，+ 加载更多，w 导出，Esc 关闭

表单
  Tab/上下 移动，左右 切换选项，在列表项上按 Enter 打开选择器，
  Ctrl-S 提交，Esc 取消";
