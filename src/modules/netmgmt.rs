//! Network > Management: the DCUI-style "configure management network" page.
//!
//! Edits IP/gateway, DNS and the bridged NIC of the management bridge through the PVE API.
//! Applying is protected: the current /etc/network/interfaces is backed up and a systemd timer
//! restores it after 75 s unless the change is confirmed in the console (60 s countdown), so a
//! wrong IP cannot lock you out. The local rescue reset (no API needed) lives here too.

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
use crate::ctx::Ctx;
use crate::exec::Exec;
use crate::i18n::tr;
use crate::loader::{Job, Loader};
use crate::rescue::{self, NetIface, NetworkInfo};
use crate::ui::form::{self, Form};
use crate::ui::logview::Source;
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, style_bad, style_dim, style_good, style_warn, Kv, ACCENT};
use crate::util::{jarr, js, urlenc};

const BACKUP: &str = "/etc/network/interfaces.pxvirt-console.bak";
const REVERT_UNIT: &str = "pxvirt-net-revert";
const CONFIRM_SECS: u64 = 60;

#[derive(Clone)]
struct Nic {
    name: String,
    link: bool,
    mac: String,
    speed: String,
}

#[derive(Default)]
struct Data {
    ifaces: Vec<Value>,
    changes: String,
    dns: Value,
    hosts: Value,
    nics: Vec<Nic>,
}

#[derive(Default)]
pub struct Mgmt {
    data: Loader<Data>,
    table: Table,
    scroll: u16,
    status: Option<(String, bool)>,
    /// staging a change (PUT iface) -> then ask to apply
    stage: Option<Job<String>>,
    /// apply + wait for the task
    apply: Option<Job<bool>>,
    revert: Option<Job<String>>,
    /// /etc/hosts update to do once the new IP is confirmed: (old ip, new ip)
    hosts_update: Option<(String, String)>,
    // local rescue reset
    rescue_nics: Option<Job<Vec<NetIface>>>,
    found: Vec<NetIface>,
    chosen: Option<String>,
    draft: NetworkInfo,
    rescue_apply: Option<Job<String>>,
}

fn nic_list(exec: &Exec) -> Vec<Nic> {
    let script = "for d in /sys/class/net/*; do n=${d##*/}; [ -e $d/device ] || continue; echo \"$n $(cat $d/carrier 2>/dev/null || echo 0) $(cat $d/address) $(cat $d/speed 2>/dev/null || echo -)\"; done";
    exec.output("sh", &["-c", script])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            (p.len() >= 3).then(|| Nic { name: p[0].into(), link: p[1] == "1", mac: p[2].into(), speed: p.get(3).unwrap_or(&"-").to_string() })
        })
        .collect()
}

