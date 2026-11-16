//! System > BMC: the IPMI event log (complete, newest first), BMC sensors, controller / LAN /
//! chassis information, and BMC actions (export / clear the log, identify LED, power restore
//! policy, BMC network, BMC reset).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    Frame,
};

use super::health::{sev_label, sev_style};
use super::{fv, scroll, View};
use crate::bmc::{get, Bmc};
use crate::ctx::{ActionOutcome, Ctx};
use crate::health::Sev;
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::form::{self, Form};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, style_bad, style_dim, style_label, style_warn};

const MODES: [&str; 3] = ["Event log", "Sensors", "BMC info"];

/// Where exported event logs go on the node.
const EXPORT_DIR: &str = "/var/log/pxvirt-console";

#[derive(Default)]
pub struct BmcView {
    data: Loader<Bmc>,
    mode: usize,
    tables: [Table; 3],
    scroll: u16,
    /// BMC network settings waiting for confirmation: (channel, values)
    pending_net: Option<(u8, Vec<(String, String)>)>,
}

impl BmcView {
    fn bmc(&self) -> Option<&Bmc> {
        self.data.data.as_ref()
    }

    fn ready(&self, ctx: &Ctx) -> Result<(), String> {
        ctx.need_root()?;
        ctx.need_exec("Sys.Modify").map(|_| ())?;
        match self.bmc() {
            Some(b) if b.no_tool => Err(tr("ipmitool is not installed").into()),
            Some(b) if b.no_device => Err(tr("no IPMI device").into()),
            Some(_) => Ok(()),
            None => Err(tr("loading").into()),
        }
    }

    fn channel(&self) -> u8 {
        self.bmc().and_then(|b| b.lan.first().map(|l| l.0)).unwrap_or(1)
    }

    /// Run a shell script on the node as a logged action.
    fn run(ctx: &mut Ctx, title: &str, script: String, done: &'static str) {
        let Some(e) = ctx.exec() else { return };
        ctx.act(title, move || {
            let out = e.output("sh", &["-c", &script])?;
            let last = out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
            Ok(ActionOutcome::Msg(if last.is_empty() { done.to_string() } else { format!("{done}: {last}") }))
        });
    }

    pub(crate) fn export_script() -> String {
        format!("set -e; mkdir -p {EXPORT_DIR}; f={EXPORT_DIR}/bmc-sel-$(date +%Y%m%d-%H%M%S).txt; {{ ipmitool sel info; echo; ipmitool sel elist; }} > $f; echo $f")
    }
}

fn valid_ip(s: &str) -> bool {
    s.parse::<std::net::Ipv4Addr>().is_ok()
}

