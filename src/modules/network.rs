//! Network module: management network, interfaces (PVE network config), traffic,
//! tools (ping/traceroute/DNS/port), routes, firewall.

use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    Frame,
};
use serde_json::Value;

use super::{fv, scroll, View};
use crate::ctx::Ctx;
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::form::{self, Form};
use crate::ui::logview::Source;
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, sparkline, status_style, style_bad, style_dim, style_good, style_warn, Kv, ACCENT};
use crate::util::{fmt_bytes, fmt_rate, jarr, jb, js};

pub fn views() -> Vec<Box<dyn View>> {
    vec![
        Box::new(super::netmgmt::Mgmt::default()),
        Box::new(Interfaces::default()),
        Box::new(Traffic::default()),
        Box::new(Tools::default()),
        Box::new(Routes::default()),
        Box::new(Firewall::default()),
    ]
}

// ===========================================================================
// Interfaces
// ===========================================================================

#[derive(Default)]
struct IfData {
    ifaces: Vec<Value>,
    changes: String,
}

#[derive(Default)]
struct Interfaces {
    data: Loader<IfData>,
    table: Table,
    scroll: u16,
}

fn list_str(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::Array(a)) => a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join(" "),
        _ => js(v, key),
    }
}

impl Interfaces {
    fn sel(&self) -> Option<&Value> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.ifaces.iter().find(|i| js(i, "iface") == k)
    }

    /// Choices for port / slave / VLAN parent fields: (value, "name  type  [ip]").
    fn device_opts(ifaces: &[Value], types: &[&str], exclude: &str) -> Vec<(String, String)> {
        ifaces
            .iter()
            .filter(|i| types.contains(&js(i, "type").as_str()) && js(i, "iface") != exclude)
            .map(|i| {
                let name = js(i, "iface");
                let extra = [js(i, "type"), js(i, "cidr")].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("  ");
                (name.clone(), format!("{name:<14} {extra}"))
            })
            .collect()
    }

    fn edit_form(iface: &Value, ifaces: &[Value]) -> Form {
        let t = js(iface, "type");
        let name = js(iface, "iface");
        let mut fields = vec![
            form::info("Interface", &format!("{name} ({t})")),
            form::text("cidr", "IPv4/CIDR", &js(iface, "cidr")),
            form::text("gateway", "Gateway (IPv4)", &js(iface, "gateway")),
            form::text("cidr6", "IPv6/CIDR", &js(iface, "cidr6")),
            form::text("gateway6", "Gateway (IPv6)", &js(iface, "gateway6")),
            form::check("autostart", "Autostart", jb(iface, "autostart")),
            form::text("mtu", "MTU", &js(iface, "mtu")),
        ];
        match t.as_str() {
            "bridge" | "OVSBridge" => {
                fields.push(form::multi("bridge_ports", "Bridge ports", Self::device_opts(ifaces, &["eth", "bond", "vlan"], &name), &list_str(iface, "bridge_ports")).help("Space to select; none = internal bridge"));
                fields.push(form::check("bridge_vlan_aware", "VLAN aware", jb(iface, "bridge_vlan_aware")));
            }
            "bond" | "OVSBond" => {
                fields.push(form::multi("slaves", "Slaves", Self::device_opts(ifaces, &["eth"], &name), &list_str(iface, "slaves")).req(true));
                fields.push(form::select(
                    "bond_mode",
                    "Mode",
                    form::opts(&["balance-rr", "active-backup", "balance-xor", "broadcast", "802.3ad", "balance-tlb", "balance-alb"]),
                    &js(iface, "bond_mode"),
                ));
                fields.push(form::text("bond-primary", "Primary", &js(iface, "bond-primary")));
                fields.push(form::select("bond_xmit_hash_policy", "Hash policy", form::opts(&["", "layer2", "layer2+3", "layer3+4"]), &js(iface, "bond_xmit_hash_policy")));
            }
            "vlan" => {
                fields.push(form::select("vlan-raw-device", "VLAN raw device", Self::device_opts(ifaces, &["eth", "bond", "bridge"], &name), &js(iface, "vlan-raw-device")));
                fields.push(form::text("vlan-id", "VLAN tag", &js(iface, "vlan-id")));
            }
            _ => {}
        }
        fields.push(form::text("comments", "Comment", js(iface, "comments").trim()));
        Form::new(&format!("{} {name}", tr("Edit")), &format!("edit:{name}:{t}"), fields)
    }

    fn new_form(t: &str, ifaces: &[Value]) -> Form {
        let existing: Vec<String> = ifaces.iter().map(|i| js(i, "iface")).collect();
        let next = |prefix: &str| (0..1000).map(|i| format!("{prefix}{i}")).find(|n| !existing.contains(n)).unwrap_or_default();
        let mut fields = vec![];
        match t {
            "bridge" => {
                fields.push(form::text("iface", "Name", &next("vmbr")).req(true));
                fields.push(form::multi("bridge_ports", "Bridge ports", Self::device_opts(ifaces, &["eth", "bond", "vlan"], ""), "").help("Space to select; none = internal bridge"));
                fields.push(form::check("bridge_vlan_aware", "VLAN aware", false));
            }
            "bond" => {
                fields.push(form::text("iface", "Name", &next("bond")).req(true));
                fields.push(form::multi("slaves", "Slaves", Self::device_opts(ifaces, &["eth"], ""), "").req(true).help("Space to select the member NICs"));
                fields.push(form::select("bond_mode", "Mode", form::opts(&["active-backup", "802.3ad", "balance-rr", "balance-xor", "broadcast", "balance-tlb", "balance-alb"]), "active-backup"));
                fields.push(form::select("bond_xmit_hash_policy", "Hash policy", form::opts(&["", "layer2", "layer2+3", "layer3+4"]), ""));
            }
            _ => {
                let parents = Self::device_opts(ifaces, &["eth", "bond", "bridge"], "");
                let def = parents.iter().find(|(v, _)| v.starts_with("vmbr")).or(parents.first()).map(|(v, _)| v.clone()).unwrap_or_default();
                fields.push(form::select("vlan_parent", "VLAN raw device", parents, &def));
                fields.push(form::number("vlan_tag", "VLAN tag", "100").help("1-4094; the interface is named <device>.<tag>"));
            }
        }
        fields.push(form::text("cidr", "IPv4/CIDR", ""));
        fields.push(form::text("gateway", "Gateway (IPv4)", ""));
        fields.push(form::text("cidr6", "IPv6/CIDR", ""));
        fields.push(form::check("autostart", "Autostart", true));
        fields.push(form::text("comments", "Comment", ""));
        Form::new(&format!("{} {t}", tr("Create")), &format!("new:{t}"), fields)
    }
}

