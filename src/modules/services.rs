//! Services & Logs: PVE services (with live log tail), all systemd units, journal presets, tasks.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    Frame,
};
use serde_json::Value;
use std::time::Duration;

use super::{scroll, View};
use crate::ctx::Ctx;
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::logview::{LogViewer, Source};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::task::TaskView;
use crate::ui::{draw_lines, loading_or_error, status_style, style_bad, style_dim, style_warn, Kv, ACCENT};
use crate::util::{fmt_duration, fmt_time, jarr, ji, js, urlenc};

pub fn views() -> Vec<Box<dyn View>> {
    vec![
        Box::new(PveServices::default()),
        Box::new(Units::default()),
        Box::new(Journal::default()),
        Box::new(Tasks::default()),
    ]
}

const CRITICAL: [&str; 4] = ["pveproxy", "pvedaemon", "pve-cluster", "corosync"];

/// Log source for one unit on the current node: journalctl if possible, else API syslog.
fn unit_log_source(ctx: &Ctx, unit: &str) -> Source {
    match ctx.exec() {
        Some(exec) if ctx.session.can(&ctx.node_path(), "Sys.Syslog") => Source::Journal { exec, unit: Some(unit.to_string()), kernel: false, extra: vec![] },
        _ => Source::ApiSyslog { api: ctx.api.clone(), node: ctx.node.clone(), service: unit.trim_end_matches(".service").to_string() },
    }
}

/// Tail of a unit's log for the detail pane.
fn load_tail(ctx: &Ctx, unit: &str, n: usize) -> impl FnOnce() -> anyhow::Result<Vec<String>> + Send + 'static {
    let exec = ctx.exec();
    let api = ctx.api.clone();
    let node = ctx.node.clone();
    let unit = unit.to_string();
    move || {
        if let Some(e) = exec {
            let out = e.output("journalctl", &["--no-pager", "-o", "short-iso", "-n", &n.to_string(), "-u", &unit])?;
            return Ok(out.lines().map(|s| s.to_string()).collect());
        }
        let svc = unit.trim_end_matches(".service");
        let first = api.get_full(&format!("/nodes/{node}/syslog?service={svc}&limit=1"))?;
        let total = first.get("total").and_then(|t| t.as_u64()).unwrap_or(0);
        let start = total.saturating_sub(n as u64);
        let v = api.get(&format!("/nodes/{node}/syslog?service={svc}&start={start}&limit={n}"))?;
        Ok(jarr(&v).iter().map(|r| js(r, "t")).filter(|l| l != "no content").collect())
    }
}

/// "2026-11-11T09:40:33+08:00 host unit[1]: msg" -> ("11-11 09:40:33 ", "unit[1]: msg")
fn compact_log_line(l: &str) -> (String, String) {
    let mut parts = l.splitn(3, ' ');
    let (Some(ts), Some(_host), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return (String::new(), l.to_string());
    };
    if ts.len() >= 19 && ts.as_bytes().get(10) == Some(&b'T') {
        (format!("{} {} ", &ts[5..10], &ts[11..19]), rest.to_string())
    } else {
        (String::new(), l.to_string())
    }
}

fn tail_lines(tail: &Loader<Vec<String>>, height: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(e) = &tail.error {
        lines.push(Line::from(Span::styled(e.clone(), style_dim())));
    }
    if let Some(t) = &tail.data {
        for l in t.iter().rev().take(height).rev() {
            let low = l.to_lowercase();
            let st = if low.contains("error") || low.contains("fail") { style_bad() } else if low.contains("warn") { style_warn() } else { Style::default() };
            let (ts, msg) = compact_log_line(l);
            lines.push(Line::from(vec![Span::styled(ts, style_dim()), Span::styled(msg, st)]));
        }
    }
    lines
}

// ===========================================================================
// PVE services
// ===========================================================================

#[derive(Default)]
struct PveServices {
    data: Loader<Vec<Value>>,
    table: Table,
    tail: Loader<Vec<String>>,
    tail_for: String,
}