impl Mgmt {
    /// The bridge/interface carrying the node's management IP.
    fn mgmt_iface<'a>(&'a self, ctx: &Ctx) -> Option<&'a Value> {
        let d = self.data.data.as_ref()?;
        let node_ip = ctx.node_info(&ctx.node).map(|n| n.ip.clone()).unwrap_or_default();
        let ip_of = |i: &Value| js(i, "cidr").split('/').next().unwrap_or("").to_string();
        d.ifaces
            .iter()
            .find(|i| !node_ip.is_empty() && ip_of(i) == node_ip)
            .or_else(|| d.ifaces.iter().find(|i| !js(i, "gateway").is_empty()))
            .or_else(|| d.ifaces.iter().find(|i| js(i, "iface") == "vmbr0"))
    }

    fn rescue_allowed(ctx: &Ctx) -> Result<(), String> {
        ctx.need_root()?;
        if !ctx.this_machine() || !crate::util::is_root_process() {
            return Err(tr("local only").into());
        }
        Ok(())
    }

    /// PUT the interface change, then return the pending diff.
    fn stage_change(&mut self, ctx: &mut Ctx, iface: &str, params: Vec<(&'static str, String)>) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let iface = iface.to_string();
        ctx.audit(&format!("stage network change {iface}"));
        self.status = Some((tr("Saving...").into(), false));
        self.stage = Some(Job::spawn(move || {
            api.put(&format!("/nodes/{node}/network/{iface}"), &params)?;
            let full = api.get_full(&format!("/nodes/{node}/network"))?;
            Ok(js(&full, "changes"))
        }));
    }

    fn ask_apply(&self, ctx: &mut Ctx, diff: &str) {
        let mut body: Vec<String> = diff.lines().take(40).map(|s| s.to_string()).collect();
        if body.is_empty() {
            body.push(tr("No pending changes").into());
        }
        body.push(String::new());
        if ctx.exec().is_some() {
            body.push(format!("{} {CONFIRM_SECS}s", tr("After applying you must confirm the new settings within")));
            body.push(tr("otherwise the old configuration is restored automatically.").into());
        } else {
            body.push(tr("WARNING: no local/ssh access, the change cannot be rolled back automatically.").into());
        }
        if let Some((o, n)) = &self.hosts_update {
            body.push(format!("/etc/hosts: {o} -> {n} ({})", tr("after confirming")));
        }
        if ctx.nodes.len() > 1 {
            body.push(tr("Cluster node: changing the IP also needs corosync.conf to be updated.").into());
        }
        ctx.popup(Popup::confirm("Apply network configuration", "apply", body, true));
    }

    fn start_apply(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let exec = ctx.exec();
        ctx.audit("apply network configuration");
        self.status = Some((tr("Applying network configuration...").into(), false));
        self.apply = Some(Job::spawn(move || {
            let protected = if let Some(e) = &exec {
                // server-side safety net: restores the backup even if this console dies (e.g. ssh drops)
                let script = format!(
                    "cp /etc/network/interfaces {BACKUP} && systemctl stop {REVERT_UNIT}.timer {REVERT_UNIT}.service >/dev/null 2>&1; \
                     systemd-run --quiet --unit={REVERT_UNIT} --on-active=75 /bin/sh -c 'cp {BACKUP} /etc/network/interfaces && ifreload -a'"
                );
                e.output("sh", &["-c", &script])?;
                true
            } else {
                false
            };
            wait_task(&api, &node, api.put(&format!("/nodes/{node}/network"), &[])?)?;
            Ok(protected)
        }));
    }

    fn start_revert(&mut self, ctx: &mut Ctx, reason: &str) {
        let Some(exec) = ctx.exec() else { return };
        ctx.audit(&format!("revert network configuration ({reason})"));
        self.status = Some((tr("Restoring previous network configuration...").into(), true));
        self.hosts_update = None;
        self.revert = Some(Job::spawn(move || {
            let script = format!("systemctl stop {REVERT_UNIT}.timer >/dev/null 2>&1; cp {BACKUP} /etc/network/interfaces && ifreload -a");
            exec.output("sh", &["-c", &script])?;
            Ok("OK".into())
        }));
    }

    fn keep(&mut self, ctx: &mut Ctx) {
        if let Some(exec) = ctx.exec() {
            let _ = exec.output("sh", &["-c", &format!("systemctl stop {REVERT_UNIT}.timer {REVERT_UNIT}.service >/dev/null 2>&1; true")]);
        }
        ctx.audit("network configuration confirmed");
        self.status = Some((tr("New network configuration kept").into(), false));
        if let Some((old, new)) = self.hosts_update.take() {
            let api = ctx.api.clone();
            let node = ctx.node.clone();
            ctx.act("Update /etc/hosts", move || {
                let h = api.get(&format!("/nodes/{node}/hosts"))?;
                let data = js(&h, "data");
                let updated: String = data
                    .lines()
                    .map(|l| {
                        let mut parts = l.split_whitespace();
                        if parts.next() == Some(old.as_str()) {
                            l.replacen(&old, &new, 1)
                        } else {
                            l.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n";
                api.post(&format!("/nodes/{node}/hosts"), &[("data", updated), ("digest", js(&h, "digest"))])?;
                Ok(crate::ctx::ActionOutcome::Msg(format!("/etc/hosts: {old} -> {new}")))
            });
        }
        self.data.at = None;
    }
}

fn wait_task(api: &Arc<Api>, node: &str, v: Value) -> anyhow::Result<()> {
    let Some(upid) = is_upid(&v) else { return Ok(()) };
    let path = format!("/nodes/{node}/tasks/{}/status", urlenc(&upid));
    for _ in 0..120 {
        std::thread::sleep(Duration::from_millis(500));
        // the API may be briefly unreachable while the network reloads
        if let Ok(st) = api.get(&path) {
            if js(&st, "status") == "stopped" {
                let e = js(&st, "exitstatus");
                if e != "OK" {
                    anyhow::bail!("apply failed: {e}");
                }
                return Ok(());
            }
        }
    }
    anyhow::bail!("timeout while applying the network configuration")
}

impl View for Mgmt {
    fn title(&self) -> &'static str {
        "Management"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let exec = ctx.exec();
        self.data.load(move || {
            let full = api.get_full(&format!("/nodes/{node}/network"))?;
            let ifaces = full.get("data").map(jarr).unwrap_or_default();
            let dns = api.get(&format!("/nodes/{node}/dns")).unwrap_or(Value::Null);
            let hosts = api.get(&format!("/nodes/{node}/hosts")).unwrap_or(Value::Null);
            let nics = match &exec {
                Some(e) => nic_list(e),
                None => ifaces
                    .iter()
                    .filter(|i| js(i, "type") == "eth")
                    .map(|i| Nic { name: js(i, "iface"), link: js(i, "active") == "1", mac: String::new(), speed: "-".into() })
                    .collect(),
            };
            Ok(Data { ifaces, changes: js(&full, "changes"), dns, hosts, nics })
        });
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let mut ch = self.data.poll();
        if let Some(j) = &self.stage {
            if let Some(r) = j.poll() {
                self.stage = None;
                match r {
                    Ok(diff) => {
                        self.status = None;
                        self.data.at = None;
                        self.refresh(ctx);
                        self.ask_apply(ctx, &diff);
                    }
                    Err(e) => self.status = Some((format!("{e:#}"), true)),
                }
                ch = true;
            }
        }
        if let Some(j) = &self.apply {
            if let Some(r) = j.poll() {
                self.apply = None;
                match r {
                    Ok(true) => {
                        self.status = Some((tr("Applied - waiting for confirmation").into(), false));
                        ctx.popup(Popup::countdown(
                            "Keep network configuration?",
                            "keep",
                            vec![
                                tr("The new network configuration is active.").into(),
                                tr("If you can still reach this host, keep it.").into(),
                                tr("Without an answer the old configuration is restored.").into(),
                            ],
                            CONFIRM_SECS,
                        ));
                    }
                    Ok(false) => {
                        self.status = Some((tr("Network configuration applied").into(), false));
                        self.hosts_update = None;
                    }
                    Err(e) => {
                        self.status = Some((format!("{e:#}"), true));
                        self.start_revert(ctx, "apply failed");
                    }
                }
                self.data.at = None;
                ch = true;
            }
        }
        if let Some(j) = &self.revert {
            if let Some(r) = j.poll() {
                self.revert = None;
                self.status = Some(match r {
                    Ok(_) => (tr("Previous network configuration restored").into(), false),
                    Err(e) => (format!("revert failed: {e:#}"), true),
                });
                self.data.at = None;
                ch = true;
            }
        }
        if let Some(j) = &self.rescue_nics {
            if let Some(r) = j.poll() {
                self.rescue_nics = None;
                match r {
                    Ok(list) if !list.is_empty() => {
                        self.found = list;
                        let items: Vec<MenuItem> = self
                            .found
                            .iter()
                            .enumerate()
                            .take(9)
                            .map(|(i, n)| item(char::from(b'1' + i as u8), &format!("{:<12} {}  {}", n.name, n.mac, if n.has_carrier { "LINK UP" } else { "no link" })))
                            .collect();
                        ctx.popup(Popup::menu("Select management NIC", "rnic", items));
                    }
                    Ok(_) => self.status = Some(("no physical network interface found".into(), true)),
                    Err(e) => self.status = Some((format!("{e:#}"), true)),
                }
                ch = true;
            }
        }
        if let Some(j) = &self.rescue_apply {
            if let Some(r) = j.poll() {
                self.rescue_apply = None;
                self.status = Some(match r {
                    Ok(b) => (format!("{} ({b})", tr("Network configuration applied")), false),
                    Err(e) => (format!("{e:#}"), true),
                });
                self.data.at = None;
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

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some((m, st)) = loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none()) {
            kv.styled("", m, st);
        }
        if let Some(d) = &self.data.data {
            kv.section("Current configuration");
            match self.mgmt_iface(ctx) {
                Some(i) => {
                    let ports = match i.get("bridge_ports") {
                        Some(Value::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    kv.styled("Interface", format!("{} ({})", js(i, "iface"), js(i, "type")), Style::default().fg(ACCENT));
                    kv.add("IPv4/CIDR", js(i, "cidr"));
                    kv.add("Gateway", js(i, "gateway"));
                    if !js(i, "cidr6").is_empty() {
                        kv.add("IPv6/CIDR", js(i, "cidr6"));
                        kv.add("Gateway (IPv6)", js(i, "gateway6"));
                    }
                    if !ports.is_empty() {
                        kv.add("Bridge ports", ports);
                    }
                    if !js(i, "mtu").is_empty() {
                        kv.add("MTU", js(i, "mtu"));
                    }
                }
                None => {
                    kv.styled("Interface", tr("no management bridge configured in /etc/network/interfaces"), style_warn());
                }
            }
            let dns: Vec<String> = ["dns1", "dns2", "dns3"].iter().map(|k| js(&d.dns, k)).filter(|s| !s.is_empty()).collect();
            kv.add("DNS servers", if dns.is_empty() { "-".into() } else { dns.join(", ") });
            kv.add("Search domain", js(&d.dns, "search"));
            kv.add("Hostname", ctx.node.clone());
            let host_line = js(&d.hosts, "data").lines().find(|l| l.split_whitespace().skip(1).any(|w| w == ctx.node || w.starts_with(&format!("{}.", ctx.node)))).map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
            kv.add("/etc/hosts", if host_line.is_empty() { "-".into() } else { host_line });
            if !d.changes.is_empty() {
                kv.blank();
                kv.styled("", tr("There are pending changes that are not applied yet (A = apply)"), style_warn());
            }
        }
        if self.stage.is_some() || self.apply.is_some() || self.revert.is_some() || self.rescue_apply.is_some() || self.rescue_nics.is_some() {
            kv.blank();
            kv.styled("", tr("Working..."), Style::default().fg(ACCENT));
        }
        if let Some((m, err)) = &self.status {
            kv.blank();
            kv.styled("", m.clone(), if *err { style_bad() } else { style_good() });
        }
        draw_lines(f, area, "Management network", focused, kv.lines(), self.scroll);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let bridged = self.mgmt_iface(ctx).map(|i| js(i, "bridge_ports")).unwrap_or_default();
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flat_map(|d| d.nics.iter())
            .map(|n| {
                Row::new(n.name.clone())
                    .cs(n.name.clone(), if bridged.split_whitespace().any(|p| p == n.name) { Style::default().fg(ACCENT) } else { Style::default() })
                    .cs(if n.link { "up" } else { "no link" }, if n.link { style_good() } else { style_dim() })
                    .c(if n.speed == "-" || n.speed.starts_with('-') { "-".to_string() } else { format!("{} Mb/s", n.speed) })
                    .c(n.mac.clone())
            })
            .collect();
        let cols = [col("NIC", Constraint::Length(12)), col("Link", Constraint::Length(8)), col("Speed", Constraint::Length(11)), col("MAC", Constraint::Min(17))];
        self.table.draw(f, area, "Physical NICs", &cols, &rows, focused, None);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![]
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let m = ctx.need_node_priv("Sys.Modify");
        let has = self.mgmt_iface(ctx).is_some();
        let pending = self.data.data.as_ref().map(|d| !d.changes.is_empty()).unwrap_or(false);
        let busy = self.stage.is_some() || self.apply.is_some() || self.revert.is_some();
        vec![
            item('e', "Change IP address and gateway").require(m.clone()).disabled_if(!has, "no management bridge").disabled_if(busy, "busy"),
            item('d', "Change DNS servers").require(m.clone()),
            item('b', "Change bridged physical NIC").require(m.clone()).disabled_if(!has, "no management bridge").disabled_if(busy, "busy"),
            item('A', "Apply pending changes").require(m.clone()).disabled_if(!pending, "no pending changes").disabled_if(busy, "busy"),
            item('V', "Discard pending changes").require(m).disabled_if(!pending, "no pending changes"),
            item('P', "Ping the gateway").require(ctx.need_exec("Sys.Audit").map(|_| ())),
            item('R', "Rescue: reset management network (no API)").require(Self::rescue_allowed(ctx)),
            item('F', "Rescue: stop the PVE firewall").require(Self::rescue_allowed(ctx)),
        ]
    }

    fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        match tag {
            "nic" => {
                let nics: Vec<String> = self.data.data.as_ref().map(|d| d.nics.iter().map(|n| n.name.clone()).collect()).unwrap_or_default();
                let idx = (key as u8).wrapping_sub(b'1') as usize;
                if let (Some(nic), Some(i)) = (nics.get(idx), self.mgmt_iface(ctx).cloned()) {
                    let name = js(&i, "iface");
                    let t = js(&i, "type");
                    self.stage_change(ctx, &name, vec![("type", t), ("bridge_ports", nic.clone())]);
                }
                return;
            }
            "rnic" => {
                let idx = (key as u8).wrapping_sub(b'1') as usize;
                if let Some(n) = self.found.get(idx) {
                    self.chosen = Some(n.name.clone());
                    let cur = rescue::get_network_info();
                    ctx.popup(Popup::Form(Form::new(
                        "Reset management network",
                        "rip",
                        vec![
                            form::info("Bridge", &format!("vmbr0 on {}", n.name)),
                            form::text("ip", "IP address", &cur.ip).req(true),
                            form::text("netmask", "Netmask", if cur.netmask.is_empty() { "255.255.255.0" } else { &cur.netmask }).req(true),
                            form::text("gateway", "Gateway", &cur.gateway).req(true),
                        ],
                    )));
                }
                return;
            }
            _ => {}
        }
        match key {
            'e' => {
                if let Some(i) = self.mgmt_iface(ctx) {
                    ctx.popup(Popup::Form(Form::new(
                        &format!("{} {}", tr("Change IP address and gateway"), js(i, "iface")),
                        "ip",
                        vec![
                            form::text("cidr", "IPv4/CIDR", &js(i, "cidr")).req(true).help("e.g. 192.168.1.10/24"),
                            form::text("gateway", "Gateway", &js(i, "gateway")),
                            form::text("cidr6", "IPv6/CIDR", &js(i, "cidr6")),
                            form::text("gateway6", "Gateway (IPv6)", &js(i, "gateway6")),
                        ],
                    )));
                }
            }
            'd' => {
                let d = self.data.data.as_ref().map(|d| d.dns.clone()).unwrap_or(Value::Null);
                ctx.popup(Popup::Form(Form::new(
                    "Change DNS servers",
                    "dns",
                    vec![
                        form::text("search", "Search domain", &js(&d, "search")).req(true),
                        form::text("dns1", "DNS1", &js(&d, "dns1")),
                        form::text("dns2", "DNS2", &js(&d, "dns2")),
                        form::text("dns3", "DNS3", &js(&d, "dns3")),
                    ],
                )));
            }
            'b' => {
                let items: Vec<MenuItem> = self
                    .data
                    .data
                    .iter()
                    .flat_map(|d| d.nics.iter())
                    .take(9)
                    .enumerate()
                    .map(|(i, n)| item(char::from(b'1' + i as u8), &format!("{:<12} {:<8} {}", n.name, if n.link { "up" } else { "no link" }, n.mac)))
                    .collect();
                if items.is_empty() {
                    ctx.popup(Popup::error("Change bridged physical NIC", "no physical NIC found"));
                } else {
                    ctx.popup(Popup::menu("Change bridged physical NIC", "nic", items));
                }
            }
            'A' => {
                let diff = self.data.data.as_ref().map(|d| d.changes.clone()).unwrap_or_default();
                self.ask_apply(ctx, &diff);
            }
            'V' => ctx.popup(Popup::confirm("Discard pending changes", "revert_pending", vec![tr("Discard all pending network changes?").into()], false)),
            'P' => {
                let gw = self.mgmt_iface(ctx).map(|i| js(i, "gateway")).filter(|g| !g.is_empty()).unwrap_or_else(|| rescue::get_network_info().gateway);
                if let (false, Some(e)) = (gw.is_empty(), ctx.exec()) {
                    ctx.open_log(&format!("ping {gw}"), Source::Command { exec: e, prog: "ping".into(), args: vec!["-c".into(), "5".into(), "-W".into(), "2".into(), gw] });
                } else {
                    ctx.toast_err("no default gateway");
                }
            }
            'R' => {
                self.status = None;
                self.rescue_nics = Some(Job::spawn(|| Ok(rescue::detect_physical_interfaces())));
            }
            'F' => ctx.popup(Popup::confirm(
                "Stop firewall",
                "fwstop",
                vec![tr("Stop the PVE firewall on this node?").into(), tr("All PVE firewall rules stop being enforced until it is started again.").into()],
                true,
            )),
            _ => {}
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        match tag {
            "ip" => {
                let Some(i) = self.mgmt_iface(ctx).cloned() else { return };
                let cidr = fv(v, "cidr").to_string();
                let Some((ip, prefix)) = cidr.split_once('/') else {
                    ctx.popup(Popup::error("IPv4/CIDR", "use address/prefix, e.g. 192.168.1.10/24"));
                    return;
                };
                let gw = fv(v, "gateway").to_string();
                let mask = rescue::cidr_to_netmask(prefix.parse().unwrap_or(0));
                if !gw.is_empty() {
                    if let Err(e) = rescue::validate_network_config(&NetworkInfo { ip: ip.into(), netmask: mask, gateway: gw.clone(), iface: String::new() }) {
                        ctx.popup(Popup::error("Invalid network configuration", &e));
                        return;
                    }
                }
                let old_ip = js(&i, "cidr").split('/').next().unwrap_or("").to_string();
                self.hosts_update = (old_ip != ip && !old_ip.is_empty()).then(|| (old_ip, ip.to_string()));
                let mut params: Vec<(&'static str, String)> = vec![("type", js(&i, "type")), ("cidr", cidr)];
                let mut del = Vec::new();
                for k in ["gateway", "cidr6", "gateway6"] {
                    let val = fv(v, k);
                    if val.is_empty() {
                        if !js(&i, k).is_empty() {
                            del.push(k);
                        }
                    } else {
                        params.push((k, val.to_string()));
                    }
                }
                // keep the bridge settings PVE would otherwise reset
                for k in ["bridge_ports", "bridge_stp", "bridge_fd", "bridge_vlan_aware", "mtu", "comments", "autostart"] {
                    let val = js(&i, k);
                    if !val.is_empty() {
                        params.push((k, val));
                    }
                }
                if !del.is_empty() {
                    params.push(("delete", del.join(",")));
                }
                let name = js(&i, "iface");
                self.stage_change(ctx, &name, params);
            }
            "dns" => {
                let mut params: Vec<(&'static str, String)> = vec![("search", fv(v, "search").into())];
                let mut del = Vec::new();
                for k in ["dns1", "dns2", "dns3"] {
                    if fv(v, k).is_empty() {
                        del.push(k);
                    } else {
                        params.push((k, fv(v, k).into()));
                    }
                }
                if !del.is_empty() {
                    params.push(("delete", del.join(",")));
                }
                let n = ctx.node.clone();
                ctx.api_call("Update DNS", "PUT", format!("/nodes/{n}/dns"), params);
            }
            "rip" => {
                let iface = self.chosen.clone().unwrap_or_default();
                self.draft = NetworkInfo { ip: fv(v, "ip").into(), netmask: fv(v, "netmask").into(), gateway: fv(v, "gateway").into(), iface: "vmbr0".into() };
                if let Err(e) = rescue::validate_network_config(&self.draft) {
                    ctx.popup(Popup::error("Invalid network configuration", &e));
                    return;
                }
                let mut body: Vec<String> = vec![tr("/etc/network/interfaces will be replaced with:").into(), String::new()];
                body.extend(rescue::build_network_config(&iface, &self.draft).lines().map(|s| format!("  {s}")));
                ctx.popup(Popup::confirm("Reset management network", "rapply", body, true));
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        match tag {
            "apply" => self.start_apply(ctx),
            "keep:keep" => self.keep(ctx),
            "keep:revert" => self.start_revert(ctx, "declined"),
            "keep:timeout" => self.start_revert(ctx, "no confirmation"),
            "revert_pending" => {
                let n = ctx.node.clone();
                self.hosts_update = None;
                ctx.api_call("Discard network changes", "DELETE", format!("/nodes/{n}/network"), vec![]);
            }
            "rapply" => {
                let iface = self.chosen.clone().unwrap_or_default();
                let draft = self.draft.clone();
                ctx.audit(&format!("rescue network reset {iface} {}", draft.ip));
                self.rescue_apply = Some(Job::spawn(move || rescue::apply_network_config(&iface, &draft).map_err(|e| anyhow::anyhow!(e))));
            }
            "fwstop" => ctx.exec_call("pve-firewall stop", Exec::Local, "pve-firewall", vec!["stop".into()]),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.data.clear();
        self.status = None;
    }
}
