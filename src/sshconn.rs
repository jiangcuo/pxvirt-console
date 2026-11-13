//! SSH mode: one OpenSSH ControlMaster connection carries everything.
//!
//! - the PVE API through a local port forward (127.0.0.1:<random> -> node:127.0.0.1:8006)
//! - host commands, shells, guest consoles and the live metrics stream (multiplexed sessions)
//!
//! The master is started before the TUI so host-key and password prompts work normally.

use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::exec::Exec;

/// Extra ssh options used by every `Exec::Ssh` / `Exec::SshVia` in SSH mode
/// (control socket, port). Empty in local mode.
static SSH_OPTS: OnceLock<Vec<String>> = OnceLock::new();

pub fn ssh_opts() -> &'static [String] {
    SSH_OPTS.get().map(|v| v.as_slice()).unwrap_or(&[])
}

pub struct SshConn {
    /// user@host (without port)
    pub target: String,
    pub user: String,
    pub host: String,
    port: Option<u16>,
    socket: String,
    /// local end of the API forward
    pub api_port: u16,
    /// short hostname of the node = PVE node name
    pub node: String,
}

/// "root@host", "root@host:2222", "host" (user defaults to root)
pub fn parse_target(s: &str) -> (String, String, Option<u16>) {
    let (user, rest) = match s.split_once('@') {
        Some((u, r)) => (u.to_string(), r.to_string()),
        None => ("root".to_string(), s.to_string()),
    };
    let (host, port) = if rest.starts_with('[') {
        // [v6]:port
        match rest.split_once("]:") {
            Some((h, p)) => (h.trim_start_matches('[').to_string(), p.parse().ok()),
            None => (rest.trim_matches(|c| c == '[' || c == ']').to_string(), None),
        }
    } else if rest.matches(':').count() == 1 {
        let (h, p) = rest.split_once(':').unwrap();
        (h.to_string(), p.parse().ok())
    } else {
        (rest, None)
    };
    (user, host, port)
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

fn socket_path() -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR").ok().filter(|d| std::path::Path::new(d).is_dir()).unwrap_or_else(|| "/tmp".into());
    format!("{dir}/pxvirt-console-{}-{}.sock", unsafe { libc::getuid() }, std::process::id())
}

impl SshConn {
    /// Start the master connection (interactive: may ask for host key / password).
    pub fn connect(spec: &str) -> Result<Self> {
        let (user, host, port) = parse_target(spec);
        let target = format!("{user}@{host}");
        let socket = socket_path();
        let api_port = free_port()?;
        let mut cmd = Command::new("ssh");
        cmd.args(["-M", "-S", &socket, "-fN", "-o", "ControlPersist=yes", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=15"]);
        cmd.arg("-L").arg(format!("127.0.0.1:{api_port}:127.0.0.1:8006"));
        if let Some(p) = port {
            cmd.arg("-p").arg(p.to_string());
        }
        cmd.arg(&target);
        eprintln!("Connecting to {target} ...");
        let st = cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).status().context("running ssh")?;
        if !st.success() {
            bail!("ssh connection to {target} failed");
        }
        let mut opts = vec!["-S".to_string(), socket.clone()];
        if let Some(p) = port {
            opts.push("-p".into());
            opts.push(p.to_string());
        }
        let _ = SSH_OPTS.set(opts);
        let mut conn = Self { target: target.clone(), user, host, port, socket, api_port, node: String::new() };
        let name = Exec::Ssh(target).output("hostname", &["-s"]).context("reading the node name")?;
        conn.node = name.trim().to_string();
        if conn.node.is_empty() {
            bail!("could not read the hostname of {}", conn.host);
        }
        Ok(conn)
    }

    pub fn exec(&self) -> Exec {
        Exec::Ssh(self.target.clone())
    }

    pub fn label(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.target),
            None => self.target.clone(),
        }
    }

    pub fn close(&self) {
        let _ = Command::new("ssh")
            .args(["-S", &self.socket, "-O", "exit", &self.target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(parse_target("root@10.0.0.5"), ("root".into(), "10.0.0.5".into(), None));
        assert_eq!(parse_target("admin@pve1:2222"), ("admin".into(), "pve1".into(), Some(2222)));
        assert_eq!(parse_target("pve1"), ("root".into(), "pve1".into(), None));
        assert_eq!(parse_target("root@[fd00::1]:22"), ("root".into(), "fd00::1".into(), Some(22)));
        assert_eq!(parse_target("root@fd00::1"), ("root".into(), "fd00::1".into(), None));
    }
}
