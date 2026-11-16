//! System > Health: every finding of the health checks (hardware first), the checks that
//! passed, acknowledgements and jumps to the page with the details.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    Frame,
};

use super::{scroll, View};
use crate::ctx::{ActionOutcome, Cmd, Ctx};
use crate::health::{Cat, Finding, Report, Sev};
use crate::i18n::tr;
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, style_bad, style_dim, style_good, style_label, style_warn};
use crate::util::fmt_time;

pub fn sev_style(s: Sev) -> Style {
    match s {
        Sev::Crit => style_bad(),
        Sev::Warn => style_warn(),
        Sev::Info => Style::default(),
        Sev::Ok => style_good(),
    }
}

pub fn sev_label(s: Sev) -> &'static str {
    match s {
        Sev::Crit => "Critical",
        Sev::Warn => "Warning",
        Sev::Info => "Info",
        Sev::Ok => "Normal",
    }
}

/// What a fix does.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Do {
    /// host shell script (after a confirmation when `danger`)
    Sh { danger: bool },
    /// show the output of a host command
    Show,
    /// open the journal of a unit / the kernel
    Log,
    /// apt-get in the terminal
    Apt,
    /// PVE API
    Api,
}

/// A fix offered for a finding: (key, label, kind, script / argument).
#[derive(Clone, Debug, PartialEq)]
pub struct Fix {
    pub key: char,
    pub label: &'static str,
    what: Do,
    arg: String,
}

fn fix(key: char, label: &'static str, what: Do, arg: impl Into<String>) -> Fix {
    Fix { key, label, what, arg: arg.into() }
}

/// Shell-quote a value taken from the host (unit names, pool names, mount points).
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Fixes for a finding (by its id). Everything here is a host or API action the console
/// already offers elsewhere; the health page just puts it next to the problem.
pub fn fixes(id: &str) -> Vec<Fix> {
    let sh = Do::Sh { danger: false };
    let danger = Do::Sh { danger: true };
    let (kind, rest) = id.split_once(':').unwrap_or((id, ""));
    match kind {
        "fs" => {
            let (what, mount) = rest.split_once(':').unwrap_or(("", rest));
            if what == "inodes" {
                return vec![fix('d', "Show the directories with the most files", Do::Show, format!("du --inodes -x -d 3 {} 2>/dev/null | sort -n | tail -n 30", q(mount)))];
            }
            vec![
                fix('d', "Show the largest directories", Do::Show, format!("du -xh -d 3 {} 2>/dev/null | sort -h | tail -n 30", q(mount))),
                fix('j', "Shrink the journal to 200 MB", sh, "journalctl --vacuum-size=200M 2>&1 | tail -n 1"),
                fix('a', "Clean the apt cache", sh, "apt-get clean && echo 'apt cache cleaned'"),
                fix('k', "Remove unused packages and old kernels (apt autoremove)", Do::Apt, "autoremove --purge"),
            ]
        }
        "unit" => vec![
            fix('l', "Show the log of the unit", Do::Log, rest),
            fix('R', "Restart the unit", danger, format!("systemctl restart {u} && systemctl is-active {u}", u = q(rest))),
            fix('x', "Reset the failed state", sh, format!("systemctl reset-failed {} && echo 'failed state reset'", q(rest))),
        ],
        "kern" => vec![fix('l', "Show the kernel log (warnings and errors)", Do::Log, "")],
        "time" => vec![
            fix('s', "Sync now (step the clock to NTP time)", danger, "chronyc -a makestep >/dev/null && chronyc -n tracking | grep 'System time'"),
            fix('S', "Restart the NTP service (chrony)", sh, "systemctl restart chrony && echo 'chrony restarted'"),
        ],
        "cluster" if rest == "quorum" => vec![fix('E', "Emergency: pvecm expected 1 (make this node quorate alone)", danger, "pvecm expected 1 && pvecm status | grep -E 'Quorate|Expected'")],
        "bmc" if rest == "no-ipmitool" => vec![fix('I', "Install ipmitool", Do::Apt, "install ipmitool")],
        "bmc" if rest == "sel:full" => vec![
            fix('w', "Export the event log to a file on the node", sh, super::bmc::BmcView::export_script()),
            fix('C', "Export, then clear the event log", danger, format!("f=$({}) && ipmitool sel clear >/dev/null && echo \"cleared, saved to $f\"", super::bmc::BmcView::export_script())),
        ],
        "zfs" => {
            let (what, pool) = rest.split_once(':').unwrap_or(("", rest));
            match what {
                "scrub" => vec![fix('s', "Start a scrub", sh, format!("zpool scrub {} && echo 'scrub started'", q(pool)))],
                "errors" => vec![
                    fix('v', "Show zpool status -v", Do::Show, format!("zpool status -v {}", q(pool))),
                    fix('c', "Clear the error counters (zpool clear)", danger, format!("zpool clear {} && echo 'error counters cleared'", q(pool))),
                ],
                "health" => vec![fix('v', "Show zpool status -v", Do::Show, format!("zpool status -v {}", q(pool)))],
                _ => vec![],
            }
        }
        "ceph" => vec![fix('v', "Show ceph health detail", Do::Show, "timeout 20 ceph health detail")],
        "guest" if rest.starts_with("lock:") => vec![fix('U', "Unlock the guest", Do::Api, rest.trim_start_matches("lock:"))],
        "ha" => vec![fix('e', "Clear the error state (disable, then request started)", Do::Api, rest)],
        "corosync" => vec![fix('v', "Show corosync-cfgtool -s", Do::Show, "corosync-cfgtool -s")],
        "nic" => vec![fix('v', "Show ip -s link", Do::Show, format!("ip -s link show {}", q(rest.split_once(':').map(|x| x.1).unwrap_or(rest))))],
        "mem" => vec![fix('v', "Show the biggest memory users", Do::Show, "ps -eo pid,rss,comm --sort=-rss | head -n 20")],
        _ => vec![],
    }
}

