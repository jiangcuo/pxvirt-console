//! /etc/pxvirt-console/config.toml

use serde::Deserialize;

pub const CONFIG_PATH: &str = "/etc/pxvirt-console/config.toml";
const SYSTEM_KNOWN_HOSTS: &str = "/etc/pxvirt-console/known_hosts";

/// Per-user directory (~/.config/pxvirt-console) for non-root use as a client.
fn user_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("pxvirt-console"))
}

/// Writable state file (acknowledged health findings...): /etc for root, ~/.config otherwise.
pub fn state_path(name: &str) -> String {
    if crate::util::is_root_process() {
        return format!("/etc/pxvirt-console/{name}");
    }
    user_dir().map(|d| d.join(name).to_string_lossy().to_string()).unwrap_or_else(|| format!("/etc/pxvirt-console/{name}"))
}

/// Per-user file (saved connections): always ~/.config/pxvirt-console, also for root.
pub fn user_path(name: &str) -> Option<std::path::PathBuf> {
    user_dir().map(|d| d.join(name))
}

/// Trusted certificate fingerprints: /etc for root, ~/.config for normal users.
pub fn known_hosts_path() -> String {
    if crate::util::is_root_process() {
        return SYSTEM_KNOWN_HOSTS.to_string();
    }
    user_dir().map(|d| d.join("known_hosts").to_string_lossy().to_string()).unwrap_or_else(|| SYSTEM_KNOWN_HOSTS.to_string())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Logged-in session is locked after this many seconds without a key press.
    pub idle_timeout_secs: u64,
    /// "auto", "en" or "zh". tty1 (TERM=linux) is always English.
    pub lang: String,
    /// Auto refresh interval of the active view.
    pub refresh_secs: u64,
    /// Show host info and live metrics before login (like ESXi DCUI).
    pub prelogin_dashboard: bool,
    /// Default realm on the login screen.
    pub default_realm: String,
    /// Graphical console: "auto", "sdl", "terminal", "novnc".
    pub console_display: String,
    /// Max frames per second for the terminal VNC renderer.
    pub console_fps: u32,
    /// SSH user for remote mode host commands (key auth only).
    pub ssh_user: String,
    /// Health check thresholds ([health] section).
    pub health: HealthCfg,
}

/// Thresholds of the health checks (percent, °C, seconds, days, ms).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HealthCfg {
    pub fs_warn: f64,
    pub fs_crit: f64,
    pub disk_temp_warn: f64,
    pub disk_temp_crit: f64,
    pub nvme_temp_warn: f64,
    pub nvme_temp_crit: f64,
    pub cpu_temp_warn: f64,
    pub cpu_temp_crit: f64,
    pub nvme_wear_warn: f64,
    pub lvm_thin_warn: f64,
    pub lvm_thin_crit: f64,
    pub zfs_cap_warn: f64,
    pub zfs_cap_crit: f64,
    pub zfs_scrub_days: f64,
    pub storage_warn: f64,
    pub storage_crit: f64,
    pub time_offset_warn: f64,
    pub time_offset_crit: f64,
    pub cert_days_warn: f64,
    pub cert_days_crit: f64,
    pub await_warn_ms: f64,
    pub mem_pressure_warn: f64,
    pub swap_warn: f64,
    /// how long a growing error counter stays reported (hours)
    pub growth_hours: f64,
}

impl Default for HealthCfg {
    fn default() -> Self {
        Self {
            fs_warn: 85.0,
            fs_crit: 95.0,
            disk_temp_warn: 55.0,
            disk_temp_crit: 65.0,
            nvme_temp_warn: 70.0,
            nvme_temp_crit: 80.0,
            cpu_temp_warn: 90.0,
            cpu_temp_crit: 100.0,
            nvme_wear_warn: 90.0,
            lvm_thin_warn: 90.0,
            lvm_thin_crit: 95.0,
            zfs_cap_warn: 80.0,
            zfs_cap_crit: 90.0,
            zfs_scrub_days: 35.0,
            storage_warn: 90.0,
            storage_crit: 95.0,
            time_offset_warn: 1.0,
            time_offset_crit: 5.0,
            cert_days_warn: 30.0,
            cert_days_crit: 7.0,
            await_warn_ms: 500.0,
            mem_pressure_warn: 10.0,
            swap_warn: 80.0,
            growth_hours: 24.0,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 15 * 60,
            lang: "auto".into(),
            refresh_secs: 5,
            prelogin_dashboard: true,
            default_realm: "pam".into(),
            console_display: "auto".into(),
            console_fps: 10,
            ssh_user: "root".into(),
            health: HealthCfg::default(),
        }
    }
}

impl Config {
    /// ~/.config/pxvirt-console/config.toml (if present) overrides /etc/pxvirt-console/config.toml.
    pub fn load() -> Self {
        let user = user_dir().map(|d| d.join("config.toml")).filter(|p| p.exists() && !crate::util::is_root_process());
        let path = user.map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| CONFIG_PATH.to_string());
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                eprintln!("pxvirt-console: ignoring invalid {path}: {e}");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }
}