impl PveServices {
    fn sel(&self) -> Option<&Value> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.iter().find(|s| js(s, "service") == k)
    }
}

impl View for PveServices {
    fn title(&self) -> &'static str {
        "PVE Services"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.data.load(move || Ok(jarr(&api.get(&format!("/nodes/{node}/services"))?)));
        self.tail.at = None;
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let mut ch = self.data.poll() | self.tail.poll();
        if let Some(svc) = self.table.selected().map(|s| s.to_string()) {
            if svc != self.tail_for {
                self.tail_for = svc.clone();
                self.tail.clear();
            }
            if self.tail.stale(Duration::from_secs(5)) && ctx.session.can(&ctx.node_path(), "Sys.Syslog") {
                self.tail.load(load_tail(ctx, &svc, 60));
                ch = true;
            }
        }
        ch
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(5)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flatten()
            .filter(|s| js(s, "unit-state") != "not-found")
            .map(|s| {
                let state = js(s, "state");
                let us = js(s, "unit-state");
                Row::new(js(s, "service"))
                    .cs(js(s, "service"), if CRITICAL.contains(&js(s, "service").as_str()) { Style::default().fg(ACCENT) } else { Style::default() })
                    .cs(state.clone(), status_style(&state))
                    .cs(us.clone(), if us == "enabled" { Style::default() } else { style_dim() })
                    .c(js(s, "desc"))
                    .dim(state == "dead" && us != "enabled")
            })
            .collect();
        let cols = [col("Service", Constraint::Length(18)), col("State", Constraint::Length(8)), col("Boot", Constraint::Length(9)), col("Description", Constraint::Min(10))];
        self.table.draw(f, area, "PVE Services", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let h = area.height.saturating_sub(2) as usize;
        let mut lines = tail_lines(&self.tail, h);
        if !ctx.session.can(&ctx.node_path(), "Sys.Syslog") {
            lines = vec![Line::from(Span::styled("needs Sys.Syslog", style_dim()))];
        }
        let title = format!("{} {}", tr("Journal"), self.tail_for);
        draw_lines(f, area, &title, focused, lines, 0);
    }