#[derive(Default)]
pub struct Health {
    /// fix waiting for its confirmation: (finding id, fix)
    pending: Option<(String, Fix)>,
    table: Table,
    scroll: u16,
    shown: Option<Arc<Report>>,
}

impl Health {
    fn report<'a>(&self, ctx: &'a Ctx) -> Option<&'a Report> {
        ctx.health.as_deref()
    }

    fn selected<'a>(&self, ctx: &'a Ctx) -> Option<&'a Finding> {
        let k = self.table.selected()?;
        self.report(ctx)?.findings.iter().find(|f| f.id == k)
    }

    fn jump(&self, ctx: &mut Ctx) {
        let target = match self.selected(ctx) {
            Some(f) => f.jump.unwrap_or_else(|| f.cat.jump()),
            None => match self.table.selected().and_then(|k| k.strip_prefix("ok:")).and_then(|k| Cat::ALL.iter().find(|c| format!("{c:?}") == k.split(':').next().unwrap_or(""))) {
                Some(c) => c.jump(),
                None => return,
            },
        };
        ctx.cmds.push(Cmd::Goto(target.0, target.1));
    }
}

impl Health {
    fn apply(&mut self, x: &Finding, fx: Fix, ctx: &mut Ctx) {
        let title = format!("{}: {}", tr(fx.label), x.title.en());
        let exec = match (fx.what, ctx.exec()) {
            (_, Some(e)) => e,
            (Do::Api, None) => crate::exec::Exec::Local, // not used by API fixes
            (_, None) => return,
        };
        match fx.what {
            Do::Sh { .. } => {
                let script = fx.arg.clone();
                ctx.act(title, move || {
                    let out = exec.output("sh", &["-c", &script])?;
                    Ok(ActionOutcome::Msg(out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("OK").trim().to_string()))
                });
                // the problem may be gone: check again soon
                ctx.health_now = true;
            }
            Do::Show => {
                let script = fx.arg.clone();
                let t = title.clone();
                ctx.act(title, move || {
                    let (_, out) = exec.output_any("sh", &["-c", &script])?;
                    Ok(ActionOutcome::Text(t, out))
                });
            }
            Do::Log => {
                let (unit, kernel, extra) = if fx.arg.is_empty() { (None, true, vec!["-p".to_string(), "warning".to_string(), "-b".to_string()]) } else { (Some(fx.arg.clone()), false, vec![]) };
                let t = format!("{} @ {}", unit.clone().unwrap_or_else(|| "kernel".into()), ctx.node);
                ctx.open_log(&t, crate::ui::logview::Source::Journal { exec, unit, kernel, extra });
            }
            Do::Apt => {
                let args: Vec<String> = fx.arg.split_whitespace().map(|s| s.to_string()).collect();
                let banner = format!("apt-get {} on {}\n", fx.arg, ctx.node);
                crate::modules::updates::Updates::suspend_apt(ctx, args, banner);
            }
            Do::Api => {
                let n = ctx.node.clone();
                if x.id.starts_with("guest:lock:") {
                    // "qemu/105" / "lxc/200"
                    ctx.api_call(format!("Unlock {}", fx.arg), "PUT", format!("/nodes/{n}/{}/config", fx.arg), vec![("delete", "lock".into()), ("skiplock", "1".into())]);
                } else if x.id.starts_with("ha:") {
                    let api = ctx.api.clone();
                    let sid = fx.arg.clone();
                    ctx.act(title, move || {
                        let p = format!("/cluster/ha/resources/{sid}");
                        api.put(&p, &[("state", "disabled".to_string())])?;
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        api.put(&p, &[("state", "started".to_string())])?;
                        Ok(ActionOutcome::Msg(format!("{sid}: error cleared, start requested")))
                    });
                }
            }
        }
    }
}

