//! Disks → Hardware RAID: controllers, virtual and physical drives of every RAID / HBA / NVMe
//! tool found on the host (pxvirt-raid). Actions come from the tool specs; the confirmation
//! depends on their risk (view: run, low: confirm, medium: confirm with the command line,
//! high: type the object id).

use std::collections::HashMap;

use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    Frame,
};

use pxvirt_raid::{ActionSpec, Controller, Found, Health, ParamKind, PhysicalDrive, Risk, Role, Target, VirtualDrive};

use super::{fv, scroll, View};
use crate::ctx::{ActionOutcome, Ctx};
use crate::i18n::tr;
use crate::loader::Loader;
use crate::raidhost::{scan_host, Host};
use crate::ui::form::{self, Field, Form};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, style_bad, style_dim, style_good, style_warn, Kv, ACCENT};

#[derive(Default)]
pub struct HwRaid {
    data: Loader<Host>,
    table: Table,
    scroll: u16,
    pending: Option<Pending>,
    /// locate LEDs switched from this console (for controllers that do not report them)
    lit: HashMap<String, bool>,
}

/// An action waiting for its form / confirmation.
struct Pending {
    /// tool binary, or "ses:<slot dir>" to switch the LED through the SES enclosure
    path: String,
    action: ActionSpec,
    addr: Vec<(String, String)>,
    params: Vec<(String, String)>,
    object: String,
    drive_sep: &'static str,
    /// key of the drive in `lit` when the action switches its locate LED
    led_key: String,
}

/// Locate LED through the kernel SES enclosure driver (when the tool has no LED command).
const SES_ON: ActionSpec = pxvirt_raid::spec::act('L', "Locate LED on", Target::Pd, Risk::Low, &[]).led(true);
const SES_OFF: ActionSpec = pxvirt_raid::spec::act('K', "Locate LED off", Target::Pd, Risk::Low, &[]).led(false);

fn led_key(f: &Found, c: &Controller, p: &PhysicalDrive) -> String {
    format!("{}|{}|{}", f.spec.id, c.id, p.id)
}

/// Is the drive a member of the virtual drive? (members list, group name or array letter)
fn member_of(v: &VirtualDrive, p: &PhysicalDrive) -> bool {
    if v.members.iter().any(|m| *m == p.token || *m == p.id || (!p.serial.is_empty() && m.contains(&p.serial))) {
        return true;
    }
    if !p.group.is_empty() && (p.group == format!("VD {}", v.id) || p.group == format!("Volume {}", v.id)) {
        return true;
    }
    let arr = |a: &[(String, String)]| a.iter().find(|(k, _)| k == "array").map(|x| x.1.clone()).unwrap_or_default();
    let va = arr(&v.addr);
    !va.is_empty() && va == arr(&p.addr) && p.role == Role::Member
}

enum Obj<'a> {
    Ctl(&'a Found, &'a Controller),
    Vd(&'a Found, &'a Controller, &'a VirtualDrive),
    Pd(&'a Found, &'a Controller, &'a PhysicalDrive),
    Missing(usize),
    Error(usize),
}

fn hstyle(h: Health) -> Style {
    match h {
        Health::Ok => style_good(),
        Health::Warning => style_warn(),
        Health::Critical => style_bad(),
        Health::Unknown => style_dim(),
    }
}

fn dot(h: Health) -> &'static str {
    match h {
        Health::Ok => "●",
        Health::Warning => "▲",
        Health::Critical => "✖",
        Health::Unknown => "○",
    }
}

impl HwRaid {
    fn host(&self) -> Option<&Host> {
        self.data.data.as_ref()
    }

