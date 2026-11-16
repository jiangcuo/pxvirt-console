//! System > Updates: upgradable packages (refreshes the package index automatically when it
//! is stale), search all packages, upgrade everything / one package, install a package.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    Frame,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use super::{fv, scroll, View};
use crate::api::{is_upid, Api};
use crate::ctx::{ActionOutcome, Cmd, Ctx, Suspend};
use crate::exec::Exec;
use crate::i18n::tr;
use crate::loader::{Job, Loader};
use crate::ui::form::{self, Form};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, style_dim, style_good, style_warn, Kv, ACCENT};
use crate::util::{jarr, jb, js, urlenc};

/// Refresh the index automatically when it is older than this.
const STALE_SECS: i64 = 6 * 3600;

fn is_pve_pkg(name: &str) -> bool {
    ["pve", "proxmox", "qemu-server", "lxc-pve", "pxvirt", "ceph", "zfs", "corosync", "spiceterm", "vncterm"].iter().any(|x| name.contains(x))
}

#[derive(Default)]
struct UpdData {
    pkgs: Vec<Value>,
    repos: Value,
    /// seconds since the last successful `apt update`
    age: Option<i64>,
}

#[derive(Clone)]
struct Found {
    name: String,
    version: String,
    suite: String,
    installed: String,
}

#[derive(Default)]
pub struct Updates {
    data: Loader<UpdData>,
    table: Table,
    scroll: u16,
    /// `apt update` running in the background (UPID polling until done)
    refreshing: Option<Job<String>>,
    auto_done: bool,
    status: Option<(String, bool)>,
    /// search mode
    query: Option<String>,
    found: Loader<Vec<Found>>,
    ftable: Table,
}

fn index_age(exec: &Option<Exec>) -> Option<i64> {
    let e = exec.as_ref()?;
    let out = e.output("sh", &["-c", "stat -c %Y /var/lib/apt/periodic/update-success-stamp /var/cache/apt/pkgcache.bin 2>/dev/null | sort -n | tail -1"]).ok()?;
    let ts: i64 = out.trim().parse().ok()?;
    Some(chrono::Utc::now().timestamp() - ts)
}

/// Run `apt update` through the API and wait for the task to finish.
fn run_apt_update(api: Arc<Api>, node: String) -> anyhow::Result<String> {
    let v = api.post(&format!("/nodes/{node}/apt/update"), &[])?;
    let Some(upid) = is_upid(&v) else { return Ok("OK".into()) };
    let path = format!("/nodes/{node}/tasks/{}/status", urlenc(&upid));
    for _ in 0..600 {
        std::thread::sleep(Duration::from_secs(1));
        let st = api.get(&path)?;
        if js(&st, "status") == "stopped" {
            return Ok(js(&st, "exitstatus"));
        }
    }
    anyhow::bail!("apt update did not finish within 10 minutes")
}

fn parse_apt_list(out: &str) -> Vec<Found> {
    // "name/suite[,suite] version arch [installed,...]"
    out.lines()
        .filter(|l| l.contains('/') && !l.starts_with("Listing"))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (name, suite) = it.next()?.split_once('/')?;
            let version = it.next().unwrap_or("").to_string();
            let _arch = it.next();
            let rest: String = it.collect::<Vec<_>>().join(" ");
            let installed = if rest.contains("upgradable") {
                "upgradable".to_string()
            } else if rest.contains("installed") {
                "installed".to_string()
            } else {
                String::new()
            };
            Some(Found { name: name.to_string(), version, suite: suite.to_string(), installed })
        })
        .collect()
}

impl Updates {
    fn start_refresh(&mut self, ctx: &mut Ctx) {
        if self.refreshing.is_some() {
            return;
        }
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        ctx.audit("apt update");
        self.status = Some((tr("Refreshing package index (apt update)...").into(), false));
        self.refreshing = Some(Job::spawn(move || run_apt_update(api, node)));
    }

    pub(crate) fn suspend_apt(ctx: &mut Ctx, args: Vec<String>, banner: String) {
        if let Some(exec) = ctx.exec() {
            ctx.audit(&format!("apt-get {}", args.join(" ")));
            ctx.cmds.push(Cmd::Suspend(Suspend {
                exec,
                prog: "apt-get".into(),
                args,
                env: vec![("DEBIAN_FRONTEND".into(), "readline".into())],
                banner,
                logout_after: false,
                pause_after: true,
            }));
        }
    }
}