impl View for BmcView {
    fn title(&self) -> &'static str {
        "BMC"
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else {
            self.data.error = Some(tr("local only").into());
            return;
        };
        self.data.load(move || Ok(Bmc::parse(&exec.output("sh", &["-c", crate::bmc::SCRIPT])?)));
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    /// ipmitool is slow (the event log alone can take half a minute)
    fn refresh_secs(&self) -> Option<u64> {
        Some(120)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.tables[self.mode])
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("v", MODES[(self.mode + 1) % MODES.len()])]
    }

    fn on_key(&mut self, key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        if key.code == KeyCode::Char('v') {
            self.mode = (self.mode + 1) % MODES.len();
            self.scroll = 0;
            return true;
        }
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut status = loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none());
        let mut rows: Vec<Row> = Vec::new();
        if let Some(b) = self.bmc() {
            if b.no_tool {
                status = Some((tr("ipmitool is not installed: press I to install it").to_string(), style_warn()));
            } else if b.no_device {
                status = Some((tr("No IPMI device (/dev/ipmi0). No BMC, or the kernel modules are not loaded: press M").to_string(), style_dim()));
            } else {
                match self.mode {
                    0 => {
                        for e in &b.sel {
                            let s = e.sev();
                            rows.push(Row::new(format!("{:x}", e.id)).c(format!("{:x}", e.id)).c(e.when()).cs(tr(sev_label(s)), sev_style(s)).c(e.event.clone()).c(e.sensor.clone()).c(e.state.clone()));
                        }
                        if rows.is_empty() {
                            status = Some((tr("The event log is empty").to_string(), style_dim()));
                        }
                    }
                    1 => {
                        for x in &b.sensors {
                            let s = x.sev();
                            let st = if x.status == "ns" { style_dim() } else { sev_style(s) };
                            rows.push(Row::new(x.name.clone()).c(x.name.clone()).cs(x.reading.clone(), st).cs(x.status.clone(), st).c(x.entity.clone()));
                        }
                    }
                    _ => {
                        let mut add = |sec: &str, kv: &[(String, String)]| {
                            if kv.is_empty() {
                                return;
                            }
                            rows.push(Row::new(format!("#{sec}")).cs(tr(sec), style_label()).c(""));
                            for (k, v) in kv {
                                rows.push(Row::new(format!("{sec}:{k}")).c(format!("  {k}")).c(v.clone()));
                            }
                        };
                        add("Controller", &b.mc);
                        for (ch, l) in &b.lan {
                            add(&format!("LAN channel {ch}"), l);
                        }
                        add("Chassis", &b.chassis);
                        add("Event log", &b.sel_info);
                        add("FRU", &b.fru);
                    }
                }
            }
        }
        let title = format!("BMC · {}", tr(MODES[self.mode]));
        let t = &mut self.tables[self.mode];
        match self.mode {
            0 => {
                let cols = [
                    col("ID", Constraint::Length(5)),
                    col("Time", Constraint::Length(19)),
                    col("Severity", Constraint::Length(9)),
                    col("Event", Constraint::Min(22)),
                    col("Sensor", Constraint::Length(20)),
                    col("State", Constraint::Length(11)),
                ];
                t.draw(f, area, &title, &cols, &rows, focused, status);
            }
            1 => {
                let cols = [col("Sensor", Constraint::Length(24)), col("Reading", Constraint::Min(16)), col("Status", Constraint::Length(7)), col("Entity", Constraint::Length(7))];
                t.draw(f, area, &title, &cols, &rows, focused, status);
            }
            _ => {
                let cols = [col("Item", Constraint::Length(30)), col("Value", Constraint::Min(20))];
                t.draw(f, area, &title, &cols, &rows, focused, status);
            }
        }
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let lab = style_label();
        let kvl = |k: &str, v: String| Line::from(vec![Span::styled(format!("{:<18}", tr(k)), lab), Span::raw(v)]);
        if let Some(b) = self.bmc() {
            let sel = self.tables[self.mode].selected().map(|s| s.to_string());
            match self.mode {
                0 => {
                    if let Some(e) = sel.and_then(|k| b.sel.iter().find(|e| format!("{:x}", e.id) == k)) {
                        let s = e.sev();
                        lines.push(Line::from(Span::styled(tr(sev_label(s)).to_string(), sev_style(s))));
                        lines.push(kvl("Record", format!("{:x} ({})", e.id, e.id)));
                        lines.push(kvl("Time", e.when()));
                        lines.push(kvl("Sensor", e.sensor.clone()));
                        lines.push(kvl("Event", e.event.clone()));
                        lines.push(kvl("State", e.state.clone()));
                        if e.at.is_none() {
                            lines.push(Line::from(Span::styled(tr("Logged before the BMC clock was set (pre-init)."), style_dim())));
                        }
                        lines.push(Line::from(""));
                    }
                }
                1 => {
                    if let Some(x) = sel.and_then(|k| b.sensors.iter().find(|x| x.name == k)) {
                        lines.push(kvl("Sensor", x.name.clone()));
                        lines.push(kvl("Reading", x.reading.clone()));
                        lines.push(Line::from(vec![Span::styled(format!("{:<18}", tr("Status")), lab), Span::styled(format!("{} ({})", x.status, tr(sev_label(x.sev()))), sev_style(x.sev()))]));
                        lines.push(kvl("Entity", x.entity.clone()));
                        lines.push(Line::from(""));
                        lines.push(Line::from(Span::styled(tr("ok = normal, ns = no reading, nc = non-critical, cr = critical, nr = non-recoverable"), style_dim())));
                        lines.push(Line::from(""));
                    }
                }
                _ => {}
            }
            if b.no_tool || b.no_device {
                for l in [
                    "This page reads the BMC (iBMC, iDRAC, iLO, XClarity, IPMI) of the node through ipmitool:",
                    "  - the complete event log (SEL), newest first, with severity",
                    "  - fans, power supplies, voltages and temperatures",
                    "  - BMC firmware, network address, power state and restore policy",
                    "",
                    "Actions: export / clear the event log, identify LED, power restore policy, BMC network, BMC reset.",
                ] {
                    lines.push(Line::from(tr(l).to_string()));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(if b.no_tool { tr("Press I to install ipmitool.") } else { tr("Press M to load the IPMI kernel modules.") }, style_warn())));
            }
            // summary
            if !b.no_tool && !b.no_device {
                lines.push(Line::from(Span::styled(tr("BMC"), lab)));
                let fw = get(&b.mc, "Firmware Revision");
                lines.push(kvl("Vendor", get(&b.mc, "Manufacturer Name").to_string()));
                lines.push(kvl("Firmware", format!("{fw}  IPMI {}", get(&b.mc, "IPMI Version"))));
                for (ch, l) in &b.lan {
                    lines.push(kvl(&format!("LAN {ch}"), format!("{} ({})", get(l, "IP Address"), get(l, "IP Address Source"))));
                    lines.push(kvl("MAC", get(l, "MAC Address").to_string()));
                }
                lines.push(kvl("Power", get(&b.chassis, "System Power").to_string()));
                lines.push(kvl("After power loss", get(&b.chassis, "Power Restore Policy").to_string()));
                let intr = get(&b.chassis, "Chassis Intrusion");
                if !intr.is_empty() {
                    lines.push(Line::from(vec![Span::styled(format!("{:<18}", tr("Intrusion")), lab), Span::styled(intr.to_string(), if intr == "active" { style_warn() } else { Style::default() })]));
                }
                let used = b.sel_used();
                let ev = format!("{} {}, {}% {}", b.sel.len(), tr("entries"), used.map(|u| format!("{u:.0}")).unwrap_or("?".into()), tr("used"));
                lines.push(Line::from(vec![Span::styled(format!("{:<18}", tr("Event log")), lab), Span::styled(ev, if used.unwrap_or(0.0) >= 90.0 { style_warn() } else { Style::default() })]));
                if get(&b.sel_info, "Overflow") == "true" {
                    lines.push(Line::from(Span::styled(tr("The event log overflowed: newer events were lost. Export and clear it (C)."), style_bad())));
                }
                let bad: Vec<_> = b.sensors.iter().filter(|x| x.sev() > Sev::Ok).collect();
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<18}", tr("Sensors")), lab),
                    if bad.is_empty() { Span::raw(format!("{} {}", b.sensors.len(), tr("all normal"))) } else { Span::styled(format!("{} / {} {}", bad.len(), b.sensors.len(), tr("not normal")), style_bad()) },
                ]));
            }
        }
        draw_lines(f, area, "Details", focused, lines, self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let root = || ctx.need_root().and_then(|_| ctx.need_exec("Sys.Modify").map(|_| ()));
        let r = || self.ready(ctx);
        let mut v = Vec::new();
        match self.bmc() {
            Some(b) if b.no_tool => return vec![item('I', "Install ipmitool (apt-get install ipmitool)").require(root())],
            Some(b) if b.no_device => return vec![item('M', "Load the IPMI kernel modules (ipmi_si, ipmi_devintf)").require(root())],
            _ => {}
        }
        v.extend([
            item('w', "Export the event log to a file on the node").require(r()),
            item('C', "Clear the event log (exported first)").require(r()),
            item('i', "Identify: blink the chassis LED for 4 minutes").require(r()),
            item('O', "Identify LED off").require(r()),
            item('p', "Power restore policy (after power loss)").require(r()),
            item('n', "BMC network settings").require(r()),
            item('R', "Reset the BMC (cold reset)").require(r()),
        ]);
        v
    }

    fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        if tag == "policy" {
            let p = match key {
                'o' => "always-on",
                'p' => "previous",
                'f' => "always-off",
                _ => return,
            };
            Self::run(ctx, &format!("BMC power restore policy {p}"), format!("ipmitool chassis policy {p}"), "power restore policy set");
            self.data.at = None;
            return;
        }
        match key {
            'I' => crate::modules::updates::Updates::suspend_apt(ctx, vec!["install".into(), "ipmitool".into()], format!("apt-get install ipmitool on {}\n", ctx.node)),
            'M' => Self::run(ctx, "Load IPMI modules", "modprobe ipmi_devintf; modprobe ipmi_si; sleep 1; ls /dev/ipmi* 2>/dev/null || echo 'no IPMI device found'".into(), "IPMI modules"),
            'w' => Self::run(ctx, "Export BMC event log", Self::export_script(), "saved"),
            'C' => ctx.popup(Popup::confirm(
                "Clear BMC event log",
                "clear",
                vec![
                    format!("{} {} {}", tr("The event log is saved to"), EXPORT_DIR, tr("first, then cleared on the BMC.")),
                    tr("The BMC keeps no other copy.").to_string(),
                ],
                true,
            )),
            'i' => Self::run(ctx, "BMC identify LED on", "ipmitool chassis identify 240".into(), "identify LED blinking"),
            'O' => Self::run(ctx, "BMC identify LED off", "ipmitool chassis identify 0".into(), "identify LED off"),
            'p' => {
                let cur = self.bmc().map(|b| get(&b.chassis, "Power Restore Policy").to_string()).unwrap_or_default();
                ctx.popup(Popup::menu(
                    &format!("{} ({}: {cur})", tr("After power loss"), tr("now")),
                    "policy",
                    vec![item('o', "Always power on"), item('p', "Restore the previous state"), item('f', "Stay off")],
                ));
            }
            'n' => {
                let ch = self.channel();
                let l = self.bmc().and_then(|b| b.lan.iter().find(|x| x.0 == ch).map(|x| x.1.clone())).unwrap_or_default();
                let src = if get(&l, "IP Address Source").to_lowercase().contains("dhcp") { "dhcp" } else { "static" };
                let vlan = get(&l, "802.1q VLAN ID");
                ctx.popup(Popup::Form(Form::new(
                    &format!("{} {ch}", tr("BMC network, LAN channel")),
                    "net",
                    vec![
                        form::info("MAC", get(&l, "MAC Address")),
                        form::select("src", "Address", form::opts(&["static", "dhcp"]), src),
                        form::text("ip", "IP address", get(&l, "IP Address")).req(false),
                        form::text("mask", "Netmask", get(&l, "Subnet Mask")).req(false),
                        form::text("gw", "Gateway", get(&l, "Default Gateway IP")).req(false),
                        form::text("vlan", "VLAN ID", if vlan.parse::<u16>().is_ok() { vlan } else { "" }).req(false).help("empty = no VLAN"),
                    ],
                )));
            }
            'R' => ctx.popup(Popup::confirm(
                "Reset BMC",
                "reset",
                vec![tr("Cold reset of the BMC: its web interface, remote console and IPMI are gone for 1–3 minutes.").into(), tr("The node and its guests keep running.").into()],
                true,
            )),
            _ => {}
        }
    }

    fn on_form(&mut self, tag: &str, values: &[(String, String)], ctx: &mut Ctx) {
        if tag != "net" {
            return;
        }
        let ch = self.channel();
        if fv(values, "src") == "static" {
            for (k, name) in [("ip", "IP address"), ("mask", "Netmask")] {
                if !valid_ip(fv(values, k)) {
                    return ctx.toast_err(format!("{}: {}", tr(name), tr("not a valid IPv4 address")));
                }
            }
            if !fv(values, "gw").is_empty() && !valid_ip(fv(values, "gw")) {
                return ctx.toast_err(format!("{}: {}", tr("Gateway"), tr("not a valid IPv4 address")));
            }
        }
        let vlan = fv(values, "vlan");
        if !vlan.is_empty() && !vlan.parse::<u16>().map(|v| (1..=4094).contains(&v)).unwrap_or(false) {
            return ctx.toast_err(tr("VLAN ID must be 1-4094"));
        }
        let summary = if fv(values, "src") == "dhcp" {
            "DHCP".to_string()
        } else {
            format!("{} / {}  gw {}", fv(values, "ip"), fv(values, "mask"), fv(values, "gw"))
        };
        self.pending_net = Some((ch, values.to_vec()));
        ctx.popup(Popup::confirm(
            "BMC network",
            "netapply",
            vec![
                format!("LAN {ch}: {summary}{}", if vlan.is_empty() { String::new() } else { format!("  VLAN {vlan}") }),
                tr("Sessions to the old BMC address (web interface, remote console) will drop.").into(),
            ],
            true,
        ));
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        match tag {
            "clear" => {
                Self::run(ctx, "Clear BMC event log", format!("f=$({}) && ipmitool sel clear >/dev/null && echo $f", Self::export_script()), "cleared, saved to");
                self.data.at = None;
            }
            "reset" => Self::run(ctx, "BMC cold reset", "ipmitool mc reset cold".into(), "BMC is resetting"),
            "netapply" => {
                let Some((ch, v)) = self.pending_net.take() else { return };
                let mut cmds = vec![];
                if fv(&v, "src") == "dhcp" {
                    cmds.push(format!("ipmitool lan set {ch} ipsrc dhcp"));
                } else {
                    cmds.push(format!("ipmitool lan set {ch} ipsrc static"));
                    cmds.push(format!("ipmitool lan set {ch} ipaddr {}", fv(&v, "ip")));
                    cmds.push(format!("ipmitool lan set {ch} netmask {}", fv(&v, "mask")));
                    if !fv(&v, "gw").is_empty() {
                        cmds.push(format!("ipmitool lan set {ch} defgw ipaddr {}", fv(&v, "gw")));
                    }
                }
                cmds.push(match fv(&v, "vlan") {
                    "" => format!("ipmitool lan set {ch} vlan id off"),
                    id => format!("ipmitool lan set {ch} vlan id {id}"),
                });
                // values were validated (IPv4 addresses, numeric VLAN), safe to put into the script
                Self::run(ctx, &format!("BMC network LAN {ch}"), format!("set -e; {}", cmds.join("; ")), "BMC network changed");
                self.data.at = None;
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(v: &mut BmcView, ctx: &Ctx) -> String {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(150, 22)).unwrap();
        t.draw(|f| {
            let a = f.area();
            let (l, r) = (Rect::new(0, 0, 95, a.height), Rect::new(95, 0, 55, a.height));
            v.draw_list(f, l, ctx, true);
            v.draw_detail(f, r, ctx, false);
        })
        .unwrap();
        let b = t.backend().buffer().clone();
        (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect()
    }

    #[test]
    fn renders_all_modes() {
        let ctx = Ctx::test();
        let mut v = BmcView::default();
        v.data.data = Some(Bmc::parse(crate::bmc::tests::OUT));
        let mut out = String::new();
        for m in 0..3 {
            v.mode = m;
            let s = render(&mut v, &ctx);
            out.push_str(&s);
        }
        println!("{out}");
        for want in ["Power Supply AC lost", "2026-11-14 21:00:00", "FAN1 Speed", "192.168.18.111", "always-off", "93% used", "LAN channel 1"] {
            assert!(out.contains(want), "missing {want}");
        }
        v.data.data = Some(Bmc::parse("@@noipmitool\n"));
        let menu = v.menu(&ctx);
        assert_eq!(menu.iter().map(|i| i.key).collect::<String>(), "I");
        assert!(render(&mut v, &ctx).contains("ipmitool is not installed"));
    }

    #[test]
    fn ip_validation() {
        assert!(valid_ip("10.0.0.1"));
        assert!(!valid_ip("10.0.0.1; reboot"));
        assert!(!valid_ip(""));
    }
}
