//! Shared context handed to views: API, session/permissions, node being viewed,
//! and an outbox of commands for the app (popups, actions, suspend to shell...).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::api::{is_upid, Api};
use crate::config::Config;
use crate::exec::Exec;
use crate::loader::Job;
use crate::ui::logview::{LogViewer, Source};
use crate::ui::popup::Popup;

#[derive(Default, Clone)]
pub struct Session {
    pub user: String,
    pub perms: HashMap<String, HashSet<String>>,
    /// pveproxy unreachable: pvesh transport, root only.
    pub rescue: bool,
}

impl Session {
    pub fn is_root(&self) -> bool {
        self.user == "root@pam"
    }

    /// Effective privilege check against the /access/permissions map
    /// (most specific listed path wins).
    pub fn can(&self, path: &str, privilege: &str) -> bool {
        if self.is_root() || self.rescue {
            return true;
        }
        let mut p = path.trim_end_matches('/').to_string();
        loop {
            let key = if p.is_empty() { "/" } else { p.as_str() };
            if let Some(set) = self.perms.get(key) {
                return set.contains(privilege);
            }
            if p.is_empty() {
                return false;
            }
            match p.rfind('/') {
                Some(i) => p.truncate(i),
                None => p.clear(),
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NodeInfo {
    pub name: String,
    pub ip: String,
    pub online: bool,
    pub local: bool,
}

pub enum ActionOutcome {
    Upid(String),
    Msg(String),
    /// Show text (title, body) in the log viewer.
    Text(String, String),
}

/// Interactive program to run with the TUI suspended (shell, guest console, apt upgrade).
pub struct Suspend {
    pub exec: Exec,
    pub prog: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub banner: String,
    /// Log out afterwards (shell).
    pub logout_after: bool,
    pub pause_after: bool,
}

pub struct VncRequest {
    pub node: String,
    pub vmid: u32,
    pub name: String,
}

pub enum Cmd {
    Popup(Popup),
    Toast(String, bool),
    Action { title: String, job: Job<ActionOutcome> },
    Suspend(Suspend),
    Vnc(VncRequest),
    SwitchNode(String),
    /// Open a page: (module index, view title).
    Goto(usize, &'static str),
}

/// Disabled-reason for actions that need host commands in API-only remote mode
/// (such actions are hidden instead of greyed out).
pub const NEEDS_SSH: &str = "needs SSH mode";

pub struct Ctx {
    pub api: Arc<Api>,
    pub session: Session,
    pub cfg: Config,
    /// Node currently viewed.
    pub node: String,
    pub nodes: Vec<NodeInfo>,
    /// Name of the node this console runs on (local mode only).
    pub local_node: Option<String>,
    /// API-only remote mode (--remote): no host commands at all.
    pub remote: bool,
    /// SSH mode (--ssh): `user@host` of the node we are connected to.
    pub ssh: Option<String>,
    pub cmds: Vec<Cmd>,
    /// Architecture per node (filled lazily from /nodes/x/status).
    pub node_arch: HashMap<String, String>,
    /// Live metrics of the viewed node (shared with the sampler thread).
    pub metrics: Arc<std::sync::Mutex<crate::metrics::MetricsData>>,
    /// Latest health report of the viewed node.
    pub health: Option<Arc<crate::health::Report>>,
    /// Run every health check now (set by the Health page).
    pub health_now: bool,
}

impl Ctx {
    pub fn node_path(&self) -> String {
        format!("/nodes/{}", self.node)
    }

    pub fn is_local_node(&self) -> bool {
        self.local_node.as_deref() == Some(self.node.as_str())
    }

    pub fn node_info(&self, name: &str) -> Option<&NodeInfo> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// The viewed node is the machine this console runs on (rescue tools, local /sys reads).
    pub fn this_machine(&self) -> bool {
        self.ssh.is_none() && !self.remote && self.is_local_node()
    }

    /// How to run host commands on `node`, if possible at all.
    pub fn exec_for(&self, node: &str) -> Option<Exec> {
        if let Some(t) = &self.ssh {
            if self.local_node.as_deref() == Some(node) {
                return Some(Exec::Ssh(t.clone()));
            }
            // other cluster nodes: hop from the node we are connected to
            let ip = self.node_info(node).map(|n| n.ip.clone()).filter(|ip| !ip.is_empty())?;
            return Some(Exec::SshVia(t.clone(), format!("root@{ip}")));
        }
        if self.remote || !crate::util::is_root_process() {
            return None;
        }
        if self.local_node.as_deref() == Some(node) {
            return Some(Exec::Local);
        }
        if self.session.is_root() {
            let ip = self.node_info(node).map(|n| n.ip.clone()).filter(|ip| !ip.is_empty())?;
            return Some(Exec::Ssh(format!("root@{ip}")));
        }
        None
    }

    pub fn exec(&self) -> Option<Exec> {
        self.exec_for(&self.node)
    }

    /// Host command access + PVE privilege on the node.
    pub fn need_exec(&self, privilege: &str) -> Result<Exec, String> {
        self.need_node_priv(privilege)?;
        self.exec().ok_or_else(|| {
            if self.remote {
                NEEDS_SSH.to_string()
            } else {
                crate::i18n::tr("local only").to_string()
            }
        })
    }

    pub fn need_priv(&self, path: &str, privilege: &str) -> Result<(), String> {
        if self.session.can(path, privilege) {
            Ok(())
        } else {
            Err(format!("needs {privilege}"))
        }
    }

    pub fn need_node_priv(&self, privilege: &str) -> Result<(), String> {
        self.need_priv(&self.node_path(), privilege)
    }

    pub fn need_root(&self) -> Result<(), String> {
        if self.session.is_root() {
            Ok(())
        } else {
            Err("root@pam only".into())
        }
    }

    pub fn popup(&mut self, p: Popup) {
        self.cmds.push(Cmd::Popup(p));
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.cmds.push(Cmd::Toast(msg.into(), false));
    }

    pub fn toast_err(&mut self, msg: impl Into<String>) {
        self.cmds.push(Cmd::Toast(msg.into(), true));
    }

    pub fn open_log(&mut self, title: &str, source: Source) {
        self.popup(Popup::Log(Box::new(LogViewer::new(title, source))));
    }

    /// Run a write action in the background; UPID results open the task viewer.
    pub fn act<F>(&mut self, title: impl Into<String>, f: F)
    where
        F: FnOnce() -> anyhow::Result<ActionOutcome> + Send + 'static,
    {
        self.cmds.push(Cmd::Action { title: title.into(), job: Job::spawn(f) });
    }

    /// Convenience: API write call; returns UPID → task viewer, otherwise a toast.
    pub fn api_call(&mut self, title: impl Into<String>, method: &'static str, path: String, params: Vec<(&'static str, String)>) {
        let api = self.api.clone();
        let title = title.into();
        let t2 = title.clone();
        self.act(title, move || {
            let v = match method {
                "POST" => api.post(&path, &params)?,
                "PUT" => api.put(&path, &params)?,
                "DELETE" => api.delete(&path, &params)?,
                _ => anyhow::bail!("bad method"),
            };
            Ok(match is_upid(&v) {
                Some(u) => ActionOutcome::Upid(u),
                None => ActionOutcome::Msg(format!("{t2}: OK")),
            })
        });
    }

    /// Run a host command in the background and report its output.
    pub fn exec_call(&mut self, title: impl Into<String>, exec: Exec, prog: &str, args: Vec<String>) {
        let prog = prog.to_string();
        let title = title.into();
        let t2 = title.clone();
        self.act(title, move || {
            let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let out = exec.output(&prog, &a)?;
            let first = out.lines().next().unwrap_or("").trim().to_string();
            Ok(ActionOutcome::Msg(if first.is_empty() { format!("{t2}: OK") } else { format!("{t2}: {first}") }))
        });
    }

    pub fn audit(&self, what: &str) {
        crate::audit(&self.session.user, &self.node, what);
    }
}

#[cfg(test)]
impl Ctx {
    /// Context for view tests: root session, pvesh API, node "n1".
    pub fn test() -> Ctx {
        Ctx {
            api: Arc::new(crate::api::Api::pvesh()),
            session: Session { user: "root@pam".into(), ..Default::default() },
            cfg: Config::default(),
            node: "n1".into(),
            nodes: vec![],
            local_node: None,
            remote: false,
            ssh: None,
            cmds: vec![],
            node_arch: Default::default(),
            metrics: Arc::new(std::sync::Mutex::new(Default::default())),
            health: None,
            health_now: false,
        }
    }
}