    fn draw_extra(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx) -> bool {
        let mut kv = Kv::new();
        if let Some(s) = self.sel() {
            kv.add("Service", js(s, "service"));
            kv.add("Name", js(s, "name"));
            kv.status("State", js(s, "state"));
            kv.add("Active", js(s, "active-state"));
            kv.add("Boot", js(s, "unit-state"));
            kv.add("Description", js(s, "desc"));
        }
        draw_lines(f, area, "Details", false, kv.lines(), 0);
        true
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![]
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(s) = self.sel() else { return vec![] };
        let m = ctx.need_node_priv("Sys.Modify");
        let running = js(s, "state") == "running";
        let enabled = js(s, "unit-state") == "enabled";
        let root_exec = ctx.need_root().and(ctx.need_exec("Sys.Modify").map(|_| ()));
        vec![
            item('s', "Start").require(m.clone()).disabled_if(running, "already running"),
            item('x', "Stop").require(m.clone()).disabled_if(!running, "not running"),
            item('t', "Restart").require(m.clone()),
            item('o', "Reload").require(m).disabled_if(!running, "not running"),
            item('L', "View log").require(ctx.need_node_priv("Sys.Syslog")),
            item(if enabled { 'E' } else { 'e' }, if enabled { "Disable at boot" } else { "Enable at boot" }).require(root_exec),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(svc) = self.sel().map(|s| js(s, "service")) else { return };
        match key {
            's' | 'x' | 't' | 'o' => {
                let cmd = match key {
                    's' => "start",
                    'x' => "stop",
                    't' => "restart",
                    _ => "reload",
                };
                let critical = CRITICAL.contains(&svc.as_str());
                if key == 's' || (!critical && key != 'x') {
                    self.run(ctx, &svc, cmd);
                } else {
                    let mut body = vec![format!("{cmd} {svc} on {}?", ctx.node)];
                    if critical {
                        body.push(match svc.as_str() {
                            "pveproxy" => "The Web UI and API on this node are unavailable while it restarts.".into(),
                            "pve-cluster" => "pmxcfs (/etc/pve) is unavailable while it restarts; guests keep running.".into(),
                            "corosync" => "Cluster communication of this node is interrupted (HA may fence it!).".into(),
                            _ => "API requests through this node are interrupted.".into(),
                        });
                    }
                    ctx.popup(Popup::confirm(&format!("{cmd} {svc}"), &format!("{cmd}:{svc}"), body, true));
                }
            }
            'L' => {
                let src = unit_log_source(ctx, &svc);
                ctx.open_log(&format!("{svc} @ {}", ctx.node), src);
            }
            'e' | 'E' => {
                if let Some(e) = ctx.exec() {
                    let a = if key == 'e' { "enable" } else { "disable" };
                    ctx.exec_call(format!("systemctl {a} {svc}"), e, "systemctl", vec![a.into(), svc]);
                }
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if let Some((cmd, svc)) = tag.split_once(':') {
            self.run(ctx, svc, cmd);
        }
    }

    fn reset(&mut self) {
        self.data.clear();
        self.tail.clear();
    }
}

impl PveServices {
    fn run(&mut self, ctx: &mut Ctx, svc: &str, cmd: &str) {
        let n = ctx.node.clone();
        if ctx.session.rescue {
            if let Some(e) = ctx.exec() {
                ctx.exec_call(format!("systemctl {cmd} {svc}"), e, "systemctl", vec![cmd.into(), svc.into()]);
            }
            return;
        }
        ctx.api_call(format!("{cmd} {svc}"), "POST", format!("/nodes/{n}/services/{svc}/{cmd}"), vec![]);
    }
}

// ===========================================================================
// All systemd units
// ===========================================================================

#[derive(Default)]
struct Units {
    data: Loader<Vec<Value>>,
    table: Table,
    failed_only: bool,
    tail: Loader<Vec<String>>,
    tail_for: String,
}

impl View for Units {
    fn title(&self) -> &'static str {
        "All Units"
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else {
            self.data.error = Some(tr("local only").into());
            return;
        };
        self.data.load(move || {
            let out = exec.output("systemctl", &["list-units", "--type=service", "--all", "--output=json", "--no-pager"])?;
            let mut v = jarr(&serde_json::from_str(&out)?);
            v.sort_by_key(|u| (js(u, "active") != "failed", js(u, "unit")));
            Ok(v)
        });
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let mut ch = self.data.poll() | self.tail.poll();
        if let Some(u) = self.table.selected().map(|s| s.to_string()) {
            if u != self.tail_for {
                self.tail_for = u.clone();
                self.tail.clear();
            }
            if self.tail.stale(Duration::from_secs(5)) && ctx.exec().is_some() {
                self.tail.load(load_tail(ctx, &u, 60));
                ch = true;
            }
        }
        ch
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(10)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("f", "failed only")]
    }

    fn on_key(&mut self, key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        if key.code == KeyCode::Char('f') {
            self.failed_only = !self.failed_only;
            return true;
        }
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flatten()
            .filter(|u| !self.failed_only || js(u, "active") == "failed")
            .map(|u| {
                let active = js(u, "active");
                let sub = js(u, "sub");
                Row::new(js(u, "unit"))
                    .c(js(u, "unit"))
                    .cs(active.clone(), status_style(&active))
                    .c(sub)
                    .c(js(u, "description"))
                    .dim(active == "inactive")
            })
            .collect();
        let cols = [col("Unit", Constraint::Length(34)), col("Active", Constraint::Length(9)), col("Sub", Constraint::Length(8)), col("Description", Constraint::Min(10))];
        let title = if self.failed_only { "Services (failed)" } else { "Services" };
        self.table.draw(f, area, title, &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let lines = tail_lines(&self.tail, area.height.saturating_sub(2) as usize);
        draw_lines(f, area, &format!("{} {}", tr("Journal"), self.tail_for), focused, lines, 0);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(k) = self.table.selected() else { return vec![] };
        let u = self.data.data.as_ref().and_then(|d| d.iter().find(|u| js(u, "unit") == k));
        let active = u.map(|u| js(u, "active")).unwrap_or_default();
        let e = ctx.need_exec("Sys.Modify").map(|_| ());
        let root = ctx.need_root();
        vec![
            item('s', "Start").require(e.clone()),
            item('x', "Stop").require(e.clone()),
            item('t', "Restart").require(e.clone()),
            item('o', "Reload").require(e.clone()),
            item('c', "Reset failed state").require(e.clone()).disabled_if(active != "failed", "not failed"),
            item('e', "Enable at boot").require(e.clone()).require(root.clone()),
            item('E', "Disable at boot").require(e.clone()).require(root),
            item('i', "Show status").require(ctx.need_exec("Sys.Audit").map(|_| ())),
            item('L', "View log").require(ctx.need_exec("Sys.Syslog").map(|_| ())),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(unit) = self.table.selected().map(|s| s.to_string()) else { return };
        let Some(exec) = ctx.exec() else { return };
        let cmd = match key {
            's' => "start",
            'x' => "stop",
            't' => "restart",
            'o' => "reload",
            'c' => "reset-failed",
            'e' => "enable",
            'E' => "disable",
            'i' => {
                let u = unit.clone();
                ctx.act(format!("status {unit}"), move || {
                    let (_, out) = exec.output_any("systemctl", &["status", "--no-pager", "-l", &u])?;
                    Ok(crate::ctx::ActionOutcome::Text(format!("systemctl status {u}"), out))
                });
                return;
            }
            'L' => {
                ctx.open_log(&format!("{unit} @ {}", ctx.node), Source::Journal { exec, unit: Some(unit), kernel: false, extra: vec![] });
                return;
            }
            _ => return,
        };
        if matches!(cmd, "stop" | "restart") && (unit.starts_with("pve") || unit.starts_with("corosync") || unit.starts_with("ssh")) {
            ctx.popup(Popup::confirm(&format!("{cmd} {unit}"), &format!("{cmd}:{unit}"), vec![format!("{cmd} {unit} on {}?", ctx.node)], true));
            return;
        }
        ctx.exec_call(format!("systemctl {cmd} {unit}"), exec, "systemctl", vec![cmd.into(), unit]);
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if let (Some((cmd, unit)), Some(exec)) = (tag.split_once(':'), ctx.exec()) {
            ctx.exec_call(format!("systemctl {cmd} {unit}"), exec, "systemctl", vec![cmd.into(), unit.into()]);
        }
    }

    fn reset(&mut self) {
        self.data.clear();
        self.tail.clear();
    }
}

// ===========================================================================
// Journal presets
// ===========================================================================

struct Preset {
    key: char,
    label: &'static str,
    unit: Option<&'static str>,
    kernel: bool,
    extra: &'static [&'static str],
    prio: usize,
    range: usize,
}

const PRESETS: &[Preset] = &[
    Preset { key: 'j', label: "Full journal (last hour)", unit: None, kernel: false, extra: &[], prio: 0, range: 3 },
    Preset { key: 'e', label: "Errors of this boot", unit: None, kernel: false, extra: &[], prio: 1, range: 1 },
    Preset { key: 'w', label: "Warnings of the last 24 hours", unit: None, kernel: false, extra: &[], prio: 2, range: 4 },
    Preset { key: 'k', label: "Kernel messages (dmesg, this boot)", unit: None, kernel: true, extra: &[], prio: 0, range: 1 },
    Preset { key: 'p', label: "PVE daemons (pvedaemon, pveproxy, pvestatd, pve-cluster)", unit: None, kernel: false, extra: &["-u", "pvedaemon", "-u", "pveproxy", "-u", "pvestatd", "-u", "pve-cluster"], prio: 0, range: 1 },
    Preset { key: 'c', label: "Cluster (corosync, pve-cluster, pve-ha-*)", unit: None, kernel: false, extra: &["-u", "corosync", "-u", "pve-cluster", "-u", "pve-ha-lrm", "-u", "pve-ha-crm"], prio: 0, range: 1 },
    Preset { key: 's', label: "SSH / logins (sshd, login)", unit: None, kernel: false, extra: &["-u", "ssh", "-t", "login", "-t", "sshd"], prio: 0, range: 4 },
    Preset { key: 'q', label: "QEMU / guests (qmeventd, pve-guests)", unit: None, kernel: false, extra: &["-u", "qmeventd", "-u", "pve-guests"], prio: 0, range: 1 },
    Preset { key: 'a', label: "pxvirt-console audit log", unit: None, kernel: false, extra: &["-t", "pxvirt-console"], prio: 0, range: 0 },
    Preset { key: 'b', label: "Previous boot", unit: None, kernel: false, extra: &["-b", "-1"], prio: 0, range: 0 },
];

#[derive(Default)]
struct Journal;

impl View for Journal {
    fn title(&self) -> &'static str {
        "Journal"
    }

    fn refresh(&mut self, _ctx: &mut Ctx) {}

    fn refresh_secs(&self) -> Option<u64> {
        None
    }

    fn has_detail(&self) -> bool {
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let src = if ctx.exec().is_some() { "journalctl".to_string() } else { format!("API /nodes/{}/journal", ctx.node) };
        let mut lines = vec![
            Line::from(Span::styled(format!("{} @ {} ({src})", tr("Journal"), ctx.node), Style::default().fg(ACCENT))),
            Line::from(""),
        ];
        for it in self.menu(ctx) {
            let st = if it.disabled.is_some() { style_dim() } else { Style::default() };
            let mut sp = vec![Span::styled(format!("  ({}) ", it.key), Style::default().fg(ACCENT)), Span::styled(tr(&it.label).to_string(), st)];
            if let Some(d) = it.disabled {
                sp.push(Span::styled(format!("  {d}"), style_dim()));
            }
            lines.push(Line::from(sp));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "In the viewer: Space follow/pause, / search, n/N next/prev, & matching lines only, p priority, t time range, w export",
            style_dim(),
        )));
        draw_lines(f, area, "Journal", focused, lines, 0);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let syslog = ctx.need_node_priv("Sys.Syslog");
        let exec = ctx.exec().is_some();
        let mut v: Vec<MenuItem> = PRESETS
            .iter()
            .map(|p| item(p.key, p.label).require(syslog.clone()).disabled_if(!exec && p.key != 'j', tr("local only")))
            .collect();
        v.push(item('u', "Logs of a unit...").require(syslog).disabled_if(!exec, tr("local only")));
        v
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        if key == 'u' {
            ctx.popup(Popup::Form(crate::ui::form::Form::new(
                "Logs of a unit",
                "unit",
                vec![crate::ui::form::text("unit", "Unit", "pveproxy").req(true)],
            )));
            return;
        }
        let Some(p) = PRESETS.iter().find(|p| p.key == key) else { return };
        let source = match ctx.exec() {
            Some(exec) => Source::Journal { exec, unit: p.unit.map(|s| s.to_string()), kernel: p.kernel, extra: p.extra.iter().map(|s| s.to_string()).collect() },
            None => Source::ApiJournal { api: ctx.api.clone(), node: ctx.node.clone() },
        };
        let lv = LogViewer::new(&format!("{} @ {}", p.label, ctx.node), source).with_prio(p.prio).with_range(p.range);
        ctx.popup(Popup::Log(Box::new(lv)));
    }

    fn on_form(&mut self, _tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let unit = super::fv(v, "unit").to_string();
        let src = unit_log_source(ctx, &unit);
        ctx.open_log(&format!("{unit} @ {}", ctx.node), src);
    }
}

