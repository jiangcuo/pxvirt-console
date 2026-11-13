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