impl View for Health {
    fn title(&self) -> &'static str {
        "Health"
    }

    /// The report comes from the app's background check (every 30 s; SMART / BMC every 10 min).
    fn refresh(&mut self, _ctx: &mut Ctx) {}

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let changed = match (&self.shown, &ctx.health) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (None, None) => false,
            _ => true,
        };
        self.shown = ctx.health.clone();
        changed
    }

    fn refresh_secs(&self) -> Option<u64> {
        None
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("Enter", "open page")]
    }

    fn on_key(&mut self, key: &KeyEvent, ctx: &mut Ctx) -> bool {
        if key.code == KeyCode::Enter {
            self.jump(ctx);
            return true;
        }
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut rows: Vec<Row> = Vec::new();
        let status = match self.report(ctx) {
            None => Some((tr("Checking...").to_string(), style_dim())),
            Some(r) => {
                for x in &r.findings {
                    let sev = if x.acked { Style::default().fg(crate::ui::DIM) } else { sev_style(x.sev) };
                    let mark = if x.acked { format!("{} ✓", tr(sev_label(x.sev))) } else { tr(sev_label(x.sev)).to_string() };
                    rows.push(Row::new(x.id.clone()).cs(mark, sev).c(x.title.text()).c(x.value.clone()).c(tr(x.cat.label())).dim(x.acked));
                }
                for (c, what) in &r.passed {
                    rows.push(Row::new(format!("ok:{c:?}:{}", what.en())).cs(tr("Normal"), style_good()).c(what.text()).c("").c(tr(c.label())));
                }
                None
            }
        };
        let cols = [col("Status", Constraint::Length(9)), col("Check", Constraint::Min(26)), col("Value", Constraint::Length(20)), col("Area", Constraint::Length(16))];
        let title = match self.report(ctx) {
            Some(r) => format!("{} · {} {} · {} {}", tr("Health"), r.count(Sev::Crit), tr("critical"), r.count(Sev::Warn), tr("warnings")),
            None => tr("Health").to_string(),
        };
        self.table.draw(f, area, &title, &cols, &rows, focused, status);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let lab = style_label();
        if let Some(x) = self.selected(ctx) {
            lines.push(Line::from(vec![Span::styled(format!("{}  ", tr(sev_label(x.sev))), sev_style(x.sev)), Span::raw(x.title.text())]));
            lines.push(Line::from(vec![Span::styled(format!("{}: ", tr("Area")), lab), Span::raw(tr(x.cat.label()).to_string())]));
            if !x.value.is_empty() {
                lines.push(Line::from(vec![Span::styled(format!("{}: ", tr("Value")), lab), Span::raw(x.value.clone())]));
            }
            for d in &x.detail {
                lines.push(Line::from(Span::raw(format!("  {d}"))));
            }
            if !x.advice.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(tr("What to do"), lab)));
                lines.push(Line::from(Span::raw(tr(&x.advice).to_string())));
            }
            let fx = fixes(&x.id);
            if !fx.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(format!("{} (f)", tr("Fixes")), lab)));
                for f in &fx {
                    lines.push(Line::from(format!("  {}  {}", f.key, tr(f.label))));
                }
            }
            if x.acked {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(tr("Acknowledged: not counted as an alert until it gets worse."), style_dim())));
            }
            let (_, t) = x.jump.unwrap_or_else(|| x.cat.jump());
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(format!("Enter → {}", tr(t)), style_dim())));
            lines.push(Line::from(""));
        }
        // summary by area
        if let Some(r) = self.report(ctx) {
            lines.push(Line::from(Span::styled(tr("Summary"), lab)));
            for c in Cat::ALL {
                let worst = r.findings.iter().filter(|x| x.cat == c && !x.acked).map(|x| x.sev).max();
                let checked = worst.is_some() || r.passed.iter().any(|p| p.0 == c) || r.findings.iter().any(|x| x.cat == c);
                let (txt, st) = match worst {
                    Some(s) if s >= Sev::Info => (tr(sev_label(s)).to_string(), sev_style(s)),
                    _ if checked => (tr("Normal").to_string(), style_good()),
                    _ => ("—".to_string(), style_dim()),
                };
                lines.push(Line::from(vec![Span::raw(format!("  {:<20}", tr(c.label()))), Span::styled(txt, st)]));
            }
            for e in &r.errors {
                lines.push(Line::from(Span::styled(e.clone(), style_bad())));
            }
            if let Some(at) = r.at.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(format!("{} {}", tr("Checked at"), fmt_time(at.as_secs() as i64)), style_dim())));
                lines.push(Line::from(Span::styled(tr("Host checks every minute, SMART and BMC every 10 minutes."), style_dim())));
            }
        }
        draw_lines(f, area, "Details", focused, lines, self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if tag != "fix" {
            return;
        }
        let Some((id, fx)) = self.pending.take() else { return };
        let Some(x) = self.report(ctx).and_then(|r| r.findings.iter().find(|f| f.id == id)).cloned() else { return };
        self.apply(&x, fx, ctx);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let mut v = vec![item('o', "Open related page")];
        if let Some(x) = self.selected(ctx) {
            if x.acked {
                v.push(item('u', "Remove acknowledgement"));
            } else {
                v.push(item('a', "Acknowledge (hide from alerts until it gets worse)").disabled_if(x.sev < Sev::Warn, "only warnings and critical findings"));
            }
        }
        if let Some(x) = self.selected(ctx) {
            let n = fixes(&x.id).len();
            let api_only = fixes(&x.id).iter().all(|f| f.what == Do::Api);
            v.insert(1, item('f', "Fix...").disabled_if(n == 0, "no automatic fix for this finding").require(ctx.need_root()).disabled_if(ctx.exec().is_none() && !api_only, crate::ctx::NEEDS_SSH));
        }
        v.push(item('c', "Check now (all checks, SMART and BMC included)"));
        v
    }

    fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        if tag == "fix" {
            let Some(x) = self.selected(ctx).cloned() else { return };
            let Some(fx) = fixes(&x.id).into_iter().find(|f| f.key == key) else { return };
            if let Do::Sh { danger: true } = fx.what {
                let typed = x.id == "cluster:quorum";
                let body = vec![x.title.text(), tr(fx.label).to_string()];
                self.pending = Some((x.id.clone(), fx));
                ctx.popup(if typed { Popup::confirm_typed("Fix", "fix", body, &ctx.node.clone()) } else { Popup::confirm("Fix", "fix", body, true) });
            } else {
                self.apply(&x, fx, ctx);
            }
            return;
        }
        match key {
            'o' => self.jump(ctx),
            'f' => {
                let Some(x) = self.selected(ctx) else { return };
                let items: Vec<MenuItem> = fixes(&x.id).iter().map(|f| item(f.key, f.label)).collect();
                if !items.is_empty() {
                    let title = format!("{}: {}", tr("Fix"), x.title.text());
                    ctx.popup(Popup::menu(&title, "fix", items));
                }
            }
            'a' | 'u' => {
                let Some(x) = self.selected(ctx).cloned() else { return };
                match crate::health::set_ack(&ctx.node, &x, key == 'a') {
                    Ok(()) => {
                        // show at once, the next check applies it as well
                        if let Some(r) = &ctx.health {
                            let mut r2 = (**r).clone();
                            if let Some(f) = r2.findings.iter_mut().find(|f| f.id == x.id) {
                                f.acked = key == 'a';
                            }
                            ctx.health = Some(Arc::new(r2));
                        }
                        ctx.toast(if key == 'a' { "acknowledged" } else { "acknowledgement removed" });
                    }
                    Err(e) => ctx.toast_err(format!("cannot save: {e}")),
                }
            }
            'c' => {
                ctx.health_now = true;
                ctx.toast("checking...");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_by_finding() {
        let keys = |id: &str| fixes(id).iter().map(|f| f.key).collect::<String>();
        assert_eq!(keys("fs:space:/"), "djak");
        assert_eq!(keys("unit:pxvirt-spdk.service"), "lRx");
        assert_eq!(keys("zfs:scrub:rpool"), "s");
        assert_eq!(keys("guest:lock:qemu/105"), "U");
        assert_eq!(fixes("guest:lock:qemu/105")[0].arg, "qemu/105");
        assert_eq!(keys("cluster:node:pve2"), "");
        // values from the host are quoted
        assert!(fixes("fs:space:/var/a'b")[0].arg.contains("'/var/a'\\''b'"));
        // fix keys are unique per finding
        for id in ["fs:space:/", "unit:x", "time:offset", "bmc:sel:full", "zfs:errors:p", "kern:io", "ha:vm:100"] {
            let mut k: Vec<char> = keys(id).chars().collect();
            let n = k.len();
            k.sort();
            k.dedup();
            assert_eq!(k.len(), n, "{id}");
        }
    }

    #[test]
    fn jump_targets_exist() {
        let modules = crate::modules::build_modules();
        let src = include_str!("../health.rs");
        let mut targets: Vec<(usize, String)> = Cat::ALL.iter().map(|c| (c.jump().0, c.jump().1.to_string())).collect();
        // explicit .jump(m, "Title") in the checks
        for part in src.split(".jump(").skip(1) {
            let Some((m, rest)) = part.split_once(", \"") else { continue };
            let (Ok(m), Some((t, _))) = (m.parse::<usize>(), rest.split_once('"')) else { continue };
            targets.push((m, t.to_string()));
        }
        for (m, t) in targets {
            assert!(modules[m].views.iter().any(|v| v.title() == t), "no page {t} in module {}", modules[m].title);
        }
    }
}
