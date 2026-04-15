use crossterm::event::KeyCode;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const ROOT_USERNAME: &str = "root";
pub const SESSION_IDLE_TIMEOUT_SECS: u64 = 60;
const LOGIN_LOCK_THRESHOLD: usize = 3;
const LOGIN_LOCKOUT_SECS: u64 = 60;

#[derive(Clone, PartialEq)]
pub enum Screen {
    Login,
    Main,
    ResetPassword,
    Network,
    NetworkPing,
    NetworkReset,
    NetworkSelectIface,
    NetworkConfigIP,
    NetworkReview,
    RestartService,
    ServiceConfirm(usize),
}

pub enum NetworkResetTaskResult {
    Interfaces(Vec<NetIface>),
    Applied,
}

pub const APP_VERSION: &str = env!("APP_VERSION");

pub struct App {
    pub screen: Screen,
    pub main_index: usize,
    pub service_index: usize,
    pub should_exit: bool,

    // System info
    pub pve_version: String,
    pub hostname: String,
    pub kernel_version: String,

    // Login
    pub login_password: String,
    pub login_attempts: usize,
    pub login_checking: bool,
    pub login_rx: Option<mpsc::Receiver<bool>>,
    login_locked_until: Option<Instant>,

    // Password form
    pub password_field: usize, // 0=username, 1=password, 2=confirm
    pub username: String,
    pub password: String,
    pub password_confirm: String,

    // Services
    pub services: Vec<Service>,
    pub service_restart_status: Option<String>,
    pub service_restart_rx: Option<mpsc::Receiver<Result<(), String>>>,

    // Network
    pub network_info: NetworkInfo,
    pub network_draft: NetworkInfo,
    pub network_ifaces: Vec<NetIface>,
    pub network_index: usize,
    pub ping_target: String,
    pub ping_result: Option<String>,
    pub ping_running: bool,
    pub ping_rx: Option<mpsc::Receiver<String>>,

    // Network reset async
    pub network_reset_status: Option<String>,
    pub network_reset_rx: Option<mpsc::Receiver<Result<NetworkResetTaskResult, String>>>,

    // Message
    pub message: Option<(String, bool)>, // (msg, is_success)

    // Shell
    pub enter_shell: bool,
    last_activity_at: Instant,
}

pub struct Service {
    pub name: String,
    pub display: String,
    pub running: bool,
}

#[derive(Clone, Default)]
pub struct NetworkInfo {
    pub ip: String,
    pub netmask: String,
    pub gateway: String,
    pub iface: String,
}