impl View for Interfaces {
    fn title(&self) -> &'static str {
        "Interfaces"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.data.load(move || {
            let full = api.get_full(&format!("/nodes/{node}/network"))?;
            let mut ifaces = full.get("data").map(jarr).unwrap_or_default();
            ifaces.sort_by_key(|i| {
                let t = js(i, "type");
                let order = match t.as_str() {
                    "bridge" | "OVSBridge" => 0,
                    "bond" | "OVSBond" => 1,
                    "eth" => 2,
                    "vlan" => 3,
                    _ => 4,
                };
                (order, js(i, "iface"))
            });
            Ok(IfData { ifaces, changes: js(&full, "changes") })
        });
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(15)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let live = ctx.metrics.lock().unwrap().ifaces.clone();
        let local = !live.is_empty();
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flat_map(|d| d.ifaces.iter())
            .map(|i| {
                let name = js(i, "iface");
                let active = jb(i, "active");
                let ports = [list_str(i, "bridge_ports"), list_str(i, "slaves"), js(i, "vlan-raw-device")].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" ");
                let rate = if local {
                    live.iter().find(|l| l.name == name).map(|l| format!("↓{} ↑{}", fmt_rate(l.rx_bps), fmt_rate(l.tx_bps))).unwrap_or_default()
                } else {
                    String::new()
                };
                let pending = i.get("pending").is_some();
                Row::new(name.clone())
                    .cs(if pending { format!("{name}*") } else { name }, if pending { style_warn() } else { Style::default() })
                    .c(js(i, "type"))
                    .cs(if active { "yes" } else { "no" }, if active { style_good() } else { style_dim() })
                    .c(js(i, "cidr"))
                    .c(js(i, "gateway"))
                    .c(ports)
                    .c(rate)
                    .c(js(i, "comments").trim().to_string())
                    .dim(!active)
            })
            .collect();
        let cols = [
            col("Name", Constraint::Length(14)),
            col("Type", Constraint::Length(8)),
            col("Active", Constraint::Length(6)),
            col("CIDR", Constraint::Length(19)),
            col("Gateway", Constraint::Length(15)),
            col("Ports/Slaves", Constraint::Length(14)),
            col("Rate", Constraint::Length(20)),
            col("Comment", Constraint::Min(6)),
        ];
        let mut title = tr("Interfaces").to_string();
        if self.data.data.as_ref().map(|d| !d.changes.is_empty()).unwrap_or(false) {
            title.push_str(" - PENDING CHANGES (A=apply V=revert)");
        }
        self.table.draw(f, area, &title, &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut lines: Vec<Line> = Vec::new();
        if let Some(i) = self.sel() {
            let mut kv = Kv::new();
            if let Value::Object(m) = i {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                for k in keys {
                    if k == "pending" {
                        continue;
                    }
                    kv.add(k, list_str(i, k));
                }
                if let Some(p) = m.get("pending") {
                    kv.section("Pending");
                    if let Value::Object(pm) = p {
                        for (k, _) in pm {
                            kv.styled(k, list_str(p, k), style_warn());
                        }
                    }
                }
            }
            lines = kv.lines();
        }
        if let Some(d) = &self.data.data {
            if !d.changes.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(tr("Pending changes (not applied yet)"), style_warn())));
                for l in d.changes.lines() {
                    let st = if l.starts_with('+') { style_good() } else if l.starts_with('-') { style_bad() } else { style_dim() };
                    lines.push(Line::from(Span::styled(l.to_string(), st)));
                }
            }
        }
        draw_lines(f, area, "Details", focused, lines, self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let m = ctx.need_node_priv("Sys.Modify");
        let pending = self.data.data.as_ref().map(|d| !d.changes.is_empty()).unwrap_or(false);
        let sel = self.sel();
        vec![
            item('e', "Edit").require(m.clone()).disabled_if(sel.is_none(), "nothing selected"),
            item('n', "Create bridge / bond / VLAN").require(m.clone()),
            item('x', "Delete").require(m.clone()).disabled_if(sel.is_none(), "nothing selected").disabled_if(sel.map(|s| js(s, "type") == "eth").unwrap_or(false), "physical NIC"),
            item('A', "Apply pending changes (ifreload)").require(m.clone()).disabled_if(!pending, "no pending changes"),
            item('V', "Revert pending changes").require(m).disabled_if(!pending, "no pending changes"),
        ]
    }

    fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        if tag == "newtype" {
            let t = match key {
                'b' => "bridge",
                'o' => "bond",
                _ => "vlan",
            };
            let ifaces = self.data.data.as_ref().map(|d| d.ifaces.clone()).unwrap_or_default();
            ctx.popup(Popup::Form(Self::new_form(t, &ifaces)));
            return;
        }
        match key {
            'e' => {
                if let Some(i) = self.sel() {
                    let all = self.data.data.as_ref().map(|d| d.ifaces.clone()).unwrap_or_default();
                    ctx.popup(Popup::Form(Self::edit_form(i, &all)));
                }
            }
            'n' => ctx.popup(Popup::menu("Create", "newtype", vec![item('b', "Linux Bridge"), item('o', "Linux Bond"), item('v', "Linux VLAN")])),
            'x' => {
                if let Some(i) = self.sel() {
                    let n = js(i, "iface");
                    ctx.popup(Popup::confirm("Delete interface", &format!("del:{n}"), vec![format!("Delete {n}? (takes effect on Apply)")], true));
                }
            }
            'A' => {
                let diff = self.data.data.as_ref().map(|d| d.changes.clone()).unwrap_or_default();
                let mut body: Vec<String> = diff.lines().take(30).map(|s| s.to_string()).collect();
                body.push(String::new());
                body.push("Apply now? A wrong management IP can disconnect remote sessions.".into());
                ctx.popup(Popup::confirm("Apply network configuration", "apply", body, true));
            }
            'V' => ctx.popup(Popup::confirm("Revert", "revert", vec!["Discard all pending network changes?".into()], false)),
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        let n = ctx.node.clone();
        match tag {
            "apply" => ctx.api_call("Apply network config", "PUT", format!("/nodes/{n}/network"), vec![]),
            "revert" => ctx.api_call("Revert network config", "DELETE", format!("/nodes/{n}/network"), vec![]),
            t if t.starts_with("del:") => {
                let i = &t[4..];
                ctx.api_call(format!("Delete {i}"), "DELETE", format!("/nodes/{n}/network/{i}"), vec![]);
            }
            _ => {}
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let n = ctx.node.clone();
        let parts: Vec<&str> = tag.splitn(3, ':').collect();
        let keys = [
            "cidr", "gateway", "cidr6", "gateway6", "mtu", "bridge_ports", "slaves", "bond_mode", "bond-primary",
            "bond_xmit_hash_policy", "vlan-raw-device", "vlan-id", "comments",
        ];
        let mut params: Vec<(&'static str, String)> = Vec::new();
        let mut delete: Vec<&str> = Vec::new();
        for k in keys {
            if v.iter().any(|(fk, _)| fk == k) {
                let val = fv(v, k);
                if val.is_empty() {
                    delete.push(k);
                } else {
                    params.push((k, val.to_string()));
                }
            }
        }
        for k in ["autostart", "bridge_vlan_aware"] {
            if v.iter().any(|(fk, _)| fk == k) {
                params.push((k, fv(v, k).to_string()));
            }
        }
        match parts.as_slice() {
            ["edit", iface, t] => {
                params.push(("type", t.to_string()));
                if !delete.is_empty() {
                    params.push(("delete", delete.join(",")));
                }
                ctx.api_call(format!("Update {iface}"), "PUT", format!("/nodes/{n}/network/{iface}"), params);
            }
            ["new", t] => {
                let name = if *t == "vlan" {
                    let tag: u32 = fv(v, "vlan_tag").parse().unwrap_or(0);
                    if !(1..=4094).contains(&tag) || fv(v, "vlan_parent").is_empty() {
                        ctx.popup(Popup::error("VLAN", "choose a device and a tag between 1 and 4094"));
                        return;
                    }
                    format!("{}.{tag}", fv(v, "vlan_parent"))
                } else {
                    fv(v, "iface").to_string()
                };
                params.push(("type", t.to_string()));
                params.push(("iface", name.clone()));
                ctx.api_call(format!("Create {name}"), "POST", format!("/nodes/{n}/network"), params);
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

// ===========================================================================
// Traffic
// ===========================================================================

#[derive(Default)]
struct Traffic {
    table: Table,
    all: bool,
}

impl View for Traffic {
    fn title(&self) -> &'static str {
        "Traffic"
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn refresh(&mut self, _ctx: &mut Ctx) {}

    fn refresh_secs(&self) -> Option<u64> {
        None
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("v", "virtual ifaces")]
    }

    fn on_key(&mut self, key: &crossterm::event::KeyEvent, _ctx: &mut Ctx) -> bool {
        if key.code == crossterm::event::KeyCode::Char('v') {
            self.all = !self.all;
            return true;
        }
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let m = ctx.metrics.lock().unwrap().clone();
        let rows: Vec<Row> = m
            .ifaces
            .iter()
            .filter(|i| self.all || i.physical || i.name.starts_with("vmbr") || i.name.starts_with("bond"))
            .map(|i| {
                let errs = i.rx_errs + i.tx_errs;
                let drops = i.rx_drop + i.tx_drop;
                Row::new(i.name.clone())
                    .cs(i.name.clone(), if i.physical { Style::default().fg(ACCENT) } else { Style::default() })
                    .c(fmt_rate(i.rx_bps))
                    .c(fmt_rate(i.tx_bps))
                    .c(fmt_bytes(i.rx_bytes as f64))
                    .c(fmt_bytes(i.tx_bytes as f64))
                    .cs(errs.to_string(), if errs > 0 { style_bad() } else { style_dim() })
                    .cs(drops.to_string(), if drops > 0 { style_warn() } else { style_dim() })
                    .c(sparkline(&i.rx_hist.iter().zip(&i.tx_hist).map(|(a, b)| a + b).collect::<Vec<_>>(), 20, None))
            })
            .collect();
        let status = if m.coarse || !m.ok { Some((format!("per-interface traffic: {}", tr("local only")), style_dim())) } else { None };
        let cols = [
            col("Interface", Constraint::Length(14)),
            col("RX/s", Constraint::Length(10)),
            col("TX/s", Constraint::Length(10)),
            col("RX total", Constraint::Length(9)),
            col("TX total", Constraint::Length(9)),
            col("Errors", Constraint::Length(7)),
            col("Drops", Constraint::Length(7)),
            col("History", Constraint::Min(10)),
        ];
        self.table.draw(f, area, if self.all { "Traffic (all)" } else { "Traffic" }, &cols, &rows, focused, status);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let m = ctx.metrics.lock().unwrap().clone();
        let w = area.width.saturating_sub(2) as usize;
        let mut lines = Vec::new();
        if let Some(i) = self.table.selected().and_then(|k| m.ifaces.iter().find(|i| i.name == k)) {
            lines.push(Line::from(Span::styled(format!("{}  RX {}", i.name, fmt_rate(i.rx_bps)), Style::default().fg(ACCENT))));
            lines.push(Line::from(Span::styled(sparkline(&i.rx_hist, w, None), style_good())));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(format!("TX {}", fmt_rate(i.tx_bps)), Style::default().fg(ACCENT))));
            lines.push(Line::from(Span::styled(sparkline(&i.tx_hist, w, None), style_warn())));
            lines.push(Line::from(""));
            let mut kv = Kv::new();
            kv.add("RX packets", i.rx_pkts.to_string());
            kv.add("TX packets", i.tx_pkts.to_string());
            kv.add("RX errors/drops", format!("{} / {}", i.rx_errs, i.rx_drop));
            kv.add("TX errors/drops", format!("{} / {}", i.tx_errs, i.tx_drop));
            let speed = crate::util::read_file(&format!("/sys/class/net/{}/speed", i.name)).trim().to_string();
            let oper = crate::util::read_file(&format!("/sys/class/net/{}/operstate", i.name)).trim().to_string();
            let mac = crate::util::read_file(&format!("/sys/class/net/{}/address", i.name)).trim().to_string();
            if ctx.this_machine() {
                kv.status("State", oper);
                kv.add("Speed", if speed.is_empty() || speed.starts_with('-') { "-".into() } else { format!("{speed} Mb/s") });
                kv.add("MAC", mac);
            }
            lines.extend(kv.lines());
        }
        draw_lines(f, area, "Details", focused, lines, 0);
    }
}

// ===========================================================================
// Tools
// ===========================================================================

#[derive(Default)]
struct Tools {
    history: Vec<String>,
}

const TOOLS: [(char, &str); 6] = [
    ('p', "Ping"),
    ('t', "Traceroute"),
    ('d', "DNS lookup"),
    ('c', "TCP port check"),
    ('a', "ARP table / neighbours"),
    ('s', "Listening sockets"),
];

impl View for Tools {
    fn title(&self) -> &'static str {
        "Tools"
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn refresh(&mut self, _ctx: &mut Ctx) {}

    fn refresh_secs(&self) -> Option<u64> {
        None
    }

    fn has_detail(&self) -> bool {
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut lines = vec![
            Line::from(Span::styled(format!("{} {} ({})", tr("Tools"), ctx.node, ctx.exec().map(|e| e.label()).unwrap_or_else(|| tr("local only").into())), Style::default().fg(ACCENT))),
            Line::from(""),
        ];
        for it in self.menu(ctx) {
            let st = if it.disabled.is_some() { style_dim() } else { Style::default() };
            lines.push(Line::from(vec![Span::styled(format!("  ({}) ", it.key), Style::default().fg(ACCENT)), Span::styled(tr(&it.label).to_string(), st)]));
        }
        if !self.history.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(tr("History"), Style::default().fg(ACCENT))));
            for h in self.history.iter().rev().take(10) {
                lines.push(Line::from(format!("  {h}")));
            }
        }
        draw_lines(f, area, "Tools", focused, lines, 0);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let e = ctx.need_exec("Sys.Audit").map(|_| ());
        TOOLS.iter().map(|(k, l)| item(*k, l).require(e.clone())).collect()
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let last = |p: &str| self.history.iter().rev().find_map(|h| h.strip_prefix(p).map(|s| s.split_whitespace().next().unwrap_or("").to_string())).unwrap_or_default();
        let target = |label: &str, v: String| form::text("target", label, &v).req(true);
        let f = match key {
            'p' => Some(Form::new("Ping", "ping", vec![target("Host", last("ping ")), form::number("count", "Count", "5")])),
            't' => Some(Form::new("Traceroute", "trace", vec![target("Host", last("traceroute "))])),
            'd' => Some(Form::new("DNS lookup", "dns", vec![target("Name", last("dns "))])),
            'c' => Some(Form::new("TCP port check", "port", vec![target("Host", last("port ")), form::number("port", "Port", "8006")])),
            'a' => {
                if let Some(e) = ctx.exec() {
                    self.history.push("ip neigh".into());
                    ctx.open_log("ip neigh", Source::Command { exec: e, prog: "ip".into(), args: vec!["neigh".into(), "show".into()] });
                }
                None
            }
            's' => {
                if let Some(e) = ctx.exec() {
                    self.history.push("ss -tulpn".into());
                    ctx.open_log("Listening sockets", Source::Command { exec: e, prog: "ss".into(), args: vec!["-tulpn".into()] });
                }
                None
            }
            _ => None,
        };
        if let Some(f) = f {
            ctx.popup(Popup::Form(f));
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else { return };
        let t = fv(v, "target").to_string();
        if t.starts_with('-') {
            ctx.toast_err("invalid target");
            return;
        }
        let (title, prog, args): (String, &str, Vec<String>) = match tag {
            "ping" => {
                let c = fv(v, "count").parse::<u32>().unwrap_or(5).clamp(1, 1000);
                (format!("ping {t}"), "ping", vec!["-c".into(), c.to_string(), "-W".into(), "2".into(), t.clone()])
            }
            "trace" => (
                format!("traceroute {t}"),
                "sh",
                vec!["-c".into(), "if command -v traceroute >/dev/null; then traceroute -n -w 2 \"$1\"; else tracepath -n \"$1\"; fi".into(), "sh".into(), t.clone()],
            ),
            "dns" => (
                format!("dns {t}"),
                "sh",
                vec![
                    "-c".into(),
                    "echo '# getent ahosts'; getent ahosts \"$1\"; if command -v dig >/dev/null; then echo; echo '# dig'; dig +noall +answer \"$1\" A \"$1\" AAAA; fi; echo; echo '# /etc/resolv.conf'; cat /etc/resolv.conf".into(),
                    "sh".into(),
                    t.clone(),
                ],
            ),
            "port" => {
                let port = fv(v, "port").parse::<u16>().unwrap_or(0);
                (
                    format!("port {t}:{port}"),
                    "bash",
                    vec!["-c".into(), "if timeout 5 bash -c \"</dev/tcp/$1/$2\" 2>/dev/null; then echo \"$1:$2 is OPEN\"; else echo \"$1:$2 is CLOSED or filtered\"; fi".into(), "bash".into(), t.clone(), port.to_string()],
                )
            }
            _ => return,
        };
        self.history.push(title.clone());
        ctx.open_log(&title, Source::Command { exec, prog: prog.into(), args });
    }
}

