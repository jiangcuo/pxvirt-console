use crossterm::event::KeyCode;
use std::process::Command;
use std::sync::mpsc;
use std::thread;

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
    RestartService,
    ServiceConfirm(usize),
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct App {
    pub screen: Screen,
    pub main_index: usize,
    pub service_index: usize,
    pub should_exit: bool,

    // System info
    pub pve_version: String,
    pub hostname: String,

    // Login
    pub login_password: String,
    pub login_attempts: usize,
    pub login_checking: bool,
    pub login_rx: Option<mpsc::Receiver<bool>>,

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
    pub network_ifaces: Vec<NetIface>,
    pub network_index: usize,
    pub ping_target: String,
    pub ping_result: Option<String>,
    pub ping_running: bool,
    pub ping_rx: Option<mpsc::Receiver<String>>,

    // Network reset async
    pub network_reset_status: Option<String>,
    pub network_reset_rx: Option<mpsc::Receiver<Result<Vec<NetIface>, String>>>,

    // Message
    pub message: Option<(String, bool)>, // (msg, is_success)

    // Shell
    pub enter_shell: bool,
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
            login_password: String::new(),
            login_attempts: 0,
            login_checking: false,
            login_rx: None,
            password_field: 0,
            username: "root".to_string(),
            password: String::new(),
            password_confirm: String::new(),
            services: get_services(),
            service_restart_status: None,
            service_restart_rx: None,
            network_info: get_network_info(),
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
        }
    }

    pub fn logout(&mut self) {
        self.screen = Screen::Login;
        self.login_password.clear();
        self.login_attempts = 0;
        self.message = None;
    }

    pub fn poll_login(&mut self) {
        if let Some(rx) = &self.login_rx {
            if let Ok(result) = rx.try_recv() {
                self.login_checking = false;
                self.login_rx = None;
                if result {
                    self.screen = Screen::Main;
                    self.login_password.clear();
                    self.login_attempts = 0;
                } else {
                    self.login_attempts += 1;
                    self.login_password.clear();
                    self.message = Some((format!("Invalid password (attempt {})", self.login_attempts), false));
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
            Screen::NetworkReset | Screen::NetworkSelectIface | Screen::NetworkConfigIP => {
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
        match key {
            KeyCode::Char(c) => {
                self.login_password.push(c);
            }
            KeyCode::Backspace => {
                self.login_password.pop();
            }
            KeyCode::Enter => {
                if !self.login_checking {
                    let password = self.login_password.clone();
                    let (tx, rx) = mpsc::channel();
                    self.login_rx = Some(rx);
                    self.login_checking = true;
                    thread::spawn(move || {
                        let ok = matches!(pwcheck::pwcheck("root", &password), pwcheck::PwcheckResult::Ok);
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
                    self.username = "root".to_string();
                    self.password.clear();
                    self.password_confirm.clear();
                    self.password_field = 0;
                    self.screen = Screen::ResetPassword;
                }
                1 => {
                    self.network_info = get_network_info();
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
                self.screen = Screen::Main;
            }
            KeyCode::Tab | KeyCode::Down => {
                self.password_field = (self.password_field + 1) % 3;
            }
            KeyCode::Up => {
                self.password_field = if self.password_field == 0 { 2 } else { self.password_field - 1 };
            }
            KeyCode::Char(c) => match self.password_field {
                0 => self.username.push(c),
                1 => self.password.push(c),
                2 => self.password_confirm.push(c),
                _ => {}
            },
            KeyCode::Backspace => match self.password_field {
                0 => { self.username.pop(); }
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
        if self.password.is_empty() {
            self.message = Some(("Password cannot be empty".to_string(), false));
            return;
        }
        if self.password != self.password_confirm {
            self.message = Some(("Passwords do not match".to_string(), false));
            return;
        }

        // Use chpasswd to change password
        let input = format!("{}:{}", self.username, self.password);
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
                    Ok(ifaces) => {
                        self.network_ifaces = ifaces;
                        self.network_index = 0;
                        if self.network_ifaces.is_empty() {
                            self.message = Some(("No physical interfaces found".to_string(), false));
                            self.screen = Screen::Network;
                        } else {
                            self.screen = Screen::NetworkSelectIface;
                        }
                    }
                    Err(e) => {
                        self.message = Some((e, false));
                        self.screen = Screen::Network;
                    }
                }
            }
        }
    }

    fn handle_network_reset(&mut self, key: KeyCode) {
        match &self.screen {
            Screen::NetworkReset => {
                match key {
                    KeyCode::Esc | KeyCode::Left => {
                        self.screen = Screen::Network;
                    }
                    KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                        // Start async network reset
                        if self.network_reset_rx.is_none() {
                            let (tx, rx) = mpsc::channel();
                            self.network_reset_rx = Some(rx);
                            self.network_reset_status = Some("Backing up config...".to_string());
                            
                            thread::spawn(move || {
                                let result = do_network_reset_async();
                                let _ = tx.send(result);
                            });
                        }
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
                        // Move to IP config
                        self.network_info.ip.clear();
                        self.network_info.netmask = "255.255.255.0".to_string();
                        self.network_info.gateway.clear();
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
                        match self.password_field {
                            0 => self.network_info.ip.push(c),
                            1 => self.network_info.netmask.push(c),
                            2 => self.network_info.gateway.push(c),
                            _ => {}
                        }
                    }
                    KeyCode::Backspace => {
                        match self.password_field {
                            0 => { self.network_info.ip.pop(); }
                            1 => { self.network_info.netmask.pop(); }
                            2 => { self.network_info.gateway.pop(); }
                            _ => {}
                        }
                    }
                    KeyCode::Enter => {
                        // Apply config
                        if let Some(iface) = self.network_ifaces.get(self.network_index) {
                            match self.apply_network_config(&iface.name) {
                                Ok(_) => {
                                    self.message = Some(("Network configured successfully".to_string(), true));
                                    self.network_info = get_network_info();
                                    self.screen = Screen::Network;
                                }
                                Err(e) => {
                                    self.message = Some((format!("Failed: {}", e), false));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }


    fn apply_network_config(&self, iface_name: &str) -> Result<(), String> {
        use std::fs;

        let cidr = netmask_to_cidr_num(&self.network_info.netmask);
        let config = format!(
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
            self.network_info.ip,
            cidr,
            self.network_info.gateway,
            iface_name
        );

        fs::write("/etc/network/interfaces", &config)
            .map_err(|e| format!("Write failed: {}", e))?;

        // Apply with ifreload
        let status = Command::new("ifreload").arg("-a").status()
            .map_err(|e| format!("ifreload failed: {}", e))?;

        if status.success() {
            Ok(())
        } else {
            Err("ifreload returned error".to_string())
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

fn do_network_reset_async() -> Result<Vec<NetIface>, String> {
    use std::fs;
    use chrono::Local;

    // Step 1: Backup
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let backup_path = format!("/etc/network/interfaces.bak.{}", timestamp);
    fs::copy("/etc/network/interfaces", &backup_path)
        .map_err(|e| format!("Backup failed: {}", e))?;

    // Step 2: Write minimal config
    let minimal_config = "auto lo\niface lo inet loopback\n";
    fs::write("/etc/network/interfaces", minimal_config)
        .map_err(|e| format!("Write failed: {}", e))?;

    // Step 3: Clear interfaces.d
    if let Ok(entries) = fs::read_dir("/etc/network/interfaces.d") {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }

    // Step 4: Reload network
    Command::new("ifreload").arg("-a").status().ok();

    // Step 5: Detect interfaces
    let interfaces = detect_physical_interfaces();
    Ok(interfaces)
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