impl View for Updates {
    fn title(&self) -> &'static str {
        "Updates"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let exec = ctx.exec();
        self.data.load(move || {
            let mut pkgs = jarr(&api.get(&format!("/nodes/{node}/apt/update"))?);
            for p in pkgs.iter_mut() {
                if let Value::Object(m) = p {
                    for v in m.values_mut() {
                        if let Value::String(s) = v {
                            *s = crate::util::fix_mojibake(s);
                        }
                    }
                }
            }
            pkgs.sort_by_key(|p| (!is_pve_pkg(&js(p, "Package")), js(p, "Package")));
            let repos = api.get(&format!("/nodes/{node}/apt/repositories")).unwrap_or(Value::Null);
            Ok(UpdData { pkgs, repos, age: index_age(&exec) })
        });
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let mut ch = self.data.poll() | self.found.poll();
        // refresh a stale index automatically (once per session)
        if !self.auto_done {
            if let Some(d) = &self.data.data {
                self.auto_done = true;
                let stale = d.age.map(|a| a > STALE_SECS).unwrap_or(false);
                if stale && ctx.need_node_priv("Sys.Modify").is_ok() {
                    self.start_refresh(ctx);
                    ch = true;
                }
            }
        }
        if let Some(j) = &self.refreshing {
            if let Some(r) = j.poll() {
                self.refreshing = None;
                self.status = Some(match r {
                    Ok(s) if s == "OK" => (tr("Package index refreshed").into(), false),
                    Ok(s) => (format!("apt update: {s}"), true),
                    Err(e) => (format!("apt update: {e:#}"), true),
                });
                self.data.at = None;
                self.refresh(ctx);
                ch = true;
            }
        }
        ch
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(300)
    }

    fn table(&mut self) -> Option<&mut Table> {
        if self.query.is_some() {
            Some(&mut self.ftable)
        } else {
            Some(&mut self.table)
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.query.is_some() {
            vec![("Esc", "back")]
        } else {
            vec![]
        }
    }

    fn on_key(&mut self, key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        if key.code == KeyCode::Esc && self.query.is_some() {
            self.query = None;
            self.found.clear();
            return true;
        }
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        if let Some(q) = &self.query {
            let rows: Vec<Row> = self
                .found
                .data
                .iter()
                .flatten()
                .map(|p| {
                    let st = match p.installed.as_str() {
                        "installed" => style_good(),
                        "upgradable" => style_warn(),
                        _ => style_dim(),
                    };
                    Row::new(p.name.clone()).c(p.name.clone()).c(p.version.clone()).cs(if p.installed.is_empty() { "-".into() } else { p.installed.clone() }, st).c(p.suite.clone())
                })
                .collect();
            let cols = [col("Package", Constraint::Min(20)), col("Version", Constraint::Length(24)), col("State", Constraint::Length(11)), col("Suite", Constraint::Length(18))];
            let title = format!("{} \"{q}\"  (Esc {})", tr("Search"), tr("back"));
            self.ftable.draw(f, area, &title, &cols, &rows, focused, loading_or_error(&self.found.error, self.found.loading()));
            return;
        }
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flat_map(|d| d.pkgs.iter())
            .map(|p| {
                let name = js(p, "Package");
                Row::new(name.clone())
                    .cs(name.clone(), if is_pve_pkg(&name) { Style::default().fg(ACCENT) } else { Style::default() })
                    .c(js(p, "Version"))
                    .c(js(p, "OldVersion"))
                    .c(js(p, "Origin"))
            })
            .collect();
        let cols = [col("Package", Constraint::Min(18)), col("New version", Constraint::Length(20)), col("Installed", Constraint::Length(20)), col("Origin", Constraint::Length(12))];
        let mut title = tr("Upgradable").to_string();
        if let Some(age) = self.data.data.as_ref().and_then(|d| d.age) {
            title.push_str(&format!("  ·  {} {}", tr("index refreshed"), crate::util::fmt_duration(age.max(0) as u64)));
            title.push_str(tr(" ago"));
        }
        let status = if self.refreshing.is_some() {
            Some((tr("refreshing...").to_string(), Style::default().fg(ACCENT)))
        } else {
            loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none())
        };
        self.table.draw(f, area, &title, &cols, &rows, focused, status);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some((m, err)) = &self.status {
            kv.styled("", m.clone(), if *err { crate::ui::style_bad() } else { style_good() });
            kv.blank();
        }
        if let Some(d) = &self.data.data {
            if self.query.is_none() {
                if let Some(p) = self.table.selected().and_then(|k| d.pkgs.iter().find(|p| js(p, "Package") == k)) {
                    kv.add("Package", js(p, "Package"));
                    kv.add("Installed", js(p, "OldVersion"));
                    kv.add("New version", js(p, "Version"));
                    kv.add("Section", js(p, "Section"));
                    kv.add("Priority", js(p, "Priority"));
                    kv.add("Description", js(p, "Description"));
                }
            }
            kv.section("Repositories");
            for file in d.repos.get("files").map(jarr).unwrap_or_default() {
                for r in file.get("repositories").map(jarr).unwrap_or_default() {
                    let en = jb(&r, "Enabled");
                    let list = |k: &str| r.get(k).map(jarr).unwrap_or_default().iter().map(|u| u.as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join(" ");
                    kv.styled(if en { "on" } else { "off" }, format!("{} {} {}", list("URIs"), list("Suites"), list("Components")), if en { Style::default() } else { style_dim() });
                }
            }
            for i in d.repos.get("infos").map(jarr).unwrap_or_default() {
                if js(&i, "kind") == "warning" {
                    kv.styled("warning", js(&i, "message"), style_warn());
                }
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let m = ctx.need_node_priv("Sys.Modify");
        let apt = ctx.need_root().and(ctx.need_exec("Sys.Modify").map(|_| ()));
        if self.query.is_some() {
            let sel = self.ftable.selected().is_some();
            return vec![
                item('i', "Install").require(apt.clone()).disabled_if(!sel, "nothing selected"),
                item('v', "Show package info").require(ctx.need_exec("Sys.Audit").map(|_| ())).disabled_if(!sel, "nothing selected"),
                item('s', "New search").require(ctx.need_exec("Sys.Audit").map(|_| ())),
            ];
        }
        let sel = self.table.selected().is_some();
        vec![
            item('u', "Refresh package index (apt update)").require(m).disabled_if(self.refreshing.is_some(), "already running"),
            item('U', "Upgrade all (apt dist-upgrade)").require(apt.clone()),
            item('i', "Upgrade only this package").require(apt).disabled_if(!sel, "nothing selected"),
            item('c', "Show changelog").disabled_if(!sel, "nothing selected"),
            item('s', "Search packages").require(ctx.need_exec("Sys.Audit").map(|_| ())),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let n = ctx.node.clone();
        match key {
            'u' => self.start_refresh(ctx),
            'U' => Self::suspend_apt(ctx, vec!["dist-upgrade".into()], format!("apt-get dist-upgrade on {n}. Review the changes before confirming.\n")),
            's' => ctx.popup(Popup::Form(Form::new(
                "Search packages",
                "search",
                vec![form::text("q", "Name contains", self.query.as_deref().unwrap_or("")).req(true)],
            ))),
            'i' if self.query.is_some() => {
                if let Some(p) = self.ftable.selected().map(|s| s.to_string()) {
                    Self::suspend_apt(ctx, vec!["install".into(), p.clone()], format!("apt-get install {p} on {n}\n"));
                }
            }
            'i' => {
                if let Some(p) = self.table.selected().map(|s| s.to_string()) {
                    Self::suspend_apt(ctx, vec!["install".into(), "--only-upgrade".into(), p.clone()], format!("Upgrading only {p} on {n}\n"));
                }
            }
            'v' => {
                if let (Some(p), Some(exec)) = (self.ftable.selected().map(|s| s.to_string()), ctx.exec()) {
                    ctx.act(format!("apt-cache show {p}"), move || {
                        let out = exec.output("apt-cache", &["show", "--no-all-versions", &p])?;
                        Ok(ActionOutcome::Text(p, out))
                    });
                }
            }
            'c' => {
                if let Some(pkg) = self.table.selected().map(|s| s.to_string()) {
                    let api = ctx.api.clone();
                    ctx.act(format!("Changelog {pkg}"), move || {
                        let v = api.get(&format!("/nodes/{n}/apt/changelog?name={pkg}"))?;
                        Ok(ActionOutcome::Text(format!("Changelog {pkg}"), v.as_str().unwrap_or("").to_string()))
                    });
                }
            }
            _ => {}
        }
    }

    fn on_form(&mut self, _tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let q: String = fv(v, "q").chars().filter(|c| c.is_ascii_alphanumeric() || "-+._".contains(*c)).collect();
        if q.is_empty() {
            return;
        }
        let Some(exec) = ctx.exec() else { return };
        self.query = Some(q.clone());
        self.ftable = Table::default();
        self.found.clear();
        self.found.load(move || {
            let out = exec.output("sh", &["-c", "apt list \"*$1*\" 2>/dev/null | head -500", "sh", &q])?;
            let mut v = parse_apt_list(&out);
            // exact and prefix matches first
            v.sort_by_key(|p| (p.name != q, !p.name.starts_with(q.as_str()), p.name.clone()));
            Ok(v)
        });
    }

    fn reset(&mut self) {
        self.data.clear();
        self.auto_done = false;
        self.query = None;
        self.found.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apt_list() {
        let v = parse_apt_list("Listing...\nhtop/stable,now 3.4.1-5 amd64 [installed]\nbtop/stable 1.4.3 amd64\nvim/stable 2:9.1 amd64 [installed,upgradable to: 2:9.2]\n");
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].installed, "installed");
        assert_eq!(v[1].installed, "");
        assert_eq!(v[2].installed, "upgradable");
    }
}