// ===========================================================================
// Routes
// ===========================================================================

#[derive(Default)]
struct Routes {
    data: Loader<Vec<Value>>,
    table: Table,
}

impl View for Routes {
    fn title(&self) -> &'static str {
        "Routes"
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
            let mut v = Vec::new();
            for fam in ["-4", "-6"] {
                let out = exec.output("ip", &["-j", fam, "route", "show", "table", "all"])?;
                let mut rows = jarr(&serde_json::from_str(&out).unwrap_or(Value::Null));
                for r in rows.iter_mut() {
                    if let Value::Object(m) = r {
                        m.insert("family".into(), Value::String(if fam == "-4" { "inet".into() } else { "inet6".into() }));
                    }
                }
                v.extend(rows.into_iter().filter(|r| js(r, "table") != "local"));
            }
            Ok(v)
        });
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(30)
    }

    fn has_detail(&self) -> bool {
        false
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
            .enumerate()
            .map(|(i, r)| {
                let dst = js(r, "dst");
                Row::new(i.to_string())
                    .cs(dst.clone(), if dst == "default" { Style::default().fg(ACCENT) } else { Style::default() })
                    .c(js(r, "gateway"))
                    .c(js(r, "dev"))
                    .c(js(r, "protocol"))
                    .c(js(r, "metric"))
                    .c(js(r, "prefsrc"))
                    .c(js(r, "table"))
                    .c(js(r, "family"))
            })
            .collect();
        let cols = [
            col("Destination", Constraint::Min(18)),
            col("Gateway", Constraint::Length(18)),
            col("Device", Constraint::Length(12)),
            col("Proto", Constraint::Length(8)),
            col("Metric", Constraint::Length(7)),
            col("Source", Constraint::Length(16)),
            col("Table", Constraint::Length(6)),
            col("Family", Constraint::Length(6)),
        ];
        self.table.draw(f, area, "Routes", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

// ===========================================================================
// Firewall
// ===========================================================================

#[derive(Default)]
struct FwData {
    cluster: Value,
    node: Value,
    rules: Vec<(String, Value)>,
    status: String,
}

#[derive(Default)]
struct Firewall {
    data: Loader<FwData>,
    table: Table,
    scroll: u16,
}

impl View for Firewall {
    fn title(&self) -> &'static str {
        "Firewall"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let exec = ctx.exec();
        self.data.load(move || {
            let cluster = api.get("/cluster/firewall/options").unwrap_or(Value::Null);
            let nodeo = api.get(&format!("/nodes/{node}/firewall/options")).unwrap_or(Value::Null);
            let mut rules = Vec::new();
            for r in jarr(&api.get("/cluster/firewall/rules").unwrap_or(Value::Null)) {
                rules.push(("cluster".to_string(), r));
            }
            for r in jarr(&api.get(&format!("/nodes/{node}/firewall/rules")).unwrap_or(Value::Null)) {
                rules.push((format!("node {node}"), r));
            }
            let status = exec.and_then(|e| e.output_any("pve-firewall", &["status"]).ok()).map(|o| o.1.trim().to_string()).unwrap_or_default();
            Ok(FwData { cluster, node: nodeo, rules, status })
        });
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

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flat_map(|d| d.rules.iter())
            .enumerate()
            .map(|(i, (scope, r))| {
                let en = jb(r, "enable");
                let action = js(r, "action");
                Row::new(i.to_string())
                    .c(scope.clone())
                    .c(js(r, "pos"))
                    .c(js(r, "type"))
                    .cs(action.clone(), status_style(if action == "ACCEPT" { "ok" } else if action == "DROP" || action == "REJECT" { "error" } else { "x" }))
                    .c(js(r, "macro"))
                    .c(js(r, "source"))
                    .c(js(r, "dest"))
                    .c(format!("{} {}", js(r, "proto"), js(r, "dport")).trim().to_string())
                    .c(js(r, "comment"))
                    .dim(!en)
            })
            .collect();
        let cols = [
            col("Scope", Constraint::Length(14)),
            col("#", Constraint::Length(3)),
            col("Dir", Constraint::Length(4)),
            col("Action", Constraint::Length(7)),
            col("Macro", Constraint::Length(10)),
            col("Source", Constraint::Length(16)),
            col("Dest", Constraint::Length(16)),
            col("Port", Constraint::Length(12)),
            col("Comment", Constraint::Min(6)),
        ];
        self.table.draw(f, area, "Firewall rules", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(d) = &self.data.data {
            kv.section("Cluster firewall");
            let en = jb(&d.cluster, "enable");
            kv.styled("Enabled", if en { "yes" } else { "no" }, if en { style_good() } else { style_dim() });
            kv.add("Input policy", if js(&d.cluster, "policy_in").is_empty() { "DROP (default)".into() } else { js(&d.cluster, "policy_in") });
            kv.add("Output policy", if js(&d.cluster, "policy_out").is_empty() { "ACCEPT (default)".into() } else { js(&d.cluster, "policy_out") });
            kv.section("Node firewall");
            let nen = d.node.get("enable").map(|_| jb(&d.node, "enable")).unwrap_or(true);
            kv.styled("Enabled", if nen { "yes" } else { "no" }, if nen { style_good() } else { style_dim() });
            if !d.status.is_empty() {
                kv.add("pve-firewall", d.status.clone());
            }
        }
        draw_lines(f, area, "Options", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let c_en = self.data.data.as_ref().map(|d| jb(&d.cluster, "enable")).unwrap_or(false);
        let n_en = self.data.data.as_ref().map(|d| d.node.get("enable").map(|_| jb(&d.node, "enable")).unwrap_or(true)).unwrap_or(true);
        vec![
            item('c', if c_en { "Disable cluster firewall" } else { "Enable cluster firewall" }).require(ctx.need_priv("/", "Sys.Modify")),
            item('n', if n_en { "Disable node firewall" } else { "Enable node firewall" }).require(ctx.need_node_priv("Sys.Modify")),
            item('S', "Emergency: stop pve-firewall service").require(ctx.need_root()).require(ctx.need_exec("Sys.Modify").map(|_| ())),
            item('s', "Start pve-firewall service").require(ctx.need_root()).require(ctx.need_exec("Sys.Modify").map(|_| ())),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let c_en = self.data.data.as_ref().map(|d| jb(&d.cluster, "enable")).unwrap_or(false);
        let n_en = self.data.data.as_ref().map(|d| d.node.get("enable").map(|_| jb(&d.node, "enable")).unwrap_or(true)).unwrap_or(true);
        match key {
            'c' => {
                let body = if c_en {
                    vec!["Disable the firewall for the whole cluster?".into()]
                } else {
                    vec![
                        "Enable the cluster firewall?".into(),
                        "Default input policy is DROP: make sure rules allow 8006/22 (the management IPset is allowed by default).".into(),
                    ]
                };
                ctx.popup(Popup::confirm("Cluster firewall", if c_en { "c0" } else { "c1" }, body, !c_en));
            }
            'n' => ctx.popup(Popup::confirm("Node firewall", if n_en { "n0" } else { "n1" }, vec![format!("{} firewall on node {}?", if n_en { "Disable" } else { "Enable" }, ctx.node)], false)),
            'S' => ctx.popup(Popup::confirm("Stop firewall", "stop", vec!["Stop pve-firewall on this node? Rules stop being enforced.".into()], true)),
            's' => {
                if let Some(e) = ctx.exec() {
                    ctx.exec_call("pve-firewall start", e, "pve-firewall", vec!["start".into()]);
                }
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        let n = ctx.node.clone();
        match tag {
            "c0" | "c1" => ctx.api_call("Cluster firewall", "PUT", "/cluster/firewall/options".into(), vec![("enable", tag[1..].into())]),
            "n0" | "n1" => ctx.api_call("Node firewall", "PUT", format!("/nodes/{n}/firewall/options"), vec![("enable", tag[1..].into())]),
            "stop" => {
                if let Some(e) = ctx.exec() {
                    ctx.exec_call("pve-firewall stop", e, "pve-firewall", vec!["stop".into()]);
                }
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