// ===========================================================================
// Tasks
// ===========================================================================

#[derive(Default)]
struct Tasks {
    data: Loader<Vec<Value>>,
    table: Table,
    node_only: bool,
    errors_only: bool,
    scroll: u16,
}

impl Tasks {
    fn sel(&self) -> Option<&Value> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.iter().find(|t| js(t, "upid") == k)
    }
}

impl View for Tasks {
    fn title(&self) -> &'static str {
        "Tasks"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let node_only = self.node_only;
        let errors = self.errors_only;
        self.data.load(move || {
            let mut v = if node_only || errors {
                jarr(&api.get(&format!("/nodes/{node}/tasks?limit=500{}", if errors { "&errors=1" } else { "" }))?)
            } else {
                jarr(&api.get("/cluster/tasks")?)
            };
            v.sort_by_key(|t| (t.get("endtime").is_some() && js(t, "status") != "", -ji(t, "starttime")));
            Ok(v)
        });
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(5)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("c", "cluster/node"), ("f", "errors only")]
    }

    fn on_key(&mut self, key: &KeyEvent, ctx: &mut Ctx) -> bool {
        match key.code {
            KeyCode::Char('c') => {
                self.node_only = !self.node_only;
                self.data.clear();
                self.refresh(ctx);
                true
            }
            KeyCode::Char('f') => {
                self.errors_only = !self.errors_only;
                self.data.clear();
                self.refresh(ctx);
                true
            }
            _ => false,
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let now = chrono::Utc::now().timestamp();
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flatten()
            .map(|t| {
                let start = ji(t, "starttime");
                let end = ji(t, "endtime");
                let status = js(t, "status");
                let running = end == 0 && status.is_empty();
                let (st_text, st) = if running {
                    ("running".to_string(), Style::default().fg(ACCENT))
                } else if status == "OK" {
                    ("OK".into(), Style::default())
                } else if status.starts_with("WARNINGS") {
                    (status.clone(), style_warn())
                } else {
                    (status.clone(), style_bad())
                };
                let dur = if running { now - start } else { end - start };
                Row::new(js(t, "upid"))
                    .c(fmt_time(start))
                    .c(fmt_duration(dur.max(0) as u64))
                    .c(js(t, "node"))
                    .c(js(t, "user"))
                    .c(js(t, "type"))
                    .c(js(t, "id"))
                    .cs(st_text, st)
            })
            .collect();
        let cols = [
            col("Started", Constraint::Length(19)),
            col("Duration", Constraint::Length(9)),
            col("Node", Constraint::Length(12)),
            col("User", Constraint::Length(12)),
            col("Type", Constraint::Length(12)),
            col("ID", Constraint::Length(8)),
            col("Status", Constraint::Min(8)),
        ];
        let title = format!(
            "{} ({}{})",
            tr("Tasks"),
            if self.node_only || self.errors_only { "node" } else { "cluster" },
            if self.errors_only { ", errors" } else { "" }
        );
        self.table.draw(f, area, &title, &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(t) = self.sel() {
            kv.add("UPID", js(t, "upid"));
            kv.add("Type", js(t, "type"));
            kv.add("ID", js(t, "id"));
            kv.add("Node", js(t, "node"));
            kv.add("User", js(t, "user"));
            kv.add("Start", fmt_time(ji(t, "starttime")));
            if ji(t, "endtime") > 0 {
                kv.add("End", fmt_time(ji(t, "endtime")));
            }
            kv.status("Status", js(t, "status"));
            kv.blank();
            kv.styled("", "Enter: open live task log", style_dim());
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(t) = self.sel() else { return vec![] };
        let running = ji(t, "endtime") == 0 && js(t, "status").is_empty();
        let own = js(t, "user") == ctx.session.user;
        vec![
            item('v', "View log"),
            item('x', "Stop task").disabled_if(!running, "not running").disabled_if(!own && !ctx.session.can(&format!("/nodes/{}", js(t, "node")), "Sys.Modify"), "needs Sys.Modify"),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(t) = self.sel() else { return };
        let (node, upid) = (js(t, "node"), js(t, "upid"));
        match key {
            'v' => {
                let title = format!("{} {}", js(t, "type"), js(t, "id"));
                ctx.popup(Popup::Task(Box::new(TaskView::new(ctx.api.clone(), &node, &upid, &title))));
            }
            'x' => ctx.api_call("Stop task", "DELETE", format!("/nodes/{node}/tasks/{}", urlenc(&upid)), vec![]),
            _ => {}
        }
    }
}
