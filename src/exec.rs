//! Running host commands on the node being viewed: locally, or over ssh for other nodes.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crate::util::shell_quote;

#[derive(Clone, Debug, PartialEq)]
pub enum Exec {
    Local,
    /// `user@host`; key based auth only (BatchMode). In SSH mode this goes through the
    /// shared control connection (see `sshconn::ssh_opts`).
    Ssh(String),
    /// SSH mode, another cluster node: (`user@entry-node`, `root@node-ip`) — hop from the
    /// node we are connected to (cluster nodes trust each other's root keys).
    SshVia(String, String),
}

const SSH_BASE: [&str; 6] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-o", "StrictHostKeyChecking=accept-new"];

fn ssh_cmd(tty: bool) -> Command {
    let mut c = Command::new("ssh");
    if tty {
        c.arg("-t");
    }
    c.args(crate::sshconn::ssh_opts());
    c
}

impl Exec {
    pub fn label(&self) -> String {
        match self {
            Exec::Local => "local".into(),
            Exec::Ssh(t) => format!("ssh {t}"),
            Exec::SshVia(t, hop) => format!("ssh {t} -> {hop}"),
        }
    }

    fn build(&self, prog: &str, args: &[&str], tty: bool) -> Command {
        match self {
            Exec::Local => {
                let mut c = Command::new(prog);
                c.args(args);
                c
            }
            Exec::Ssh(target) => {
                let mut c = ssh_cmd(tty);
                if !tty {
                    c.args(SSH_BASE);
                } else {
                    c.args(&SSH_BASE[2..]);
                }
                c.arg(target).arg("--").arg(remote_cmdline(prog, args));
                c
            }
            Exec::SshVia(target, hop) => {
                let inner = remote_cmdline(prog, args);
                let mut hop_args: Vec<&str> = Vec::new();
                if tty {
                    hop_args.push("-t");
                }
                hop_args.extend(SSH_BASE);
                hop_args.extend([hop.as_str(), "--", inner.as_str()]);
                let mut c = ssh_cmd(tty);
                c.args(&SSH_BASE[2..]);
                c.arg(target).arg("--").arg(remote_cmdline("ssh", &hop_args));
                c
            }
        }
    }

    /// Build a non-interactive command.
    pub fn command(&self, prog: &str, args: &[&str]) -> Command {
        self.build(prog, args, false)
    }

    /// Build an interactive command (shell, console). Inherits the terminal.
    pub fn interactive(&self, prog: &str, args: &[&str]) -> Command {
        self.build(prog, args, true)
    }

    /// Run and return stdout; fails with stderr on non-zero exit.
    pub fn output(&self, prog: &str, args: &[&str]) -> Result<String> {
        let out = self
            .command(prog, args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {prog}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
            bail!("{prog} failed: {}", if err.is_empty() { so } else { err });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Run and return stdout+stderr regardless of exit status.
    pub fn output_any(&self, prog: &str, args: &[&str]) -> Result<(bool, String)> {
        let out = self
            .command(prog, args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {prog}"))?;
        let mut s = String::from_utf8_lossy(&out.stdout).to_string();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), s))
    }

    pub fn read_file(&self, path: &str) -> Result<String> {
        match self {
            Exec::Local => std::fs::read_to_string(path).with_context(|| format!("reading {path}")),
            _ => self.output("cat", &[path]),
        }
    }

    /// Spawn a command and stream its output lines (stdout+stderr) through a channel.
    pub fn stream(&self, prog: &str, args: &[&str]) -> Result<Stream> {
        let mut child = self
            .command(prog, args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {prog}"))?;
        let (tx, rx) = mpsc::channel();
        if let Some(out) = child.stdout.take() {
            let tx = tx.clone();
            thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        if let Some(err) = child.stderr.take() {
            thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        Ok(Stream { child, rx })
    }
}

fn remote_cmdline(prog: &str, args: &[&str]) -> String {
    let mut s = shell_quote(prog);
    for a in args {
        s.push(' ');
        s.push_str(&shell_quote(a));
    }
    s
}

/// A running child process with line-buffered output.
pub struct Stream {
    child: Child,
    pub rx: Receiver<String>,
}

impl Stream {
    pub fn drain(&self, max: usize) -> Vec<String> {
        let mut v = Vec::new();
        while v.len() < max {
            match self.rx.try_recv() {
                Ok(l) => v.push(l),
                Err(_) => break,
            }
        }
        v
    }

    pub fn finished(&mut self) -> Option<bool> {
        self.child.try_wait().ok().flatten().map(|s| s.success())
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.kill();
    }
}