#[derive(Clone)]
pub struct NetIface {
    pub name: String,
    pub mac: String,
    pub has_carrier: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            screen: Screen::Login,
            main_index: 0,
            service_index: 0,
            should_exit: false,
            pve_version: get_pve_version(),
            hostname: get_hostname(),
            kernel_version: get_kernel_version(),
            login_password: String::new(),
            login_attempts: 0,
            login_checking: false,
            login_rx: None,
            login_locked_until: None,
            password_field: 0,
            username: ROOT_USERNAME.to_string(),
            password: String::new(),
            password_confirm: String::new(),
            services: get_services(),
            service_restart_status: None,
            service_restart_rx: None,
            network_info: get_network_info(),
            network_draft: NetworkInfo::default(),
            network_ifaces: Vec::new(),
            network_index: 0,
            ping_target: String::new(),
            ping_result: None,
            ping_running: false,
            ping_rx: None,
            network_reset_status: None,
            network_reset_rx: None,
            message: None,
            enter_shell: false,
            last_activity_at: Instant::now(),
        }
    }

    pub fn logout(&mut self) {
        self.clear_secret_fields();
        self.screen = Screen::Login;
        self.main_index = 0;
        self.service_index = 0;
        self.login_password.clear();
        self.login_attempts = 0;
        self.login_checking = false;
        self.login_rx = None;
        self.login_locked_until = None;
        self.service_restart_status = None;
        self.service_restart_rx = None;
        self.network_draft = NetworkInfo::default();
        self.network_ifaces.clear();
        self.network_index = 0;
        self.ping_result = None;
        self.ping_running = false;
        self.ping_rx = None;
        self.network_reset_status = None;
        self.network_reset_rx = None;
        self.enter_shell = false;
        self.message = None;
        self.last_activity_at = Instant::now();
    }

    pub fn note_activity(&mut self) {
        self.last_activity_at = Instant::now();
    }

    pub fn poll_session_timeout(&mut self) {
        if matches!(self.screen, Screen::Login) {
            return;
        }

        if self.last_activity_at.elapsed() >= Duration::from_secs(SESSION_IDLE_TIMEOUT_SECS) {
            self.logout();
        }
    }

    pub fn login_lock_remaining_secs(&self) -> Option<u64> {
        let now = Instant::now();
        self.login_locked_until
            .and_then(|until| until.checked_duration_since(now))
            .map(|remaining| remaining.as_secs().max(1))
    }

    pub fn poll_login(&mut self) {
        if let Some(rx) = &self.login_rx {
            if let Ok(result) = rx.try_recv() {
                self.login_checking = false;
                self.login_rx = None;
                if result {
                    self.screen = Screen::Main;
                    self.login_attempts = 0;
                    self.login_locked_until = None;
                    self.note_activity();
                } else {
                    self.login_attempts = self.login_attempts.saturating_add(1);
                    if self.login_attempts >= LOGIN_LOCK_THRESHOLD {
                        self.login_locked_until =
                            Some(Instant::now() + Duration::from_secs(LOGIN_LOCKOUT_SECS));
                        self.message = Some((
                            format!(
                                "Invalid password (attempt {}). Login locked for {}s",
                                self.login_attempts, LOGIN_LOCKOUT_SECS
                            ),
                            false,
                        ));
                        self.login_attempts = 0;
                    } else {
                        let remaining = LOGIN_LOCK_THRESHOLD - self.login_attempts;
                        self.message = Some((
                            format!(
                                "Invalid password (attempt {}/{}). {} more wrong attempt(s) will lock login for {}s",
                                self.login_attempts,
                                LOGIN_LOCK_THRESHOLD,
                                remaining,
                                LOGIN_LOCKOUT_SECS
                            ),
                            false,
                        ));
                    }
                }
            }
        }
    }

    pub fn main_menu_items() -> Vec<&'static str> {
        vec![
            "Reset Password",
            "Reset Network",
            "Restart Service",
            "Enter Shell",
        ]
    }

    pub fn pending_network_config_preview(&self) -> Result<String, String> {
        if self.network_draft.iface.is_empty() {
            return Err("No interface selected".to_string());
        }

        validate_network_config(&self.network_draft)?;
        Ok(build_network_config(&self.network_draft.iface, &self.network_draft))
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        // If message is showing, any key clears it and returns
        if self.message.is_some() {
            self.message = None;
            return;
        }

        match &self.screen {
            Screen::Login => self.handle_login(key),
            Screen::Main => self.handle_main(key),
            Screen::ResetPassword => self.handle_password(key),
            Screen::Network => self.handle_network(key),
            Screen::NetworkPing => self.handle_network_ping(key),
            Screen::NetworkReset
            | Screen::NetworkSelectIface
            | Screen::NetworkConfigIP
            | Screen::NetworkReview => {
                self.handle_network_reset(key);
            }
            Screen::RestartService => self.handle_service_list(key),
            Screen::ServiceConfirm(idx) => {
                let idx = *idx;
                self.handle_service_confirm(key, idx);
            }
        }
    }

    fn handle_login(&mut self, key: KeyCode) {
        if let Some(remaining) = self.login_lock_remaining_secs() {
            match key {
                KeyCode::Enter => {
                    self.message = Some((
                        format!("Too many failed attempts. Login is locked for {}s", remaining),
                        false,
                    ));
                }
                KeyCode::Char(_) | KeyCode::Backspace => {}
                #[cfg(debug_assertions)]
                KeyCode::Esc => {
                    self.should_exit = true;
                }
                _ => {}
            }
            return;
        }

        match key {
            KeyCode::Char(c) => {
                self.login_password.push(c);
            }
            KeyCode::Backspace => {
                self.login_password.pop();
            }
            KeyCode::Enter => {
                if !self.login_checking {
                    if let Some(remaining) = self.login_lock_remaining_secs() {
                        self.message = Some((
                            format!("Too many failed attempts. Try again in {}s", remaining),
                            false,
                        ));
                        return;
                    }

                    let mut password = std::mem::take(&mut self.login_password);
                    if password.is_empty() {
                        self.message = Some(("Password cannot be empty".to_string(), false));
                        return;
                    }

                    let (tx, rx) = mpsc::channel();
                    self.login_rx = Some(rx);
                    self.login_checking = true;
                    thread::spawn(move || {
                        let ok = matches!(pwcheck::pwcheck("root", &password), pwcheck::PwcheckResult::Ok);
                        scrub_string(&mut password);
                        let _ = tx.send(ok);
                    });
                }
            }
            #[cfg(debug_assertions)]
            KeyCode::Esc => {
                self.should_exit = true;
            }
            _ => {}
        }
    }

    fn handle_main(&mut self, key: KeyCode) {
        match key {
            KeyCode::Up => {
                if self.main_index > 0 {
                    self.main_index -= 1;
                }
            }
            KeyCode::Down => {
                let max = Self::main_menu_items().len() - 1;
                if self.main_index < max {
                    self.main_index += 1;
                }
            }
            KeyCode::Enter | KeyCode::Right => match self.main_index {
                0 => {
                    self.username = ROOT_USERNAME.to_string();
                    self.clear_password_form();
                    self.screen = Screen::ResetPassword;
                }
                1 => {
                    self.network_info = get_network_info();
                    self.network_draft = NetworkInfo::default();
                    self.network_ifaces.clear();
                    self.network_index = 0;
                    self.ping_target = self.network_info.gateway.clone();
                    self.screen = Screen::Network;
                }
                2 => {
                    self.services = get_services();
                    self.service_index = 0;
                    self.screen = Screen::RestartService;
                }
                3 => {
                    self.enter_shell = true;
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn handle_password(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc | KeyCode::Left => {
                self.clear_password_form();
                self.screen = Screen::Main;
            }
            KeyCode::Tab | KeyCode::Down => {
                self.password_field = (self.password_field + 1) % 3;
            }
            KeyCode::Up => {
                self.password_field = if self.password_field == 0 { 2 } else { self.password_field - 1 };
            }
            KeyCode::Char(c) => match self.password_field {
                0 => {}
                1 => self.password.push(c),
                2 => self.password_confirm.push(c),
                _ => {}
            },
            KeyCode::Backspace => match self.password_field {
                0 => {}
                1 => { self.password.pop(); }
                2 => { self.password_confirm.pop(); }
                _ => {}
            },
            KeyCode::Enter => {
                self.submit_password();
            }
            _ => {}
        }
    }

    fn submit_password(&mut self) {
        self.username = ROOT_USERNAME.to_string();
        let mut password = std::mem::take(&mut self.password);
        let mut password_confirm = std::mem::take(&mut self.password_confirm);

        if password.is_empty() {
            scrub_string(&mut password_confirm);
            self.message = Some(("Password cannot be empty".to_string(), false));
            return;
        }
        if password != password_confirm {
            scrub_string(&mut password);
            scrub_string(&mut password_confirm);
            self.message = Some(("Passwords do not match".to_string(), false));
            return;
        }
        scrub_string(&mut password_confirm);

        // Use chpasswd to change password
        let mut input = format!("{}:{}", self.username, password);
        let result = Command::new("chpasswd")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(input.as_bytes())?;
                }
                child.wait()
            });
        scrub_string(&mut input);
        scrub_string(&mut password);

        match result {
            Ok(status) if status.success() => {
                self.message = Some((format!("Password for '{}' has been reset", self.username), true));
                self.screen = Screen::Main;
            }
            _ => {
                self.message = Some(("Failed to reset password (need root?)".to_string(), false));
            }
        }
    }

    fn handle_service_list(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc | KeyCode::Left => {
                self.screen = Screen::Main;
            }
            KeyCode::Up => {
                if self.service_index > 0 {
                    self.service_index -= 1;
                }
            }
            KeyCode::Down => {
                // +1 for "Restart All" option
                if self.service_index < self.services.len() {
                    self.service_index += 1;
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                self.screen = Screen::ServiceConfirm(self.service_index);
            }
            _ => {}
        }
    }

    fn handle_network(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc | KeyCode::Left => {
                self.screen = Screen::Main;
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                self.screen = Screen::NetworkPing;
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.screen = Screen::NetworkReset;
            }
            _ => {}
        }
    }

    fn handle_network_ping(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc | KeyCode::Left => {
                self.ping_result = None;
                self.screen = Screen::Network;
            }
            KeyCode::Char(c) => {
                if !self.ping_running {
                    self.ping_target.push(c);
                }
            }
            KeyCode::Backspace => {
                if !self.ping_running {
                    self.ping_target.pop();
                }
            }
            KeyCode::Enter => {
                if !self.ping_running && !self.ping_target.is_empty() {
                    let target = self.ping_target.clone();
                    let (tx, rx) = mpsc::channel();
                    self.ping_rx = Some(rx);
                    self.ping_running = true;
                    self.ping_result = Some("Pinging...".to_string());
                    thread::spawn(move || {
                        let output = Command::new("ping")
                            .args(["-c", "4", &target])
                            .output();
                        let result = match output {
                            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string()
                                + &String::from_utf8_lossy(&o.stderr),
                            Err(e) => format!("Error: {}", e),
                        };
                        let _ = tx.send(result);
                    });
                }
            }
            _ => {}
        }
    }

    pub fn poll_ping(&mut self) {
        if let Some(rx) = &self.ping_rx {
            if let Ok(result) = rx.try_recv() {
                self.ping_running = false;
                self.ping_rx = None;
                self.ping_result = Some(result);
            }
        }
    }

    pub fn poll_service_restart(&mut self) {
        if let Some(rx) = &self.service_restart_rx {
            if let Ok(result) = rx.try_recv() {
                self.service_restart_rx = None;
                self.service_restart_status = None;
                self.services = get_services();
                match result {
                    Ok(_) => {
                        self.message = Some(("Service(s) restarted successfully".to_string(), true));
                    }
                    Err(e) => {
                        self.message = Some((e, false));
                    }
                }
                self.screen = Screen::RestartService;
            }
        }
    }

    pub fn poll_network_reset(&mut self) {
        if let Some(rx) = &self.network_reset_rx {
            if let Ok(result) = rx.try_recv() {
                self.network_reset_rx = None;
                self.network_reset_status = None;
                match result {
                    Ok(NetworkResetTaskResult::Interfaces(ifaces)) => {
                        self.network_ifaces = ifaces;
                        self.network_index = 0;
                        if self.network_ifaces.is_empty() {
                            self.message = Some(("No physical interfaces found".to_string(), false));
                            self.screen = Screen::Network;
                        } else {
                            self.screen = Screen::NetworkSelectIface;
                        }
                    }
                    Ok(NetworkResetTaskResult::Applied) => {
                        self.network_info = get_network_info();
                        self.ping_target = self.network_info.gateway.clone();
                        self.network_draft = NetworkInfo::default();
                        self.network_ifaces.clear();
                        self.network_index = 0;
                        self.message = Some(("Network configured successfully".to_string(), true));
                        self.screen = Screen::Network;
                    }
                    Err(e) => {
                        self.message = Some((e, false));
                        if matches!(self.screen, Screen::NetworkReset) {
                            self.screen = Screen::Network;
                        }
                    }
                }
            }
        }
    }

    fn handle_network_reset(&mut self, key: KeyCode) {
        match &self.screen {
            Screen::NetworkReset => {
                if self.network_reset_rx.is_some() {
                    return;
                }

                match key {
                    KeyCode::Esc | KeyCode::Left => {
                        self.screen = Screen::Network;
                    }
                    KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                        let (tx, rx) = mpsc::channel();
                        self.network_reset_rx = Some(rx);
                        self.network_reset_status = Some("Detecting physical interfaces...".to_string());
                        self.network_draft = NetworkInfo::default();

                        thread::spawn(move || {
                            let result = Ok(NetworkResetTaskResult::Interfaces(detect_physical_interfaces()));
                            let _ = tx.send(result);
                        });
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        self.screen = Screen::Network;
                    }
                    _ => {}
                }
            }
            Screen::NetworkSelectIface => {
                match key {
                    KeyCode::Esc | KeyCode::Left => {
                        self.screen = Screen::Network;
                    }
                    KeyCode::Up => {
                        if self.network_index > 0 {
                            self.network_index -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if self.network_index < self.network_ifaces.len().saturating_sub(1) {
                            self.network_index += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(iface) = self.network_ifaces.get(self.network_index) {
                            self.network_draft = NetworkInfo {
                                ip: String::new(),
                                netmask: "255.255.255.0".to_string(),
                                gateway: self.network_info.gateway.clone(),
                                iface: iface.name.clone(),
                            };
                        }
                        self.password_field = 0; // reuse for field selection
                        self.screen = Screen::NetworkConfigIP;
                    }
                    _ => {}
                }
            }
            Screen::NetworkConfigIP => {
                match key {
                    KeyCode::Esc | KeyCode::Left => {
                        self.screen = Screen::NetworkSelectIface;
                    }
                    KeyCode::Tab | KeyCode::Down => {
                        self.password_field = (self.password_field + 1) % 3;
                    }
                    KeyCode::Up => {
                        self.password_field = if self.password_field == 0 { 2 } else { self.password_field - 1 };
                    }
                    KeyCode::Char(c) => {
                        if is_network_input_char(c) {
                            match self.password_field {
                                0 => self.network_draft.ip.push(c),
                                1 => self.network_draft.netmask.push(c),
                                2 => self.network_draft.gateway.push(c),
                                _ => {}
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        match self.password_field {
                            0 => { self.network_draft.ip.pop(); }
                            1 => { self.network_draft.netmask.pop(); }
                            2 => { self.network_draft.gateway.pop(); }
                            _ => {}
                        }
                    }
                    KeyCode::Enter => {
                        match validate_network_config(&self.network_draft) {
                            Ok(_) => {
                                self.screen = Screen::NetworkReview;
                            }
                            Err(e) => {
                                self.message = Some((format!("Failed: {}", e), false));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Screen::NetworkReview => {
                if self.network_reset_rx.is_some() {
                    return;
                }

                match key {
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('n') | KeyCode::Char('N') => {
                        self.screen = Screen::NetworkConfigIP;
                    }
                    KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                        match self.pending_network_config_preview() {
                            Ok(_) => {
                                let draft = self.network_draft.clone();
                                let iface_name = draft.iface.clone();
                                let (tx, rx) = mpsc::channel();
                                self.network_reset_rx = Some(rx);
                                self.network_reset_status =
                                    Some("Applying network configuration...".to_string());

                                thread::spawn(move || {
                                    let result = apply_network_config(&iface_name, &draft)
                                        .map(|_| NetworkResetTaskResult::Applied);
                                    let _ = tx.send(result);
                                });
                            }
                            Err(e) => {
                                self.message = Some((format!("Failed: {}", e), false));
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn handle_service_confirm(&mut self, key: KeyCode, idx: usize) {
        // Don't process keys while restarting
        if self.service_restart_rx.is_some() {
            return;
        }

        match key {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.screen = Screen::RestartService;
            }
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                // idx == services.len() means "Restart All"
                let services_to_restart: Vec<String> = if idx >= self.services.len() {
                    self.services.iter().map(|s| s.name.clone()).collect()
                } else if let Some(service) = self.services.get(idx) {
                    vec![service.name.clone()]
                } else {
                    return;
                };

                let (tx, rx) = mpsc::channel();
                self.service_restart_rx = Some(rx);
                self.service_restart_status = Some("Restarting...".to_string());

                thread::spawn(move || {
                    use std::process::Stdio;
                    let mut failed = Vec::new();
                    for name in &services_to_restart {
                        let status = Command::new("systemctl")
                            .args(["restart", name])
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                        if !status.map(|s| s.success()).unwrap_or(false) {
                            failed.push(name.clone());
                        }
                    }
                    if failed.is_empty() {
                        let _ = tx.send(Ok(()));
                    } else {
                        let _ = tx.send(Err(format!("Failed: {}", failed.join(", "))));
                    }
                });
            }
            _ => {}
        }
    }
}

fn scrub_string(value: &mut String) {
    let mut bytes = std::mem::take(value).into_bytes();
    bytes.fill(0);
}

fn build_network_config(iface_name: &str, info: &NetworkInfo) -> String {
    let cidr = netmask_to_cidr_num(&info.netmask);
    format!(
        r#"auto lo
iface lo inet loopback

auto {}
iface {} inet manual

auto vmbr0
iface vmbr0 inet static
    address {}/{}
    gateway {}
    bridge-ports {}
    bridge-stp off
    bridge-fd 0
"#,
        iface_name,
        iface_name,
        info.ip,
        cidr,
        info.gateway,
        iface_name
    )
}

fn backup_interfaces_dir(timestamp: &str) -> Result<Option<String>, String> {
    use std::fs;

    let interfaces_d = std::path::Path::new("/etc/network/interfaces.d");
    if !interfaces_d.exists() {
        return Ok(None);
    }

    let backup_dir = format!("/etc/network/interfaces.d.bak.{}", timestamp);
    fs::create_dir_all(&backup_dir).map_err(|e| format!("interfaces.d backup dir failed: {}", e))?;

    for entry in fs::read_dir(interfaces_d).map_err(|e| format!("Read interfaces.d failed: {}", e))? {
        let entry = entry.map_err(|e| format!("Read interfaces.d entry failed: {}", e))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("Read interfaces.d entry type failed: {}", e))?;
        if file_type.is_dir() {
            return Err("interfaces.d contains a directory; refusing automatic reset".to_string());
        }

        let target = std::path::Path::new(&backup_dir).join(entry.file_name());
        fs::copy(entry.path(), &target)
            .map_err(|e| format!("interfaces.d backup copy failed: {}", e))?;
    }

    Ok(Some(backup_dir))
}

fn clear_interfaces_dir() -> Result<(), String> {
    use std::fs;

    let interfaces_d = std::path::Path::new("/etc/network/interfaces.d");
    if !interfaces_d.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(interfaces_d).map_err(|e| format!("Read interfaces.d failed: {}", e))? {
        let entry = entry.map_err(|e| format!("Read interfaces.d entry failed: {}", e))?;
        let path = entry.path();
        if path.is_dir() {
            return Err("interfaces.d contains a directory; refusing automatic reset".to_string());
        }
        fs::remove_file(&path).map_err(|e| format!("Clear interfaces.d failed: {}", e))?;
    }

    Ok(())
}

fn restore_interfaces_dir(backup_dir: Option<&str>) -> Result<(), String> {
    use std::fs;

    let interfaces_d = std::path::Path::new("/etc/network/interfaces.d");
    fs::create_dir_all(interfaces_d)
        .map_err(|e| format!("Recreate interfaces.d failed: {}", e))?;
    clear_interfaces_dir()?;

    if let Some(backup_dir) = backup_dir {
        for entry in fs::read_dir(backup_dir).map_err(|e| format!("Read interfaces.d backup failed: {}", e))? {
            let entry = entry.map_err(|e| format!("Read interfaces.d backup entry failed: {}", e))?;
            let target = interfaces_d.join(entry.file_name());
            fs::copy(entry.path(), target)
                .map_err(|e| format!("Restore interfaces.d failed: {}", e))?;
        }
    }

    Ok(())
}

fn apply_network_config(iface_name: &str, info: &NetworkInfo) -> Result<(), String> {
    use chrono::Local;
    use std::fs;

    validate_network_config(info)?;

    let config = build_network_config(iface_name, info);
    let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
    let backup_path = format!("/etc/network/interfaces.bak.{}", timestamp);

    fs::copy("/etc/network/interfaces", &backup_path)
        .map_err(|e| format!("Backup failed: {}", e))?;
    let interfaces_d_backup = backup_interfaces_dir(&timestamp)?;

    let apply_result = (|| -> Result<(), String> {
        fs::write("/etc/network/interfaces", &config)
            .map_err(|e| format!("Write failed: {}", e))?;
        clear_interfaces_dir()?;

        let status = Command::new("ifreload")
            .arg("-a")
            .status()
            .map_err(|e| format!("ifreload failed: {}", e))?;

        if status.success() {
            Ok(())
        } else {
            Err("ifreload returned error".to_string())
        }
    })();

    if apply_result.is_ok() {
        return Ok(());
    }

    let apply_error = apply_result.err().unwrap_or_else(|| "Unknown error".to_string());
    let rollback_interfaces =
        fs::copy(&backup_path, "/etc/network/interfaces").map_err(|e| format!("Rollback failed: {}", e));
    let rollback_dir = restore_interfaces_dir(interfaces_d_backup.as_deref());

    if rollback_interfaces.is_ok() && rollback_dir.is_ok() {
        let _ = Command::new("ifreload").arg("-a").status();
        Err(format!(
            "{}; configuration was rolled back",
            apply_error
        ))
    } else {
        let mut details = vec![apply_error];
        if let Err(e) = rollback_interfaces {
            details.push(e);
        }
        if let Err(e) = rollback_dir {
            details.push(e);
        }
        Err(details.join("; "))
    }
}

fn get_services() -> Vec<Service> {
    let service_names = [
        ("pve-cluster", "PVE Cluster"),
        ("pvedaemon", "PVE Daemon"),
        ("pveproxy", "PVE Proxy (Web UI)"),
        ("pvestatd", "PVE Status Daemon"),
        ("ssh", "SSH Server"),
    ];

    service_names
        .iter()
        .map(|(name, display)| {
            let running = Command::new("systemctl")
                .args(["is-active", "--quiet", name])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

            Service {
                name: name.to_string(),
                display: display.to_string(),
                running,
            }
        })
        .collect()
}

fn get_pve_version() -> String {
    Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", "pve-manager"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "Unknown".to_string())
}

fn get_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn get_kernel_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn get_network_info() -> NetworkInfo {
    let mut info = NetworkInfo::default();

    // Get default gateway and interface
    if let Ok(output) = Command::new("ip").args(["route", "show", "default"]).output() {
        let line = String::from_utf8_lossy(&output.stdout);
        // default via 192.168.1.1 dev vmbr0
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5 {
            info.gateway = parts.get(2).unwrap_or(&"").to_string();
            info.iface = parts.get(4).unwrap_or(&"").to_string();
        }
    }

    // Get IP address of the interface
    if !info.iface.is_empty() {
        if let Ok(output) = Command::new("ip")
            .args(["-o", "-4", "addr", "show", &info.iface])
            .output()
        {
            let line = String::from_utf8_lossy(&output.stdout);
            // 2: vmbr0 inet 192.168.1.100/24 brd ...
            for part in line.split_whitespace() {
                if part.contains('/') && part.contains('.') {
                    if let Some((ip, mask)) = part.split_once('/') {
                        info.ip = ip.to_string();
                        // Convert CIDR to netmask
                        info.netmask = cidr_to_netmask(mask.parse().unwrap_or(24));
                    }
                    break;
                }
            }
        }
    }

    info
}

fn cidr_to_netmask(cidr: u8) -> String {
    let mask: u32 = if cidr >= 32 { !0 } else { !0 << (32 - cidr) };
    format!(
        "{}.{}.{}.{}",
        (mask >> 24) & 0xFF,
        (mask >> 16) & 0xFF,
        (mask >> 8) & 0xFF,
        mask & 0xFF
    )
}

fn netmask_to_cidr_num(netmask: &str) -> u8 {
    netmask
        .split('.')
        .filter_map(|s| s.parse::<u8>().ok())
        .map(|b| b.count_ones() as u8)
        .sum()
}

fn is_network_input_char(c: char) -> bool {
    c.is_ascii_digit() || c == '.'
}

fn is_valid_netmask(value: &str) -> bool {
    let Ok(addr) = value.parse::<std::net::Ipv4Addr>() else {
        return false;
    };

    let mask = u32::from(addr);
    let inverted = !mask;
    (inverted & (inverted + 1)) == 0
}

fn parse_ipv4_field(value: &str, field_name: &str) -> Result<std::net::Ipv4Addr, String> {
    value
        .parse::<std::net::Ipv4Addr>()
        .map_err(|_| format!("Invalid {}", field_name))
}

fn validate_network_config(info: &NetworkInfo) -> Result<(), String> {
    let ip = parse_ipv4_field(&info.ip, "IP address")?;
    let gateway = parse_ipv4_field(&info.gateway, "gateway")?;

    if !is_valid_netmask(&info.netmask) {
        return Err("Invalid netmask".to_string());
    }

    let netmask = parse_ipv4_field(&info.netmask, "netmask")?;
    let mask = u32::from(netmask);
    let prefix = mask.count_ones();

    if prefix == 0 || prefix >= 31 {
        return Err("Netmask must be between /1 and /30".to_string());
    }

    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return Err("IP address must be a usable unicast host address".to_string());
    }

    if gateway.is_unspecified() || gateway.is_loopback() || gateway.is_multicast() {
        return Err("Gateway must be a usable unicast host address".to_string());
    }

    let ip_u32 = u32::from(ip);
    let gateway_u32 = u32::from(gateway);
    let network = ip_u32 & mask;
    let broadcast = network | !mask;

    if ip_u32 == network || ip_u32 == broadcast {
        return Err("IP address cannot be the network or broadcast address".to_string());
    }

    if gateway_u32 == network || gateway_u32 == broadcast {
        return Err("Gateway cannot be the network or broadcast address".to_string());
    }

    if ip == gateway {
        return Err("IP address cannot be the same as gateway".to_string());
    }

    if (gateway_u32 & mask) != network {
        return Err("Gateway must be in the same subnet as the IP address".to_string());
    }

    Ok(())
}

fn detect_physical_interfaces() -> Vec<NetIface> {
    let mut interfaces = Vec::new();

    // List all interfaces from /sys/class/net
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            
            // Skip virtual interfaces
            if name.starts_with("lo") 
                || name.starts_with("veth")
                || name.starts_with("vmbr")
                || name.starts_with("tap")
                || name.starts_with("fwbr")
                || name.starts_with("fwpr")
                || name.starts_with("fwln")
            {
                continue;
            }

            // Check if it's a physical device (has /device symlink)
            let device_path = format!("/sys/class/net/{}/device", name);
            if !std::path::Path::new(&device_path).exists() {
                continue;
            }

            // Get MAC address
            let mac = std::fs::read_to_string(format!("/sys/class/net/{}/address", name))
                .unwrap_or_default()
                .trim()
                .to_string();

            // Bring up interface to check carrier
            let _ = Command::new("ip")
                .args(["link", "set", &name, "up"])
                .status();

            // Small delay for carrier detection
            std::thread::sleep(std::time::Duration::from_millis(500));

            // Check carrier
            let carrier = std::fs::read_to_string(format!("/sys/class/net/{}/carrier", name))
                .unwrap_or_default()
                .trim()
                .parse::<u8>()
                .unwrap_or(0);

            interfaces.push(NetIface {
                name,
                mac,
                has_carrier: carrier == 1,
            });
        }
    }

    interfaces
}

impl App {
    fn clear_password_form(&mut self) {
        scrub_string(&mut self.password);
        scrub_string(&mut self.password_confirm);
        self.password_field = 0;
    }

    fn clear_secret_fields(&mut self) {
        scrub_string(&mut self.login_password);
        self.clear_password_form();
    }
}