    fn obj(&self) -> Option<Obj<'_>> {
        let k = self.table.selected()?;
        let h = self.host()?;
        let kind = k.split('|').next()?;
        let p: Vec<usize> = k.split('|').skip(1).filter_map(|x| x.parse().ok()).collect();
        match kind {
            "m" => return Some(Obj::Missing(*p.first()?)),
            "e" => return Some(Obj::Error(*p.first()?)),
            _ => {}
        }
        let (f, ctls) = h.ctls.get(*p.first()?).map(|(f, c)| (f, c))?;
        let c = ctls.get(*p.get(1)?)?;
        match kind {
            "c" => Some(Obj::Ctl(f, c)),
            "v" => Some(Obj::Vd(f, c, c.vds.get(*p.get(2)?)?)),
            "p" => Some(Obj::Pd(f, c, c.pds.get(*p.get(2)?)?)),
            _ => None,
        }
    }

    /// (tool, controller, target, address, role, object label) of the selection
    fn target(&self) -> Option<(&Found, &Controller, Target, Vec<(String, String)>, Option<Role>, String)> {
        Some(match self.obj()? {
            Obj::Ctl(f, c) => (f, c, Target::Controller, c.addr.clone(), None, if c.id.chars().all(|x| x.is_ascii_digit()) { format!("c{}", c.id) } else { c.id.clone() }),
            Obj::Vd(f, c, v) => (f, c, Target::Vd, v.addr.clone(), None, v.id.clone()),
            Obj::Pd(f, c, p) => (f, c, Target::Pd, p.addr.clone(), Some(p.role), p.id.clone()),
            _ => return None,
        })
    }

    fn pd_row(&self, fd: &Found, c: &Controller, i: usize, ci: usize, pi: usize, branch: &str, io: &dyn Fn(&str) -> String) -> Row {
        let p = &c.pds[pi];
        let mut info = vec![p.group.clone(), io(&p.os_dev)];
        if let Some(t) = p.temp_c {
            info.push(format!("{t}°C"));
        }
        match self.led(fd, c, p) {
            (Some(true), _) => info.push(format!("◉ {}", tr("locate LED on"))),
            _ => {}
        }
        if p.fault_led == Some(true) {
            info.push(format!("✖ {}", tr("fault LED on")));
        }
        info.push(p.progress.clone());
        let lit = matches!(self.led(fd, c, p), (Some(true), _));
        Row::new(format!("p|{i}|{ci}|{pi}"))
            .cs(dot(p.health), hstyle(p.health))
            .c(format!(" {branch} {}", p.id))
            .c(p.model.clone())
            .c(format!("{} {}", p.intf, p.media).trim().to_string())
            .c(p.size.clone())
            .cs(p.state.clone(), hstyle(p.health))
            .cs(info.into_iter().filter(|s| !s.is_empty() && s != "-").collect::<Vec<_>>().join("  "), if lit { style_warn() } else { Style::default() })
    }

    /// Known locate LED state of a drive: SES enclosure first, else what this console switched.
    fn led(&self, f: &Found, c: &Controller, p: &PhysicalDrive) -> (Option<bool>, &'static str) {
        match p.locate {
            Some(on) => (Some(on), "SES"),
            None => match self.lit.get(&led_key(f, c, p)) {
                Some(on) => (Some(*on), "switched here"),
                None => (None, ""),
            },
        }
    }

    fn actions(&self) -> Vec<(ActionSpec, String)> {
        let Some((f, _, t, _, role, _)) = self.target() else { return vec![] };
        let mut v: Vec<(ActionSpec, String)> = f.spec.actions_for(t).filter(|a| role.map(|r| a.applies(r)).unwrap_or(true)).map(|a| (*a, f.path.clone())).collect();
        if let Some(Obj::Pd(f, c, p)) = self.obj() {
            if !p.ses.is_empty() && !v.iter().any(|(a, _)| a.led.is_some()) {
                let at = v.iter().position(|(a, _)| a.risk != Risk::View).unwrap_or(v.len());
                v.insert(at, (SES_OFF, format!("ses:{}", p.ses)));
                v.insert(at, (SES_ON, format!("ses:{}", p.ses)));
            }
            // only offer the LED switch that changes something
            if let (Some(on), _) = self.led(f, c, p) {
                v.retain(|(a, _)| a.led != Some(on));
            }
        }
        v
    }

    fn start(&mut self, a: ActionSpec, path: String, ctx: &mut Ctx) {
        let Some((f, c, _, addr, _, object)) = self.target() else { return };
        let drive_sep = f.spec.drive_sep;
        let ircu = matches!(f.spec.dialect, pxvirt_raid::Dialect::Ircu);
        let free: Vec<(String, String)> = c
            .pds
            .iter()
            .filter(|p| p.role == Role::Unconfigured || (ircu && p.role == Role::Jbod))
            .map(|p| (p.token.replace(' ', "_"), format!("{}  {} {} {}", p.id, p.model, p.size, p.media)))
            .collect();
        let led_key = match (a.led, self.obj()) {
            (Some(_), Some(Obj::Pd(f, c, p))) => led_key(f, c, p),
            _ => String::new(),
        };
        self.pending = Some(Pending { path, action: a, addr, params: vec![], object, drive_sep, led_key });
        if a.params.is_empty() {
            self.confirm(ctx);
            return;
        }
        let mut fields: Vec<Field> = vec![form::info("Object", &self.pending.as_ref().map(|p| p.object.clone()).unwrap_or_default())];
        for p in a.params {
            fields.push(match p.kind {
                ParamKind::Choice(o) => form::select(p.key, p.label, o.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(), o.first().map(|x| x.0).unwrap_or("")),
                ParamKind::Text(d) => form::text(p.key, p.label, d).req(p.required),
                ParamKind::Number(d) => form::number(p.key, p.label, d).req(p.required),
                ParamKind::Drives => {
                    if free.is_empty() {
                        ctx.popup(Popup::error(a.label, "no unconfigured drives on this controller"));
                        self.pending = None;
                        return;
                    }
                    form::multi(p.key, p.label, free.clone(), "").help("Space selects")
                }
            });
        }
        ctx.popup(Popup::Form(Form::new(a.label, "raid:form", fields)));
    }

    /// Ask according to the risk, or run view actions right away.
    fn confirm(&mut self, ctx: &mut Ctx) {
        let Some(p) = &self.pending else { return };
        let cmds = match Self::commands(p) {
            Ok(c) => c,
            Err(e) => {
                ctx.popup(Popup::error(p.action.label, &format!("{e:#}")));
                self.pending = None;
                return;
            }
        };
        let lines: Vec<String> = cmds.iter().map(|c| format!("$ {} {}", p.path, c.join(" "))).collect();
        match p.action.risk {
            Risk::View => self.run(ctx),
            Risk::Low => ctx.popup(Popup::confirm(p.action.label, "raid:run", vec![format!("{}: {}", tr(p.action.label), p.object)], false)),
            Risk::Medium => {
                let mut body = vec![format!("{}: {}", tr(p.action.label), p.object), String::new()];
                body.extend(lines);
                ctx.popup(Popup::confirm(p.action.label, "raid:run", body, true));
            }
            Risk::High => {
                let mut body = vec![format!("{}: {}", tr(p.action.label), p.object), tr("This can destroy data.").to_string(), String::new()];
                body.extend(lines);
                ctx.popup(Popup::confirm_typed(p.action.label, "raid:run", body, &p.object));
            }
        }
    }

    /// Command lines of the pending action (SES: write the slot's locate file).
    fn commands(p: &Pending) -> anyhow::Result<Vec<Vec<String>>> {
        if let Some(dir) = p.path.strip_prefix("ses:") {
            let v = if p.action.led == Some(true) { "1" } else { "0" };
            return Ok(vec![vec!["-c".into(), "echo $0 > \"$1\"".into(), v.into(), format!("{dir}/locate")]]);
        }
        pxvirt_raid::commands(&p.action, &p.addr, &p.params)
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let Some(p) = self.pending.take() else { return };
        let Some(exec) = ctx.exec() else {
            ctx.popup(Popup::error(p.action.label, "needs local or ssh access to the node"));
            return;
        };
        let cmds = match Self::commands(&p) {
            Ok(c) => c,
            Err(e) => return ctx.popup(Popup::error(p.action.label, &format!("{e:#}"))),
        };
        if !p.led_key.is_empty() {
            if let Some(on) = p.action.led {
                self.lit.insert(p.led_key.clone(), on);
            }
        }
        let title = format!("{} · {}", tr(p.action.label), p.object);
        if p.action.risk != Risk::View {
            for c in &cmds {
                ctx.audit(&format!("raid: {} {}", p.path, c.join(" ")));
            }
        }
        let path = if p.path.starts_with("ses:") { "sh".to_string() } else { p.path.clone() };
        let t2 = title.clone();
        ctx.act(title, move || {
            let mut out = String::new();
            for c in cmds {
                let a: Vec<&str> = c.iter().map(|s| s.as_str()).collect();
                let (_, o) = exec.output_any(&path, &a)?;
                out.push_str(&format!("$ {path} {}\n{o}\n", c.join(" ")));
            }
            Ok(ActionOutcome::Text(t2, out))
        });
        if p.action.risk != Risk::View {
            // state changed: rescan soon
            self.data.at = None;
        }
    }
}

impl View for HwRaid {
    fn title(&self) -> &'static str {
        "Hardware RAID"
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else {
            self.data.error = Some(tr("needs local or ssh access to the node").to_string());
            return;
        };
        self.data.load(move || scan_host(&exec));
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(30)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let m = ctx.metrics.lock().unwrap().clone();
        let io = |dev: &str| -> String {
            match m.disk(dev) {
                Some(s) if !dev.is_empty() => format!("R {} W {} {:.0}%", crate::util::fmt_rate(s.r_bps), crate::util::fmt_rate(s.w_bps), s.util * 100.0),
                _ => String::new(),
            }
        };
        let mut rows = Vec::new();
        if let Some(h) = self.host() {
            for (i, (fd, ctls)) in h.ctls.iter().enumerate() {
                for (ci, c) in ctls.iter().enumerate() {
                    let mut info = vec![c.mode.clone()];
                    if let Some(b) = &c.battery {
                        info.push(format!("{} {}", b.kind, b.state));
                    }
                    info.extend(c.tasks.iter().cloned());
                    let ov = c.overall();
                    rows.push(
                        Row::new(format!("c|{i}|{ci}"))
                            .cs(dot(ov), hstyle(ov))
                            .cs(format!("#{}", c.id), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
                            .cs(c.model.clone(), Style::default().fg(ACCENT))
                            .cs(fd.spec.id, style_dim())
                            .c("")
                            .cs(c.status.clone(), hstyle(c.health))
                            .c(info.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ")),
                    );
                    // each virtual drive is a small box with its member drives,
                    // the remaining drives are boxed by role
                    let nvme = fd.spec.dialect == pxvirt_raid::Dialect::Nvme;
                    let mut used = vec![false; c.pds.len()];
                    let head = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
                    for (vi, v) in c.vds.iter().enumerate() {
                        let members: Vec<usize> = (0..c.pds.len()).filter(|&pi| !used[pi] && member_of(v, &c.pds[pi])).collect();
                        for &pi in &members {
                            used[pi] = true;
                        }
                        let info = [v.os_dev.clone(), v.name.clone(), v.progress.clone(), io(&v.os_dev)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ");
                        rows.push(
                            Row::new(format!("v|{i}|{ci}|{vi}"))
                                .cs(dot(v.health), hstyle(v.health))
                                .cs(format!(" {} {} {}", if members.is_empty() { "─" } else { "┌" }, if nvme { "NS" } else { "VD" }, v.id), head)
                                .cs(v.name.clone(), head)
                                .cs(v.level.clone(), head)
                                .c(v.size.clone())
                                .cs(v.state.clone(), hstyle(v.health))
                                .c(info),
                        );
                        for (n, &pi) in members.iter().enumerate() {
                            rows.push(self.pd_row(fd, c, i, ci, pi, if n + 1 == members.len() { "└" } else { "│" }, &io));
                        }
                    }
                    let groups: [(&str, &[Role]); 5] = [
                        ("Failed drives", &[Role::Failed]),
                        ("Hot spares", &[Role::HotSpare]),
                        ("Unconfigured drives", &[Role::Unconfigured]),
                        (if nvme { "Drive" } else { "JBOD / pass-through" }, &[Role::Jbod]),
                        ("Other drives", &[Role::Member, Role::Other]),
                    ];
                    for (gi, (label, roles)) in groups.iter().enumerate() {
                        let list: Vec<usize> = (0..c.pds.len()).filter(|&pi| !used[pi] && roles.contains(&c.pds[pi].role)).collect();
                        if list.is_empty() {
                            continue;
                        }
                        let worst = list.iter().map(|&pi| c.pds[pi].health).max().unwrap_or(Health::Unknown);
                        rows.push(
                            Row::new(format!("g|{i}|{ci}|{gi}"))
                                .cs(dot(worst), hstyle(worst))
                                .cs(" ┌", Style::default().add_modifier(Modifier::BOLD))
                                .cs(format!("{} · {}", tr(label), list.len()), Style::default().add_modifier(Modifier::BOLD))
                                .c("")
                                .c("")
                                .c("")
                                .c(""),
                        );
                        for (n, &pi) in list.iter().enumerate() {
                            rows.push(self.pd_row(fd, c, i, ci, pi, if n + 1 == list.len() { "└" } else { "│" }, &io));
                        }
                    }
                }
            }
            for (k, (d, s)) in h.det.missing.iter().enumerate() {
                rows.push(
                    Row::new(format!("m|{k}"))
                        .cs("○", style_dim())
                        .c(d.addr.clone())
                        .c(s.name)
                        .cs(d.driver.clone(), style_dim())
                        .c("")
                        .cs(tr("no tool"), style_warn())
                        .cs(s.install, style_dim()),
                );
            }
            for (k, e) in h.errors.iter().enumerate() {
                rows.push(Row::new(format!("e|{k}")).cs("✖", style_bad()).c(tr("Error")).cs(e.clone(), style_bad()).c("").c("").c("").c(""));
            }
        }
        let status = match (&self.data.error, self.host()) {
            (Some(_), _) => loading_or_error(&self.data.error, false),
            (None, Some(h)) if h.det.pci.is_empty() && h.ctls.is_empty() => Some((tr("No RAID / HBA / NVMe controller found").to_string(), style_dim())),
            (None, None) => loading_or_error(&None, true),
            _ => None,
        };
        let cols = [
            col("", Constraint::Length(1)),
            col("ID", Constraint::Length(13)),
            col("Name", Constraint::Min(22)),
            col("Type", Constraint::Length(10)),
            col("Size", Constraint::Length(11)),
            col("State", Constraint::Length(16)),
            col("Info", Constraint::Min(16)),
        ];
        self.table.draw(f, area, "Hardware RAID", &cols, &rows, focused, status);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        match self.obj() {
            Some(Obj::Ctl(fd, c)) => {
                kv.section("Controller");
                kv.add("Tool", format!("{}{}", fd.spec.name, if fd.spec.tested { "" } else { "  (parser not verified on hardware)" }));
                kv.add("Path", fd.path.clone());
                for (k, v) in [("Model", &c.model), ("Serial", &c.serial), ("Firmware", &c.firmware), ("Driver", &c.driver), ("PCI", &c.pci), ("Mode", &c.mode)] {
                    if !v.is_empty() {
                        kv.add(k, v.clone());
                    }
                }
                kv.styled("Status", c.status.clone(), hstyle(c.health));
                if let Some(b) = &c.battery {
                    kv.styled(&b.kind, b.state.clone(), hstyle(b.health));
                }
                for t in &c.tasks {
                    kv.add("Task", t.clone());
                }
                if c.foreign > 0 {
                    kv.styled("Foreign", format!("{} configuration(s)", c.foreign), style_warn());
                }
                kv.add("Drives", format!("{} virtual, {} physical", c.vds.len(), c.pds.len()));
                let probs = c.problems();
                if !probs.is_empty() {
                    kv.section("Problems");
                    for p in probs {
                        kv.styled("", p.text, hstyle(p.health));
                    }
                }
                kv.section("Properties");
                for (k, v) in &c.props {
                    kv.add(k, v.clone());
                }
            }
            Some(Obj::Vd(_, _, v)) => {
                kv.section("Virtual drive");
                kv.add("ID", v.id.clone());
                for (k, x) in [("Name", &v.name), ("Level", &v.level), ("Size", &v.size), ("Cache", &v.cache), ("OS device", &v.os_dev), ("Operation", &v.progress)] {
                    if !x.is_empty() {
                        kv.add(k, x.clone());
                    }
                }
                kv.styled("State", v.state.clone(), hstyle(v.health));
                if !v.members.is_empty() {
                    kv.add("Members", v.members.join(", "));
                }
                kv.section("Properties");
                for (k, x) in &v.props {
                    kv.add(k, x.clone());
                }
            }
            Some(Obj::Pd(_, _, p)) => {
                kv.section("Physical drive");
                kv.add("ID", p.id.clone());
                for (k, x) in [("Model", &p.model), ("Serial", &p.serial), ("Firmware", &p.firmware), ("Size", &p.size), ("Media", &p.media), ("Interface", &p.intf), ("Group", &p.group), ("OS device", &p.os_dev), ("Operation", &p.progress)] {
                    if !x.is_empty() {
                        kv.add(k, x.clone());
                    }
                }
                kv.add("Role", tr(p.role.label()));
                kv.styled("State", p.state.clone(), hstyle(p.health));
                if let Some(Obj::Pd(fd, c, _)) = self.obj() {
                    match self.led(fd, c, p) {
                        (Some(on), src) => kv.styled("Locate LED", format!("{}  ({})", tr(if on { "on" } else { "off" }), tr(src)), if on { style_warn() } else { Style::default() }),
                        (None, _) => kv.styled("Locate LED", tr("unknown (the controller does not report it)"), style_dim()),
                    };
                }
                if let Some(fl) = p.fault_led {
                    kv.styled("Fault LED", tr(if fl { "on" } else { "off" }), if fl { style_bad() } else { Style::default() });
                }
                if !p.ses.is_empty() {
                    kv.add("Enclosure slot", p.ses.clone());
                }
                if let Some(t) = p.temp_c {
                    kv.add("Temperature", format!("{t} °C"));
                }
                kv.section("Properties");
                for (k, x) in &p.props {
                    kv.add(k, x.clone());
                }
            }
            Some(Obj::Missing(k)) => {
                if let Some((d, s)) = self.host().and_then(|h| h.det.missing.get(k)) {
                    kv.section("Controller");
                    kv.add("PCI", d.addr.clone());
                    kv.add("Vendor / device", format!("{} {} (subsystem {} {})", d.vendor, d.device, d.subvendor, d.subdevice));
                    kv.add("Driver", d.driver.clone());
                    kv.blank();
                    kv.styled("", tr("No management tool installed for this controller."), style_warn());
                    kv.add("Install", s.install);
                }
            }
            Some(Obj::Error(k)) => {
                if let Some(e) = self.host().and_then(|h| h.errors.get(k)) {
                    kv.styled("", e.clone(), style_bad());
                }
            }
            None => {
                if let Some(h) = self.host() {
                    kv.add("Tools", h.det.tools.iter().map(|t| t.spec.id).collect::<Vec<_>>().join(", "));
                }
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu_title(&self) -> String {
        self.target().map(|t| t.5).unwrap_or_else(|| tr("Actions").to_string())
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        self.actions()
            .into_iter()
            .map(|(a, _)| {
                let label = match a.risk {
                    Risk::High => format!("{} !", tr(a.label)),
                    _ => a.label.to_string(),
                };
                let it = item(a.key, &label);
                if a.risk == Risk::View {
                    it.require(ctx.need_node_priv("Sys.Audit"))
                } else {
                    it.require(ctx.need_node_priv("Sys.Modify"))
                }
            })
            .collect()
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        if let Some((a, path)) = self.actions().into_iter().find(|(a, _)| a.key == key) {
            self.start(a, path, ctx);
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        if tag != "raid:form" {
            return;
        }
        let Some(p) = &mut self.pending else { return };
        let mut params = Vec::new();
        for prm in p.action.params {
            let mut val = fv(v, prm.key).to_string();
            if matches!(prm.kind, ParamKind::Drives) {
                val = val.split_whitespace().map(|t| t.replace('_', " ")).collect::<Vec<_>>().join(p.drive_sep);
            }
            params.push((prm.key.to_string(), val));
        }
        p.params = params;
        self.confirm(ctx);
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if tag == "raid:run" {
            self.run(ctx);
        }
    }

    fn reset(&mut self) {
        self.data.clear();
        self.pending = None;
    }
}
