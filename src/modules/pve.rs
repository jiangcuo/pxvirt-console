//! PVE module: cluster/nodes, guests (VM + CT, multi-architecture), storage, backup, HA, replication.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    Frame,
};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

use super::pve_create::{self, CreateInfo};
use super::{fv, scroll, View};
use crate::ctx::{Cmd, Ctx, Suspend, VncRequest};
use crate::i18n::tr;
use crate::loader::{Job, Loader};
use crate::ui::form::{self, Form};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, status_style, style_bad, style_dim, style_good, style_warn, usage_style, Kv, ACCENT};
use crate::util::{bar, fmt_bytes, fmt_duration, fmt_time, jarr, jb, jf, ji, js, jsd, urlenc};

pub fn views() -> Vec<Box<dyn View>> {
    vec![
        Box::new(Cluster::default()),
        Box::new(Guests::default()),
        Box::new(Storage::default()),
        Box::new(Backup::default()),
        Box::new(Ha::default()),
        Box::new(Replication::default()),
    ]
}

/// Canonical architecture name (QEMU style) from QEMU/LXC/uname spellings.
pub fn arch_norm(a: &str) -> String {
    match a {
        "amd64" | "x86_64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        "loong64" | "loongarch64" => "loongarch64",
        "riscv64" => "riscv64",
        "i386" | "i686" => "i386",
        "armhf" | "armv7l" => "armhf",
        "ppc64el" | "ppc64le" => "ppc64le",
        "s390x" => "s390x",
        other => other,
    }
    .to_string()
}

pub fn arch_short(a: &str) -> &str {
    match a {
        "x86_64" => "x86",
        "aarch64" => "arm64",
        "loongarch64" => "la64",
        "riscv64" => "rv64",
        "ppc64le" => "ppc64",
        "" => "?",
        other => other,
    }
}

// ===========================================================================
// Cluster
// ===========================================================================

#[derive(Default)]
struct ClusterData {
    status: Vec<Value>,
    nodes: Vec<Value>,
    guests: Vec<Value>,
    arch: HashMap<String, String>,
    ha: Vec<Value>,
    version: String,
}

#[derive(Default)]
struct Cluster {
    data: Loader<ClusterData>,
    table: Table,
    scroll: u16,
}

impl View for Cluster {
    fn title(&self) -> &'static str {
        "Cluster"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let known = ctx.node_arch.clone();
        self.data.load(move || {
            let status = jarr(&api.get("/cluster/status")?);
            let nodes = jarr(&api.get("/cluster/resources?type=node").unwrap_or(Value::Null));
            let guests = jarr(&api.get("/cluster/resources?type=vm").unwrap_or(Value::Null));
            let ha = jarr(&api.get("/cluster/ha/status/current").unwrap_or(Value::Null));
            let version = api.get("/version").map(|v| js(&v, "version")).unwrap_or_default();
            let mut arch = known;
            for n in &nodes {
                let name = js(n, "node");
                if js(n, "status") == "online" && !arch.contains_key(&name) {
                    if let Ok(s) = api.get(&format!("/nodes/{name}/status")) {
                        let a = s.get("current-kernel").map(|k| js(k, "machine")).unwrap_or_default();
                        if !a.is_empty() {
                            arch.insert(name, arch_norm(&a));
                        }
                    }
                }
            }
            Ok(ClusterData { status, nodes, guests, arch, ha, version })
        });
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let ch = self.data.poll();
        if ch {
            if let Some(d) = &self.data.data {
                for (k, v) in &d.arch {
                    ctx.node_arch.insert(k.clone(), v.clone());
                }
                // keep the node list (for F2) up to date
                let mut nodes = Vec::new();
                for e in &d.status {
                    if js(e, "type") == "node" {
                        nodes.push(crate::ctx::NodeInfo { name: js(e, "name"), ip: js(e, "ip"), online: jb(e, "online"), local: jb(e, "local") });
                    }
                }
                if !nodes.is_empty() {
                    nodes.sort_by(|a, b| a.name.cmp(&b.name));
                    ctx.nodes = nodes;
                }
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
        let mut rows = Vec::new();
        if let Some(d) = &self.data.data {
            for n in &d.nodes {
                let name = js(n, "node");
                let online = js(n, "status") == "online";
                let st = d.status.iter().find(|s| js(s, "type") == "node" && js(s, "name") == name);
                let ip = st.map(|s| js(s, "ip")).unwrap_or_default();
                let local = st.map(|s| jb(s, "local")).unwrap_or(false);
                let cpu = jf(n, "cpu");
                let (mem, maxmem) = (jf(n, "mem"), jf(n, "maxmem"));
                let mf = if maxmem > 0.0 { mem / maxmem } else { 0.0 };
                let mine: Vec<&Value> = d.guests.iter().filter(|g| js(g, "node") == name).collect();
                let running = mine.iter().filter(|g| js(g, "status") == "running").count();
                let mut label = name.clone();
                if local {
                    label.push_str(" (local)");
                }
                rows.push(
                    Row::new(name.clone())
                        .cs(label, if name == ctx.node { Style::default().fg(ACCENT) } else { Style::default() })
                        .cs(if online { "online" } else { "OFFLINE" }, if online { style_good() } else { style_bad() })
                        .c(ip)
                        .c(arch_short(d.arch.get(&name).map(|s| s.as_str()).unwrap_or("")).to_string())
                        .cs(format!("{:.0}% of {}", cpu * 100.0, js(n, "maxcpu")), usage_style(cpu))
                        .cs(format!("{} {:.0}%", bar(mf, 8), mf * 100.0), usage_style(mf))
                        .c(format!("{running}/{}", mine.len()))
                        .c(fmt_duration(jf(n, "uptime") as u64))
                        .dim(!online),
                );
            }
        }
        let cols = [
            col("Node", Constraint::Min(14)),
            col("Status", Constraint::Length(8)),
            col("IP", Constraint::Length(15)),
            col("Arch", Constraint::Length(6)),
            col("CPU", Constraint::Length(11)),
            col("Memory", Constraint::Length(14)),
            col("Guests", Constraint::Length(7)),
            col("Uptime", Constraint::Length(12)),
        ];
        self.table.draw(f, area, "Nodes", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(d) = &self.data.data {
            let cl = d.status.iter().find(|s| js(s, "type") == "cluster");
            match cl {
                Some(c) => {
                    kv.add("Cluster", js(c, "name"));
                    let q = jb(c, "quorate");
                    kv.styled("Quorate", if q { "yes" } else { "NO" }, if q { style_good() } else { style_bad() });
                    kv.add("Nodes", format!("{} online / {}", d.nodes.iter().filter(|n| js(n, "status") == "online").count(), d.nodes.len()));
                    kv.add("Config version", js(c, "version"));
                }
                None => {
                    kv.add("Cluster", "standalone node");
                }
            }
            kv.add("PVE", d.version.clone());
            let running = d.guests.iter().filter(|g| js(g, "status") == "running").count();
            kv.add("Guests", format!("{running} running / {} total", d.guests.len()));
            // per architecture summary
            kv.section("Architectures");
            let mut arches: Vec<String> = d.arch.values().cloned().collect();
            arches.sort();
            arches.dedup();
            for a in arches {
                let nodes: Vec<String> = d.arch.iter().filter(|(_, v)| **v == a).map(|(k, _)| k.clone()).collect();
                let guests = d.guests.iter().filter(|g| nodes.contains(&js(g, "node"))).count();
                kv.add(&a, format!("{} node(s), {guests} guest(s)", nodes.len()));
            }
            if !d.ha.is_empty() {
                kv.section("HA");
                for h in &d.ha {
                    let t = js(h, "type");
                    if t == "quorum" || t == "master" || t == "lrm" {
                        let st = js(h, "status");
                        kv.styled(&format!("{t} {}", js(h, "node")), st.clone(), if st.contains("error") || st.contains("fence") { style_bad() } else { Style::default() });
                    }
                }
            }
        }
        draw_lines(f, area, "Cluster", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(n) = self.table.selected() else { return vec![] };
        let online = self.data.data.as_ref().and_then(|d| d.nodes.iter().find(|x| js(x, "node") == n)).map(|x| js(x, "status") == "online").unwrap_or(false);
        vec![
            item('w', "Switch to this node").disabled_if(!online, "offline").disabled_if(n == ctx.node, "current node"),
            item('S', "Shell on this node").require(ctx.need_root()).disabled_if(ctx.exec_for(n).is_none(), "no ssh access").disabled_if(!online, "offline"),
        ]
    }

    fn on_key(&mut self, key: &KeyEvent, ctx: &mut Ctx) -> bool {
        if key.code == KeyCode::Enter {
            if let Some(n) = self.table.selected().map(|s| s.to_string()) {
                if n != ctx.node {
                    ctx.cmds.push(Cmd::SwitchNode(n));
                    return true;
                }
            }
        }
        false
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(n) = self.table.selected().map(|s| s.to_string()) else { return };
        match key {
            'w' => ctx.cmds.push(Cmd::SwitchNode(n)),
            'S' => {
                if let Some(exec) = ctx.exec_for(&n) {
                    ctx.cmds.push(Cmd::Suspend(Suspend {
                        banner: format!("Shell on {n} ({}). Type 'exit' to return.\n", exec.label()),
                        exec,
                        prog: "/bin/bash".into(),
                        args: vec!["-l".into()],
                        env: vec![],
                        logout_after: false,
                        pause_after: false,
                    }));
                }
            }
            _ => {}
        }
    }
}

// ===========================================================================
// Guests
// ===========================================================================

#[derive(Default, Clone)]
struct GuestDetail {
    config: Value,
    status: Value,
    ips: Vec<(String, String)>,
    snapshots: usize,
}

struct ConfBatch {
    confs: Vec<(String, Value)>,
    node_arch: HashMap<String, String>,
}

#[derive(Default)]
struct Guests {
    data: Loader<Vec<Value>>,
    confs: HashMap<String, Value>,
    batch: Loader<ConfBatch>,
    detail: HashMap<String, Loader<GuestDetail>>,
    table: Table,
    filter_mode: usize,
    sort: usize,
    scroll: u16,
    snaps: Option<(String, Job<Vec<Value>>)>,
    snap_list: Vec<Value>,
    snap_guest: String,
    create: Option<(bool, Job<CreateInfo>)>,
    nodes_for_form: Vec<String>,
    initialized: bool,
}

const GFILTERS: [&str; 7] = ["all", "running", "stopped", "VM", "CT", "foreign arch", "templates"];
const GSORTS: [&str; 5] = ["id", "name", "cpu", "mem", "node"];

fn gkey(g: &Value) -> String {
    format!("{}/{}/{}", js(g, "node"), js(g, "type"), js(g, "vmid"))
}

impl Guests {
    fn find(&self, key: &str) -> Option<&Value> {
        self.data.data.as_ref()?.iter().find(|g| gkey(g) == key)
    }

    fn sel(&self) -> Option<&Value> {
        self.find(self.table.selected()?)
    }

    fn arch_of(&self, g: &Value, ctx: &Ctx) -> (String, bool) {
        let node_arch = ctx.node_arch.get(&js(g, "node")).cloned().unwrap_or_default();
        let conf = self.confs.get(&gkey(g));
        let a = conf.map(|c| js(c, "arch")).filter(|a| !a.is_empty()).map(|a| arch_norm(&a));
        match a {
            Some(a) => {
                let foreign = !node_arch.is_empty() && a != node_arch;
                (a, foreign)
            }
            None if conf.is_some() || js(g, "type") == "qemu" => (node_arch, false),
            None => (String::new(), false),
        }
    }

    fn targets(&self) -> Vec<Value> {
        self.table.targets().iter().filter_map(|k| self.find(k).cloned()).collect()
    }

    fn base(g: &Value) -> String {
        format!("/nodes/{}/{}/{}", js(g, "node"), js(g, "type"), js(g, "vmid"))
    }

    fn power(&mut self, ctx: &mut Ctx, cmd: &str, label: &str) {
        let targets = self.targets();
        for g in targets {
            let name = format!("{label} {} {}", js(&g, "vmid"), js(&g, "name"));
            ctx.api_call(name, "POST", format!("{}/status/{cmd}", Self::base(&g)), vec![]);
        }
        self.table.marked.clear();
    }

    fn open_snapshots(&mut self, ctx: &mut Ctx) {
        let items: Vec<MenuItem> = {
            let mut v = vec![item('+', "Take snapshot").require(ctx.need_priv(&format!("/vms/{}", self.snap_guest.rsplit('/').next().unwrap_or("")), "VM.Snapshot"))];
            let keys: Vec<char> = "123456789abcdefghijklmnopqrstuvwxyz".chars().collect();
            for (i, s) in self.snap_list.iter().filter(|s| js(s, "name") != "current").enumerate().take(keys.len()) {
                let t = ji(s, "snaptime");
                v.push(item(
                    keys[i],
                    &format!(
                        "{:<20} {}  {}{}",
                        js(s, "name"),
                        if t > 0 { fmt_time(t) } else { String::new() },
                        js(s, "description").lines().next().unwrap_or(""),
                        if jb(s, "vmstate") { " [RAM]" } else { "" }
                    ),
                ));
            }
            v
        };
        let title = format!("{} {}", tr("Snapshots"), self.snap_guest);
        ctx.popup(Popup::menu(&title, "snaps", items));
    }
}

impl View for Guests {
    fn title(&self) -> &'static str {
        "Guests"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        if !self.initialized {
            self.table.multi = true;
            self.initialized = true;
        }
        let api = ctx.api.clone();
        self.data.load(move || {
            let mut v = jarr(&api.get("/cluster/resources?type=vm")?);
            v.sort_by_key(|g| ji(g, "vmid"));
            Ok(v)
        });
        for l in self.detail.values_mut() {
            l.at = None;
        }
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let mut ch = self.data.poll();
        if self.batch.poll() {
            if let Some(b) = self.batch.data.take() {
                for (k, v) in b.confs {
                    self.confs.insert(k, v);
                }
                for (k, v) in b.node_arch {
                    ctx.node_arch.insert(k, v);
                }
            }
            ch = true;
        }
        // background: load configs (architecture) of guests not seen yet
        if !self.batch.loading() && self.batch.stale(Duration::from_secs(5)) {
            if let Some(list) = &self.data.data {
                let missing: Vec<(String, String, String, String)> = list
                    .iter()
                    .filter(|g| !self.confs.contains_key(&gkey(g)) && js(g, "status") != "unknown")
                    .take(64)
                    .map(|g| (gkey(g), js(g, "node"), js(g, "type"), js(g, "vmid")))
                    .collect();
                let mut nodes: Vec<String> = list.iter().map(|g| js(g, "node")).filter(|n| !ctx.node_arch.contains_key(n)).collect();
                nodes.sort();
                nodes.dedup();
                if !missing.is_empty() || !nodes.is_empty() {
                    let api = ctx.api.clone();
                    self.batch.load(move || {
                        let mut confs = Vec::new();
                        for (k, node, t, id) in missing {
                            match api.get(&format!("/nodes/{node}/{t}/{id}/config")) {
                                Ok(c) => confs.push((k, c)),
                                Err(_) => confs.push((k, Value::Null)),
                            }
                        }
                        let mut node_arch = HashMap::new();
                        for n in nodes {
                            if let Ok(s) = api.get(&format!("/nodes/{n}/status")) {
                                let a = s.get("current-kernel").map(|k| js(k, "machine")).unwrap_or_default();
                                if !a.is_empty() {
                                    node_arch.insert(n, arch_norm(&a));
                                }
                            }
                        }
                        Ok(ConfBatch { confs, node_arch })
                    });
                }
            }
        }
        // details for the selection
        for l in self.detail.values_mut() {
            ch |= l.poll();
        }
        if let Some(g) = self.sel().cloned() {
            let k = gkey(&g);
            let l = self.detail.entry(k.clone()).or_default();
            if l.stale(Duration::from_secs(10)) {
                let api = ctx.api.clone();
                l.load(move || {
                    let base = Guests::base(&g);
                    let config = api.get(&format!("{base}/config"))?;
                    let status = api.get(&format!("{base}/status/current")).unwrap_or(Value::Null);
                    let running = js(&status, "status") == "running";
                    let mut ips = Vec::new();
                    if running && js(&g, "type") == "qemu" && js(&config, "agent").starts_with('1') {
                        if let Ok(v) = api.get(&format!("{base}/agent/network-get-interfaces")) {
                            for i in v.get("result").map(jarr).unwrap_or_default() {
                                let name = js(&i, "name");
                                if name == "lo" {
                                    continue;
                                }
                                for a in i.get("ip-addresses").map(jarr).unwrap_or_default() {
                                    let ip = js(&a, "ip-address");
                                    if !ip.starts_with("fe80") && !ip.starts_with("127.") && ip != "::1" {
                                        ips.push((name.clone(), format!("{ip}/{}", js(&a, "prefix"))));
                                    }
                                }
                            }
                        }
                    } else if running && js(&g, "type") == "lxc" {
                        if let Ok(v) = api.get(&format!("{base}/interfaces")) {
                            for i in jarr(&v) {
                                let name = js(&i, "name");
                                if name == "lo" {
                                    continue;
                                }
                                for k in ["inet", "inet6"] {
                                    let ip = js(&i, k);
                                    if !ip.is_empty() && !ip.starts_with("fe80") {
                                        ips.push((name.clone(), ip));
                                    }
                                }
                            }
                        }
                    }
                    let snapshots = jarr(&api.get(&format!("{base}/snapshot")).unwrap_or(Value::Null)).iter().filter(|s| js(s, "name") != "current").count();
                    Ok(GuestDetail { config, status, ips, snapshots })
                });
            }
            if let Some(Some(c)) = self.detail.get(&k).map(|d| d.data.as_ref().map(|d| d.config.clone())) {
                self.confs.insert(k, c);
            }
        }
        // snapshot list
        if let Some((guest, job)) = &self.snaps {
            if let Some(r) = job.poll() {
                let guest = guest.clone();
                self.snaps = None;
                match r {
                    Ok(list) => {
                        self.snap_list = list;
                        self.snap_guest = guest;
                        self.open_snapshots(ctx);
                    }
                    Err(e) => ctx.popup(Popup::error("Snapshots", &format!("{e:#}"))),
                }
            }
        }
        // create wizard data
        if let Some((is_vm, job)) = &self.create {
            if let Some(r) = job.poll() {
                let is_vm = *is_vm;
                self.create = None;
                match r {
                    Ok(info) => {
                        let f = if is_vm { pve_create::vm_form(&info) } else { pve_create::ct_form(&info) };
                        ctx.popup(Popup::Form(f));
                    }
                    Err(e) => ctx.popup(Popup::error("Create", &format!("{e:#}"))),
                }
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

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("Space", "mark"), ("f", "show"), ("o", "sort")]
    }

    fn on_key(&mut self, key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        match key.code {
            KeyCode::Char('f') => {
                self.filter_mode = (self.filter_mode + 1) % GFILTERS.len();
                true
            }
            KeyCode::Char('o') => {
                self.sort = (self.sort + 1) % GSORTS.len();
                true
            }
            _ => false,
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut list: Vec<Value> = self.data.data.clone().unwrap_or_default();
        list.retain(|g| {
            let running = js(g, "status") == "running";
            match self.filter_mode {
                1 => running,
                2 => !running,
                3 => js(g, "type") == "qemu",
                4 => js(g, "type") == "lxc",
                5 => self.arch_of(g, ctx).1,
                6 => jb(g, "template"),
                _ => true,
            }
        });
        match self.sort {
            1 => list.sort_by_key(|g| js(g, "name").to_lowercase()),
            2 => list.sort_by(|a, b| jf(b, "cpu").partial_cmp(&jf(a, "cpu")).unwrap_or(std::cmp::Ordering::Equal)),
            3 => list.sort_by(|a, b| jf(b, "mem").partial_cmp(&jf(a, "mem")).unwrap_or(std::cmp::Ordering::Equal)),
            4 => list.sort_by_key(|g| (js(g, "node"), ji(g, "vmid"))),
            _ => list.sort_by_key(|g| ji(g, "vmid")),
        }
        let rows: Vec<Row> = list
            .iter()
            .map(|g| {
                let status = js(g, "status");
                let running = status == "running";
                let tmpl = jb(g, "template");
                let (arch, foreign) = self.arch_of(g, ctx);
                let (mem, maxmem) = (jf(g, "mem"), jf(g, "maxmem"));
                let mut st = status.clone();
                let lock = js(g, "lock");
                if !lock.is_empty() {
                    st = format!("{st} [{lock}]");
                }
                let ha = js(g, "hastate");
                Row::new(gkey(g))
                    .cs(if tmpl { "T" } else if running { "▲" } else { "▼" }, if running { style_good() } else if tmpl { style_dim() } else { style_bad() })
                    .c(js(g, "vmid"))
                    .c(js(g, "name"))
                    .c(if js(g, "type") == "qemu" { "VM" } else { "CT" })
                    .cs(arch_short(&arch).to_string(), if foreign { style_warn() } else { style_dim() })
                    .cs(js(g, "node"), if js(g, "node") == ctx.node { Style::default() } else { style_dim() })
                    .cs(st, if lock.is_empty() { status_style(&status) } else { style_warn() })
                    .c(if running { format!("{:.0}%", jf(g, "cpu") * 100.0) } else { String::new() })
                    .c(if running { format!("{}/{}", fmt_bytes(mem), fmt_bytes(maxmem)) } else { fmt_bytes(maxmem) })
                    .c(if running { fmt_duration(jf(g, "uptime") as u64) } else { String::new() })
                    .c(if ha.is_empty() { js(g, "tags") } else { format!("HA:{ha} {}", js(g, "tags")) })
                    .dim(!running)
            })
            .collect();
        let cols = [
            col("", Constraint::Length(1)),
            col("ID", Constraint::Length(5)),
            col("Name", Constraint::Min(12)),
            col("Type", Constraint::Length(4)),
            col("Arch", Constraint::Length(5)),
            col("Node", Constraint::Length(10)),
            col("Status", Constraint::Length(9)),
            col("CPU", Constraint::Length(4)),
            col("Memory", Constraint::Length(13)),
            col("Uptime", Constraint::Length(10)),
            col("Tags/HA", Constraint::Length(12)),
        ];
        let title = format!("{} [{} · sort {}]", tr("Guests"), GFILTERS[self.filter_mode], GSORTS[self.sort]);
        self.table.draw(f, area, &title, &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(g) = self.sel().cloned() {
            let k = gkey(&g);
            let (arch, foreign) = self.arch_of(&g, ctx);
            let is_vm = js(&g, "type") == "qemu";
            kv.add("ID", format!("{} ({})", js(&g, "vmid"), if is_vm { "QEMU VM" } else { "LXC container" }));
            kv.add("Name", js(&g, "name"));
            kv.add("Node", js(&g, "node"));
            kv.status("Status", js(&g, "status"));
            kv.styled(
                "Arch",
                if foreign { format!("{arch} (emulated on {} host, TCG)", ctx.node_arch.get(&js(&g, "node")).cloned().unwrap_or_default()) } else { arch.clone() },
                if foreign { style_warn() } else { Style::default() },
            );
            if let Some(d) = self.detail.get(&k).and_then(|l| l.data.as_ref()) {
                let c = &d.config;
                let s = &d.status;
                if js(s, "status") == "running" {
                    let cpu = jf(s, "cpu");
                    kv.styled("CPU", format!("{:.1}% of {} vCPU", cpu * 100.0, js(s, "cpus")), usage_style(cpu));
                    let (m, mm) = (jf(s, "mem"), jf(s, "maxmem"));
                    kv.styled("Memory", format!("{} / {}", fmt_bytes(m), fmt_bytes(mm)), usage_style(if mm > 0.0 { m / mm } else { 0.0 }));
                    kv.add("Uptime", fmt_duration(jf(s, "uptime") as u64));
                    kv.add("Net in/out", format!("{} / {}", fmt_bytes(jf(s, "netin")), fmt_bytes(jf(s, "netout"))));
                    kv.add("Disk r/w", format!("{} / {}", fmt_bytes(jf(s, "diskread")), fmt_bytes(jf(s, "diskwrite"))));
                    if is_vm {
                        kv.add("QEMU", format!("{} {}", js(s, "running-qemu"), js(s, "running-machine")));
                    }
                }
                if !d.ips.is_empty() {
                    kv.section("IP addresses");
                    for (i, ip) in &d.ips {
                        kv.add(i, ip.clone());
                    }
                }
                kv.section("Configuration");
                if is_vm {
                    kv.add("CPU", format!("{} socket(s) x {} core(s), type {}", if js(c, "sockets").is_empty() { "1".into() } else { js(c, "sockets") }, if js(c, "cores").is_empty() { "1".into() } else { js(c, "cores") }, if js(c, "cpu").is_empty() { "default".into() } else { js(c, "cpu") }));
                    kv.add("Memory", format!("{} MiB", if js(c, "memory").is_empty() { "512".into() } else { js(c, "memory") }));
                    kv.add("Machine", if js(c, "machine").is_empty() { "default".into() } else { js(c, "machine") });
                    kv.add("BIOS", if js(c, "bios").is_empty() { "seabios".into() } else { js(c, "bios") });
                    kv.add("OS type", js(c, "ostype"));
                    kv.add("KVM", if js(c, "kvm") == "0" { "disabled (TCG)".to_string() } else { "enabled".to_string() });
                    kv.add("Agent", js(c, "agent"));
                    kv.add("Boot", js(c, "boot"));
                } else {
                    kv.add("Hostname", js(c, "hostname"));
                    kv.add("OS", js(c, "ostype"));
                    kv.add("Cores", js(c, "cores"));
                    kv.add("Memory", format!("{} MiB, swap {} MiB", js(c, "memory"), js(c, "swap")));
                    kv.add("Unprivileged", js(c, "unprivileged"));
                    kv.add("Features", js(c, "features"));
                }
                kv.add("Start at boot", if jb(c, "onboot") { "yes" } else { "no" });
                kv.add("Snapshots", d.snapshots.to_string());
                if let Value::Object(m) = c {
                    let mut keys: Vec<&String> = m
                        .keys()
                        .filter(|k| {
                            ["scsi", "virtio", "sata", "ide", "efidisk", "tpmstate", "rootfs", "mp", "net", "hostpci", "usb", "serial"]
                                .iter()
                                .any(|p| k.starts_with(p) && k[p.len()..].chars().all(|ch| ch.is_ascii_digit()))
                        })
                        .collect();
                    keys.sort();
                    if !keys.is_empty() {
                        kv.section("Devices");
                    }
                    for key in keys {
                        kv.add(key, js(c, key));
                    }
                }
                let desc = js(c, "description");
                if !desc.is_empty() {
                    kv.section("Notes");
                    for l in desc.lines().take(10) {
                        kv.add("", l.to_string());
                    }
                }
            } else if let Some(e) = self.detail.get(&k).and_then(|l| l.error.clone()) {
                kv.styled("", e, style_bad());
            } else {
                kv.styled("", tr("Loading..."), style_dim());
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu_title(&self) -> String {
        let t = self.table.targets();
        if t.len() > 1 {
            format!("{} ({} {})", tr("Actions"), t.len(), tr("Guests"))
        } else {
            self.sel().map(|g| format!("{} {}", js(g, "vmid"), js(g, "name"))).unwrap_or_else(|| tr("Actions").into())
        }
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let create = ctx.need_priv("/vms", "VM.Allocate");
        let mut v = Vec::new();
        if let Some(g) = self.sel() {
            let path = format!("/vms/{}", js(g, "vmid"));
            let p = ctx.need_priv(&path, "VM.PowerMgmt");
            let running = js(g, "status") == "running";
            let paused = self.detail.get(&gkey(g)).and_then(|d| d.data.as_ref()).map(|d| js(&d.status, "qmpstatus") == "paused").unwrap_or(false);
            let is_vm = js(g, "type") == "qemu";
            let tmpl = jb(g, "template");
            let multi = self.table.marked.len() > 1;
            let exec = ctx.exec_for(&js(g, "node"));
            v.extend([
                item('s', "Start").require(p.clone()).disabled_if(running && !multi, "running").disabled_if(tmpl, "template"),
                item('d', "Shutdown").require(p.clone()).disabled_if(!running && !multi, "not running"),
                item('D', "Stop (force)").require(p.clone()).disabled_if(!running && !multi, "not running"),
                item('t', "Reboot").require(p.clone()).disabled_if(!running && !multi, "not running"),
                item('T', "Reset (hard)").require(p.clone()).disabled_if(!is_vm, "VM only").disabled_if(!running && !multi, "not running"),
                item('p', "Suspend").require(p.clone()).disabled_if(!running || paused, "not running"),
                item('u', "Resume").require(p).disabled_if(!paused && !multi, "not paused"),
            ]);
            if !multi {
                v.extend([
                    item('c', "Text console")
                        .require(ctx.need_root())
                        .disabled_if(exec.is_none(), "needs local/ssh access")
                        .disabled_if(!running, "not running")
                        .disabled_if(is_vm && self.confs.get(&gkey(g)).map(|c| js(c, "serial0").is_empty()).unwrap_or(false), "no serial0 port"),
                    item('v', "Graphical console").require(ctx.need_priv(&path, "VM.Console")).disabled_if(!is_vm, "VM only").disabled_if(!running, "not running"),
                    item('n', "Snapshots").require(ctx.need_priv(&path, "VM.Audit")),
                    item('b', "Backup now").require(ctx.need_priv(&path, "VM.Backup")),
                    item('M', "Migrate").require(ctx.need_priv(&path, "VM.Migrate")).disabled_if(ctx.nodes.len() < 2, "single node"),
                    item('C', "Clone").require(ctx.need_priv(&path, "VM.Clone")),
                    item('e', "Edit").require(ctx.need_priv(&path, "VM.Config.Options")),
                    item('H', "Add to / remove from HA").require(ctx.need_priv("/", "Sys.Console")),
                    item('U', "Unlock").require(ctx.need_root()).disabled_if(js(g, "lock").is_empty(), "not locked"),
                    item('X', "Delete").require(ctx.need_priv(&path, "VM.Allocate")).disabled_if(running, "stop it first"),
                ]);
            }
        }
        v.push(item('N', "Create VM").require(create.clone()));
        v.push(item('O', "Create CT").require(create));
        v
    }

    fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        match tag {
            "snaps" => {
                if key == '+' {
                    let is_vm = self.snap_guest.contains("/qemu/");
                    let mut fields = vec![
                        form::text("snapname", "Name", &format!("snap{}", chrono::Local::now().format("%Y%m%d%H%M"))).req(true),
                        form::text("description", "Description", ""),
                    ];
                    if is_vm {
                        fields.push(form::check("vmstate", "Include RAM", false));
                    }
                    ctx.popup(Popup::Form(Form::new("Take snapshot", "snap", fields)));
                } else {
                    let keys: Vec<char> = "123456789abcdefghijklmnopqrstuvwxyz".chars().collect();
                    let list: Vec<&Value> = self.snap_list.iter().filter(|s| js(s, "name") != "current").collect();
                    if let Some(s) = keys.iter().position(|c| *c == key).and_then(|i| list.get(i)) {
                        let name = js(s, "name");
                        ctx.popup(Popup::menu(
                            &format!("{} {name}", tr("Snapshots")),
                            &format!("snap:{name}"),
                            vec![item('R', "Rollback to this snapshot").require(ctx.need_priv("/vms", "VM.Snapshot.Rollback")), item('X', "Delete snapshot")],
                        ));
                    }
                }
                return;
            }
            t if t.starts_with("snap:") => {
                let name = t[5..].to_string();
                let g = self.snap_guest.clone();
                if key == 'R' {
                    ctx.popup(Popup::confirm_typed("Rollback", &format!("rollback:{name}"), vec![format!("Roll back {g} to snapshot {name}?"), "Current state is lost.".into()], &name));
                } else {
                    ctx.popup(Popup::confirm("Delete snapshot", &format!("delsnap:{name}"), vec![format!("Delete snapshot {name} of {g}?")], true));
                }
                return;
            }
            "migrate_mode" => return,
            _ => {}
        }
        let Some(g) = self.sel().cloned() else {
            match key {
                'N' | 'O' => self.start_create(ctx, key == 'N'),
                _ => {}
            }
            return;
        };
        let id = js(&g, "vmid");
        let name = js(&g, "name");
        let node = js(&g, "node");
        let is_vm = js(&g, "type") == "qemu";
        match key {
            's' => self.power(ctx, "start", "Start"),
            'd' => {
                let t = self.targets();
                ctx.popup(Popup::confirm("Shutdown", "shutdown", t.iter().map(|g| format!("{} {}", js(g, "vmid"), js(g, "name"))).collect(), false));
            }
            'D' => {
                let t = self.targets();
                let mut body: Vec<String> = t.iter().map(|g| format!("{} {}", js(g, "vmid"), js(g, "name"))).collect();
                body.push("Hard stop: like pulling the power cord.".into());
                ctx.popup(Popup::confirm("Stop (force)", "stop", body, true));
            }
            't' => {
                let t = self.targets();
                ctx.popup(Popup::confirm("Reboot", "reboot", t.iter().map(|g| format!("{} {}", js(g, "vmid"), js(g, "name"))).collect(), false));
            }
            'T' => ctx.popup(Popup::confirm("Reset (hard)", "reset", vec![format!("Hard reset {id} {name}?")], true)),
            'p' => self.power(ctx, "suspend", "Suspend"),
            'u' => self.power(ctx, "resume", "Resume"),
            'c' => {
                let Some(exec) = ctx.exec_for(&node) else { return };
                let (prog, args, banner) = if is_vm {
                    ("qm", vec!["terminal".to_string(), id.clone()], format!("Serial console of VM {id} {name}. Press Ctrl+O to exit.\n"))
                } else {
                    ("pct", vec!["enter".to_string(), id.clone()], format!("Shell inside container {id} {name}. Type 'exit' to return.\n"))
                };
                ctx.audit(&format!("console {id}"));
                ctx.cmds.push(Cmd::Suspend(Suspend { exec, prog: prog.into(), args, env: vec![], banner, logout_after: false, pause_after: false }));
            }
            'v' => ctx.cmds.push(Cmd::Vnc(VncRequest { node, vmid: id.parse().unwrap_or(0), name })),
            'n' => {
                let api = ctx.api.clone();
                let base = Self::base(&g);
                self.snaps = Some((format!("{base}"), Job::spawn(move || Ok(jarr(&api.get(&format!("{base}/snapshot"))?)))));
            }
            'b' => {
                let api = ctx.api.clone();
                let n2 = node.clone();
                let opts: Vec<(String, String)> = ctx
                    .api
                    .get(&format!("/nodes/{n2}/storage?content=backup"))
                    .map(|v| jarr(&v).iter().map(|s| (js(s, "storage"), format!("{} ({} free)", js(s, "storage"), fmt_bytes(jf(s, "avail"))))).collect())
                    .unwrap_or_default();
                let _ = api;
                if opts.is_empty() {
                    ctx.popup(Popup::error("Backup", "no storage with content 'backup' on this node"));
                    return;
                }
                ctx.popup(Popup::Form(Form::new(
                    &format!("{} {id}", tr("Backup now")),
                    "backup",
                    vec![
                        form::select("storage", "Storage", opts, ""),
                        form::select("mode", "Mode", form::opts(&["snapshot", "suspend", "stop"]), "snapshot"),
                        form::select("compress", "Compression", form::opts(&["zstd", "lzo", "gzip", "0"]), "zstd"),
                        form::text("notes-template", "Notes", "{{guestname}}"),
                        form::check("protected", "Protected", false),
                    ],
                )));
            }
            'M' => {
                let running = js(&g, "status") == "running";
                let my_arch = self.arch_of(&g, ctx).0;
                let opts: Vec<(String, String)> = ctx
                    .nodes
                    .iter()
                    .filter(|n| n.name != node && n.online)
                    .map(|n| {
                        let a = ctx.node_arch.get(&n.name).cloned().unwrap_or_default();
                        let warn = if !a.is_empty() && !my_arch.is_empty() && a != my_arch && !self.arch_of(&g, ctx).1 { format!(" ({a}: different arch!)") } else { format!(" ({a})") };
                        (n.name.clone(), format!("{}{warn}", n.name))
                    })
                    .collect();
                if opts.is_empty() {
                    ctx.popup(Popup::error("Migrate", "no other online node"));
                    return;
                }
                let mut fields = vec![form::info("Guest", &format!("{id} {name} on {node}")), form::select("target", "Target node", opts, "")];
                if is_vm {
                    fields.push(form::check("online", "Online (live)", running));
                    fields.push(form::check("with-local-disks", "With local disks", true));
                } else {
                    fields.push(form::check("restart", "Restart mode", running));
                }
                ctx.popup(Popup::Form(Form::new(&format!("{} {id}", tr("Migrate")), "migrate", fields)));
            }
            'C' => {
                let next = ctx.api.get("/cluster/nextid").ok().and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_u64().map(|n| n.to_string()))).unwrap_or_default();
                let nodes: Vec<(String, String)> = ctx.nodes.iter().filter(|n| n.online).map(|n| (n.name.clone(), n.name.clone())).collect();
                ctx.popup(Popup::Form(Form::new(
                    &format!("{} {id}", tr("Clone")),
                    "clone",
                    vec![
                        form::number("newid", "New ID", &next),
                        form::text("name", "Name", &format!("{name}-clone")),
                        form::select("target", "Target node", nodes, &node),
                        form::check("full", "Full clone", true),
                        form::text("storage", "Target storage", "").help("empty = same as source"),
                    ],
                )));
            }
            'e' => {
                let c = self.confs.get(&gkey(&g)).cloned().unwrap_or(Value::Null);
                let fields = if is_vm {
                    vec![
                        form::text("name", "Name", &js(&c, "name")),
                        form::number("sockets", "Sockets", &jsd(&c, "sockets", "1")),
                        form::number("cores", "Cores", &jsd(&c, "cores", "1")),
                        form::number("memory", "Memory (MiB)", &jsd(&c, "memory", "512")),
                        form::check("onboot", "Start at boot", jb(&c, "onboot")),
                        form::check("agent", "QEMU guest agent", js(&c, "agent").starts_with('1')),
                        form::text("description", "Notes", js(&c, "description").lines().next().unwrap_or("")),
                    ]
                } else {
                    vec![
                        form::text("hostname", "Hostname", &js(&c, "hostname")),
                        form::number("cores", "Cores", &jsd(&c, "cores", "1")),
                        form::number("memory", "Memory (MiB)", &js(&c, "memory")),
                        form::number("swap", "Swap (MiB)", &js(&c, "swap")),
                        form::check("onboot", "Start at boot", jb(&c, "onboot")),
                        form::text("description", "Notes", js(&c, "description").lines().next().unwrap_or("")),
                    ]
                };
                ctx.popup(Popup::Form(Form::new(&format!("{} {id}", tr("Edit")), "edit", fields)));
            }
            'H' => {
                let sid = format!("{}:{id}", if is_vm { "vm" } else { "ct" });
                if js(&g, "hastate").is_empty() {
                    ctx.api_call(format!("Add {sid} to HA"), "POST", "/cluster/ha/resources".into(), vec![("sid", sid.clone()), ("state", "started".into())]);
                } else {
                    ctx.popup(Popup::confirm("HA", &format!("haremove:{sid}"), vec![format!("Remove {sid} from HA?")], false));
                }
            }
            'U' => {
                let base = Self::base(&g);
                ctx.api_call(format!("Unlock {id}"), "PUT", format!("{base}/config"), vec![("delete", "lock".into()), ("skiplock", "1".into())]);
            }
            'X' => ctx.popup(Popup::confirm_typed(
                "Delete guest",
                "destroy",
                vec![format!("Destroy {} {id} {name} on {node}?", if is_vm { "VM" } else { "CT" }), "All disks, snapshots, jobs and HA entries of this guest are removed.".into()],
                &id,
            )),
            'N' | 'O' => self.start_create(ctx, key == 'N'),
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        match tag {
            "shutdown" => self.power(ctx, "shutdown", "Shutdown"),
            "stop" => self.power(ctx, "stop", "Stop"),
            "reboot" => self.power(ctx, "reboot", "Reboot"),
            "reset" => self.power(ctx, "reset", "Reset"),
            "destroy" => {
                if let Some(g) = self.sel().cloned() {
                    let base = Self::base(&g);
                    let mut p = vec![("purge", "1".to_string())];
                    if js(&g, "type") == "qemu" {
                        p.push(("destroy-unreferenced-disks", "1".into()));
                    }
                    ctx.api_call(format!("Destroy {}", js(&g, "vmid")), "DELETE", base, p);
                }
            }
            t if t.starts_with("haremove:") => {
                let sid = t[9..].to_string();
                ctx.api_call(format!("Remove {sid} from HA"), "DELETE", format!("/cluster/ha/resources/{sid}"), vec![]);
            }
            t if t.starts_with("rollback:") => {
                let base = self.snap_guest.clone();
                ctx.api_call(format!("Rollback {}", &t[9..]), "POST", format!("{base}/snapshot/{}/rollback", &t[9..]), vec![]);
            }
            t if t.starts_with("delsnap:") => {
                let base = self.snap_guest.clone();
                ctx.api_call(format!("Delete snapshot {}", &t[8..]), "DELETE", format!("{base}/snapshot/{}", &t[8..]), vec![]);
            }
            _ => {}
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        if tag == "create_vm" || tag == "create_ct" {
            pve_create::submit(tag, v, ctx);
            return;
        }
        if tag == "snap" {
            let base = self.snap_guest.clone();
            let mut p = vec![("snapname", fv(v, "snapname").to_string())];
            if !fv(v, "description").is_empty() {
                p.push(("description", fv(v, "description").to_string()));
            }
            if fv(v, "vmstate") == "1" {
                p.push(("vmstate", "1".into()));
            }
            ctx.api_call(format!("Snapshot {}", fv(v, "snapname")), "POST", format!("{base}/snapshot"), p);
            return;
        }
        let Some(g) = self.sel().cloned() else { return };
        let id = js(&g, "vmid");
        let node = js(&g, "node");
        let base = Self::base(&g);
        let is_vm = js(&g, "type") == "qemu";
        match tag {
            "backup" => {
                let mut p = vec![
                    ("vmid", id.clone()),
                    ("storage", fv(v, "storage").to_string()),
                    ("mode", fv(v, "mode").to_string()),
                    ("compress", fv(v, "compress").to_string()),
                ];
                if !fv(v, "notes-template").is_empty() {
                    p.push(("notes-template", fv(v, "notes-template").to_string()));
                }
                if fv(v, "protected") == "1" {
                    p.push(("protected", "1".into()));
                }
                ctx.api_call(format!("Backup {id}"), "POST", format!("/nodes/{node}/vzdump"), p);
            }
            "migrate" => {
                let mut p = vec![("target", fv(v, "target").to_string())];
                for k in ["online", "with-local-disks", "restart"] {
                    if fv(v, k) == "1" {
                        p.push((k, "1".into()));
                    }
                }
                ctx.api_call(format!("Migrate {id} -> {}", fv(v, "target")), "POST", format!("{base}/migrate"), p);
            }
            "clone" => {
                let mut p = vec![("newid", fv(v, "newid").to_string()), ("target", fv(v, "target").to_string())];
                if !fv(v, "name").is_empty() {
                    p.push((if is_vm { "name" } else { "hostname" }, fv(v, "name").to_string()));
                }
                if fv(v, "full") == "1" {
                    p.push(("full", "1".into()));
                }
                if !fv(v, "storage").is_empty() {
                    p.push(("storage", fv(v, "storage").to_string()));
                }
                ctx.api_call(format!("Clone {id} -> {}", fv(v, "newid")), "POST", format!("{base}/clone"), p);
            }
            "edit" => {
                let mut p: Vec<(&'static str, String)> = Vec::new();
                let mut del = Vec::new();
                let keys: &[&'static str] = if is_vm { &["name", "sockets", "cores", "memory", "onboot", "agent", "description"] } else { &["hostname", "cores", "memory", "swap", "onboot", "description"] };
                for k in keys {
                    let val = fv(v, k);
                    if val.is_empty() {
                        del.push(*k);
                    } else {
                        p.push((k, val.to_string()));
                    }
                }
                if !del.is_empty() {
                    p.push(("delete", del.join(",")));
                }
                ctx.api_call(format!("Edit {id}"), "PUT", format!("{base}/config"), p);
                self.confs.remove(&gkey(&g));
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.detail.clear();
    }
}

impl Guests {
    fn start_create(&mut self, ctx: &mut Ctx, vm: bool) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let arch = ctx.node_arch.get(&node).cloned().unwrap_or_default();
        self.nodes_for_form = ctx.nodes.iter().map(|n| n.name.clone()).collect();
        ctx.toast(format!("{}...", tr(if vm { "Create VM" } else { "Create CT" })));
        self.create = Some((vm, Job::spawn(move || pve_create::load_info(&api, &node, &arch))));
    }
}

// ===========================================================================
// Storage
// ===========================================================================

#[derive(Default)]
struct Storage {
    data: Loader<Vec<Value>>,
    table: Table,
    browsing: Option<String>,
    content: Loader<Vec<Value>>,
    ctable: Table,
    templates: Option<Job<Vec<Value>>>,
    tmpl_list: Vec<Value>,
}

impl Storage {
    fn sel(&self) -> Option<&Value> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.iter().find(|s| js(s, "storage") == k)
    }

    fn load_content(&mut self, ctx: &Ctx) {
        if let Some(s) = self.browsing.clone() {
            let api = ctx.api.clone();
            let node = ctx.node.clone();
            self.content.load(move || {
                let mut v = jarr(&api.get(&format!("/nodes/{node}/storage/{s}/content"))?);
                v.sort_by_key(|c| (js(c, "content"), js(c, "volid")));
                Ok(v)
            });
        }
    }
}

impl View for Storage {
    fn title(&self) -> &'static str {
        "Storage"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.data.load(move || {
            let mut v = jarr(&api.get(&format!("/nodes/{node}/storage"))?);
            v.sort_by_key(|s| js(s, "storage"));
            Ok(v)
        });
        self.load_content(ctx);
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let ch = self.data.poll() | self.content.poll();
        if let Some(j) = &self.templates {
            if let Some(r) = j.poll() {
                self.templates = None;
                match r {
                    Ok(list) => {
                        self.tmpl_list = list;
                        let stores: Vec<(String, String)> = self
                            .data
                            .data
                            .iter()
                            .flatten()
                            .filter(|s| js(s, "content").contains("vztmpl"))
                            .map(|s| (js(s, "storage"), js(s, "storage")))
                            .collect();
                        let opts: Vec<(String, String)> = self
                            .tmpl_list
                            .iter()
                            .map(|t| (js(t, "template"), format!("{} {} - {}", js(t, "os"), js(t, "version"), js(t, "headline"))))
                            .collect();
                        if stores.is_empty() || opts.is_empty() {
                            ctx.popup(Popup::error("Templates", "no template storage or no templates available (run 'pveam update')"));
                        } else {
                            ctx.popup(Popup::Form(Form::new(
                                "Download CT template",
                                "tmpl",
                                vec![form::select("storage", "Storage", stores, ""), form::select("template", "Template", opts, "")],
                            )));
                        }
                    }
                    Err(e) => ctx.popup(Popup::error("Templates", &format!("{e:#}"))),
                }
            }
        }
        ch
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(15)
    }

    fn table(&mut self) -> Option<&mut Table> {
        if self.browsing.is_some() {
            Some(&mut self.ctable)
        } else {
            Some(&mut self.table)
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.browsing.is_some() {
            vec![("Esc", "back")]
        } else {
            vec![]
        }
    }

    fn on_key(&mut self, key: &KeyEvent, ctx: &mut Ctx) -> bool {
        match key.code {
            KeyCode::Esc if self.browsing.is_some() => {
                self.browsing = None;
                self.content.clear();
                true
            }
            KeyCode::Char('b') if self.browsing.is_none() => {
                if let Some(s) = self.sel().map(|s| js(s, "storage")) {
                    self.browsing = Some(s);
                    self.content.clear();
                    self.ctable = Table::default();
                    self.load_content(ctx);
                }
                true
            }
            _ => false,
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        if let Some(s) = &self.browsing {
            let rows: Vec<Row> = self
                .content
                .data
                .iter()
                .flatten()
                .map(|c| {
                    Row::new(js(c, "volid"))
                        .c(js(c, "volid").split_once(':').map(|x| x.1.to_string()).unwrap_or_default())
                        .c(js(c, "content"))
                        .c(js(c, "format"))
                        .c(fmt_bytes(jf(c, "size")))
                        .c(js(c, "vmid"))
                        .c(if ji(c, "ctime") > 0 { fmt_time(ji(c, "ctime")) } else { String::new() })
                        .cs(if jb(c, "protected") { "yes" } else { "" }, style_warn())
                })
                .collect();
            let cols = [
                col("Volume", Constraint::Min(20)),
                col("Content", Constraint::Length(8)),
                col("Format", Constraint::Length(7)),
                col("Size", Constraint::Length(8)),
                col("VMID", Constraint::Length(5)),
                col("Created", Constraint::Length(19)),
                col("Prot.", Constraint::Length(5)),
            ];
            let title = format!("{} {s} (Esc = back)", tr("Storage"));
            self.ctable.draw(f, area, &title, &cols, &rows, focused, loading_or_error(&self.content.error, self.content.loading()));
            return;
        }
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flatten()
            .map(|s| {
                let active = jb(s, "active");
                let en = jb(s, "enabled");
                let frac = jf(s, "used_fraction");
                Row::new(js(s, "storage"))
                    .c(js(s, "storage"))
                    .c(js(s, "type"))
                    .c(js(s, "content"))
                    .c(if jb(s, "shared") { "yes" } else { "" })
                    .cs(if !en { "disabled" } else if active { "active" } else { "INACTIVE" }, if !en { style_dim() } else if active { style_good() } else { style_bad() })
                    .cs(format!("{} {:.0}%", bar(frac, 10), frac * 100.0), usage_style(frac))
                    .c(format!("{} / {}", fmt_bytes(jf(s, "used")), fmt_bytes(jf(s, "total"))))
                    .dim(!en)
            })
            .collect();
        let cols = [
            col("Storage", Constraint::Length(14)),
            col("Type", Constraint::Length(8)),
            col("Content", Constraint::Min(14)),
            col("Shared", Constraint::Length(6)),
            col("Status", Constraint::Length(8)),
            col("Usage", Constraint::Length(16)),
            col("Used / Total", Constraint::Length(16)),
        ];
        self.table.draw(f, area, "Storage", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if self.browsing.is_some() {
            if let Some(c) = self.ctable.selected().and_then(|k| self.content.data.as_ref()?.iter().find(|c| js(c, "volid") == k)) {
                if let Value::Object(m) = c {
                    for (k, v) in m {
                        let val = if k == "size" || k == "used" { fmt_bytes(v.as_f64().unwrap_or(0.0)) } else if k == "ctime" { fmt_time(v.as_i64().unwrap_or(0)) } else { js(c, k) };
                        kv.add(k, val);
                    }
                }
            }
        } else if let Some(s) = self.sel() {
            if let Value::Object(m) = s {
                for (k, _) in m {
                    let val = if ["used", "total", "avail"].contains(&k.as_str()) { fmt_bytes(jf(s, k)) } else { js(s, k) };
                    kv.add(k, val);
                }
            }
            kv.blank();
            kv.styled("", "b: browse content", style_dim());
        }
        draw_lines(f, area, "Details", focused, kv.lines(), 0);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        if let Some(s) = &self.browsing {
            let path = format!("/storage/{s}");
            let sel = self.ctable.selected().is_some();
            return vec![item('x', "Delete volume").require(ctx.need_priv(&path, "Datastore.Allocate")).disabled_if(!sel, "nothing selected")];
        }
        let Some(s) = self.sel() else { return vec![] };
        let path = format!("/storage/{}", js(s, "storage"));
        let content = js(s, "content");
        let en = jb(s, "enabled");
        vec![
            item('b', "Browse content"),
            item('w', "Download ISO from URL").require(ctx.need_priv(&path, "Datastore.AllocateTemplate")).disabled_if(!content.contains("iso"), "no 'iso' content"),
            item('T', "Download CT template").require(ctx.need_priv(&path, "Datastore.AllocateTemplate")).disabled_if(!content.contains("vztmpl"), "no 'vztmpl' content"),
            item(if en { 'E' } else { 'e' }, if en { "Disable storage" } else { "Enable storage" }).require(ctx.need_priv(&path, "Datastore.Allocate")),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        if let Some(s) = self.browsing.clone() {
            if key == 'x' {
                if let Some(vol) = self.ctable.selected().map(|s| s.to_string()) {
                    ctx.popup(Popup::confirm_typed("Delete volume", &format!("delvol:{s}:{vol}"), vec![format!("Delete {vol}?")], vol.rsplit('/').next().unwrap_or(&vol)));
                }
            }
            return;
        }
        let Some(s) = self.sel().map(|s| js(s, "storage")) else { return };
        match key {
            'b' => {
                self.browsing = Some(s);
                self.content.clear();
                self.ctable = Table::default();
                self.load_content(ctx);
            }
            'w' => ctx.popup(Popup::Form(Form::new(
                "Download ISO",
                "iso",
                vec![
                    form::info("Storage", &s),
                    form::text("url", "URL", "https://").req(true),
                    form::text("filename", "File name", "").help("empty = last part of the URL"),
                    form::text("checksum", "SHA256 (optional)", ""),
                ],
            ))),
            'T' => {
                let api = ctx.api.clone();
                let node = ctx.node.clone();
                self.templates = Some(Job::spawn(move || Ok(jarr(&api.get(&format!("/nodes/{node}/aplinfo"))?))));
            }
            'e' | 'E' => ctx.api_call(format!("{} {s}", if key == 'e' { "Enable" } else { "Disable" }), "PUT", format!("/storage/{s}"), vec![("disable", if key == 'e' { "0" } else { "1" }.into())]),
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if let Some(rest) = tag.strip_prefix("delvol:") {
            if let Some((s, vol)) = rest.split_once(':') {
                let n = ctx.node.clone();
                ctx.api_call(format!("Delete {vol}"), "DELETE", format!("/nodes/{n}/storage/{s}/content/{}", urlenc(vol)), vec![]);
            }
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let n = ctx.node.clone();
        match tag {
            "iso" => {
                let Some(s) = self.sel().map(|s| js(s, "storage")) else { return };
                let url = fv(v, "url").to_string();
                let mut filename = fv(v, "filename").to_string();
                if filename.is_empty() {
                    filename = url.rsplit('/').next().unwrap_or("download.iso").split('?').next().unwrap_or("").to_string();
                }
                let mut p = vec![("content", "iso".to_string()), ("url", url), ("filename", filename.clone())];
                if !fv(v, "checksum").is_empty() {
                    p.push(("checksum", fv(v, "checksum").to_string()));
                    p.push(("checksum-algorithm", "sha256".into()));
                }
                ctx.api_call(format!("Download {filename}"), "POST", format!("/nodes/{n}/storage/{s}/download-url"), p);
            }
            "tmpl" => ctx.api_call(
                format!("Download {}", fv(v, "template")),
                "POST",
                format!("/nodes/{n}/aplinfo"),
                vec![("storage", fv(v, "storage").to_string()), ("template", fv(v, "template").to_string())],
            ),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.data.clear();
        self.browsing = None;
        self.content.clear();
    }
}

// ===========================================================================
// Backup jobs
// ===========================================================================

#[derive(Default)]
struct Backup {
    data: Loader<Vec<Value>>,
    table: Table,
}

impl Backup {
    fn sel(&self) -> Option<&Value> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.iter().find(|j| js(j, "id") == k)
    }
}

impl View for Backup {
    fn title(&self) -> &'static str {
        "Backup"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        self.data.load(move || Ok(jarr(&api.get("/cluster/backup")?)));
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
            .flatten()
            .map(|j| {
                let en = j.get("enabled").map(|_| jb(j, "enabled")).unwrap_or(true);
                let sel = if jb(j, "all") { "all".to_string() } else if !js(j, "pool").is_empty() { format!("pool {}", js(j, "pool")) } else { js(j, "vmid") };
                Row::new(js(j, "id"))
                    .c(js(j, "id"))
                    .cs(if en { "yes" } else { "no" }, if en { style_good() } else { style_dim() })
                    .c(js(j, "schedule"))
                    .c(js(j, "storage"))
                    .c(sel)
                    .c(js(j, "mode"))
                    .c(if ji(j, "next-run") > 0 { fmt_time(ji(j, "next-run")) } else { String::new() })
                    .c(js(j, "comment"))
                    .dim(!en)
            })
            .collect();
        let cols = [
            col("ID", Constraint::Length(16)),
            col("On", Constraint::Length(3)),
            col("Schedule", Constraint::Length(14)),
            col("Storage", Constraint::Length(12)),
            col("Guests", Constraint::Length(14)),
            col("Mode", Constraint::Length(8)),
            col("Next run", Constraint::Length(19)),
            col("Comment", Constraint::Min(6)),
        ];
        self.table.draw(f, area, "Backup jobs", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(Value::Object(m)) = self.sel() {
            for (k, v) in m {
                kv.add(k, match v {
                    Value::String(s) => s.clone(),
                    o => o.to_string(),
                });
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), 0);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(j) = self.sel() else { return vec![] };
        let en = j.get("enabled").map(|_| jb(j, "enabled")).unwrap_or(true);
        let m = ctx.need_priv("/", "Sys.Modify");
        vec![
            item('R', "Run now").require(ctx.need_priv("/vms", "VM.Backup")),
            item(if en { 'E' } else { 'e' }, if en { "Disable job" } else { "Enable job" }).require(m),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(j) = self.sel().cloned() else { return };
        let id = js(&j, "id");
        match key {
            'R' => ctx.popup(Popup::confirm("Run backup job", "run", vec![format!("Run backup job {id} now on {}?", ctx.node)], false)),
            'e' | 'E' => ctx.api_call(format!("Backup job {id}"), "PUT", format!("/cluster/backup/{}", urlenc(&id)), vec![("enabled", if key == 'e' { "1" } else { "0" }.into())]),
            _ => {}
        }
    }

    fn on_confirm(&mut self, _tag: &str, ctx: &mut Ctx) {
        let Some(j) = self.sel().cloned() else { return };
        let mut params: Vec<(&'static str, String)> = Vec::new();
        const KEYS: [&str; 14] = ["vmid", "all", "pool", "exclude", "storage", "mode", "compress", "mailto", "mailnotification", "notes-template", "prune-backups", "protected", "bwlimit", "zstd"];
        for k in KEYS {
            let v = js(&j, k);
            if !v.is_empty() {
                params.push((k, v));
            }
        }
        let n = if js(&j, "node").is_empty() { ctx.node.clone() } else { js(&j, "node") };
        ctx.api_call(format!("Backup job {}", js(&j, "id")), "POST", format!("/nodes/{n}/vzdump"), params);
    }
}

// ===========================================================================
// HA
// ===========================================================================

#[derive(Default)]
struct HaData {
    status: Vec<Value>,
    resources: Vec<Value>,
}

#[derive(Default)]
struct Ha {
    data: Loader<HaData>,
    table: Table,
}

impl View for Ha {
    fn title(&self) -> &'static str {
        "HA"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        self.data.load(move || {
            Ok(HaData { status: jarr(&api.get("/cluster/ha/status/current")?), resources: jarr(&api.get("/cluster/ha/resources").unwrap_or(Value::Null)) })
        });
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(10)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flat_map(|d| d.resources.iter().map(move |r| (d, r)))
            .map(|(d, r)| {
                let sid = js(r, "sid");
                let st = d.status.iter().find(|s| js(s, "sid") == sid);
                let crm = st.map(|s| js(s, "crm_state")).unwrap_or_default();
                Row::new(sid.clone())
                    .c(sid)
                    .c(js(r, "state"))
                    .cs(crm.clone(), status_style(if crm == "started" { "ok" } else if crm.contains("error") || crm.contains("fence") { "error" } else { "x" }))
                    .c(st.map(|s| js(s, "node")).unwrap_or_default())
                    .c(js(r, "group"))
                    .c(format!("{}/{}", js(r, "max_restart"), js(r, "max_relocate")))
                    .c(js(r, "comment"))
            })
            .collect();
        let cols = [
            col("Resource", Constraint::Length(10)),
            col("Request", Constraint::Length(9)),
            col("State", Constraint::Length(10)),
            col("Node", Constraint::Length(12)),
            col("Group", Constraint::Length(10)),
            col("Restart/Reloc", Constraint::Length(13)),
            col("Comment", Constraint::Min(6)),
        ];
        self.table.draw(f, area, "HA resources", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(d) = &self.data.data {
            for s in d.status.iter().filter(|s| js(s, "type") != "service") {
                let st = js(s, "status");
                kv.styled(
                    &format!("{} {}", js(s, "type"), js(s, "node")),
                    st.clone(),
                    if st.contains("error") || st.contains("fence") || st.contains("dead") { style_bad() } else if st.starts_with("OK") || st.contains("active") { style_good() } else { Style::default() },
                );
            }
        }
        draw_lines(f, area, "HA status", focused, kv.lines(), 0);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let m = ctx.need_priv("/", "Sys.Console");
        let sel = self.table.selected().is_some();
        vec![
            item('s', "Request state: started").require(m.clone()).disabled_if(!sel, "nothing selected"),
            item('x', "Request state: stopped").require(m.clone()).disabled_if(!sel, "nothing selected"),
            item('d', "Request state: disabled").require(m.clone()).disabled_if(!sel, "nothing selected"),
            item('X', "Remove from HA").require(m).disabled_if(!sel, "nothing selected"),
            item('M', "Node maintenance mode on").require(ctx.need_root()).require(ctx.need_exec("Sys.Modify").map(|_| ())),
            item('m', "Node maintenance mode off").require(ctx.need_root()).require(ctx.need_exec("Sys.Modify").map(|_| ())),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let sid = self.table.selected().unwrap_or("").to_string();
        match key {
            's' | 'x' | 'd' => {
                let state = match key {
                    's' => "started",
                    'x' => "stopped",
                    _ => "disabled",
                };
                ctx.api_call(format!("HA {sid} -> {state}"), "PUT", format!("/cluster/ha/resources/{sid}"), vec![("state", state.into())]);
            }
            'X' => ctx.popup(Popup::confirm("Remove from HA", &format!("rm:{sid}"), vec![format!("Remove {sid} from HA management?")], false)),
            'M' | 'm' => {
                if let Some(e) = ctx.exec() {
                    let a = if key == 'M' { "enable" } else { "disable" };
                    let node = ctx.node.clone();
                    ctx.exec_call(format!("maintenance {a} {node}"), e, "ha-manager", vec!["crm-command".into(), "node-maintenance".into(), a.into(), node]);
                }
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if let Some(sid) = tag.strip_prefix("rm:") {
            ctx.api_call(format!("Remove {sid} from HA"), "DELETE", format!("/cluster/ha/resources/{sid}"), vec![]);
        }
    }
}

// ===========================================================================
// Replication
// ===========================================================================

#[derive(Default)]
struct Replication {
    data: Loader<(Vec<Value>, Vec<Value>)>,
    table: Table,
}

impl View for Replication {
    fn title(&self) -> &'static str {
        "Replication"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.data.load(move || Ok((jarr(&api.get("/cluster/replication")?), jarr(&api.get(&format!("/nodes/{node}/replication")).unwrap_or(Value::Null)))));
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

    fn has_detail(&self) -> bool {
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flat_map(|(jobs, st)| jobs.iter().map(move |j| (j, st)))
            .map(|(j, st)| {
                let id = js(j, "id");
                let s = st.iter().find(|s| js(s, "id") == id);
                let err = s.map(|s| js(s, "error")).unwrap_or_default();
                let fails = s.map(|s| ji(s, "fail_count")).unwrap_or(0);
                Row::new(id.clone())
                    .c(id)
                    .c(js(j, "guest"))
                    .c(js(j, "target"))
                    .c(js(j, "schedule"))
                    .c(s.map(|s| if ji(s, "last_sync") > 0 { fmt_time(ji(s, "last_sync")) } else { "-".into() }).unwrap_or_default())
                    .c(s.map(|s| if ji(s, "next_sync") > 0 { fmt_time(ji(s, "next_sync")) } else { "-".into() }).unwrap_or_default())
                    .cs(if err.is_empty() { "OK".into() } else { format!("{fails}x {err}") }, if err.is_empty() { style_good() } else { style_bad() })
                    .dim(jb(j, "disable"))
            })
            .collect();
        let cols = [
            col("ID", Constraint::Length(8)),
            col("Guest", Constraint::Length(6)),
            col("Target", Constraint::Length(12)),
            col("Schedule", Constraint::Length(10)),
            col("Last sync", Constraint::Length(19)),
            col("Next sync", Constraint::Length(19)),
            col("State", Constraint::Min(8)),
        ];
        self.table.draw(f, area, "Replication", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(id) = self.table.selected() else { return vec![] };
        let dis = self.data.data.as_ref().and_then(|(j, _)| j.iter().find(|x| js(x, "id") == id)).map(|j| jb(j, "disable")).unwrap_or(false);
        vec![
            item('R', "Schedule now").require(ctx.need_priv("/", "Sys.Modify")),
            item(if dis { 'e' } else { 'E' }, if dis { "Enable job" } else { "Disable job" }).require(ctx.need_priv("/", "Sys.Modify")),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(id) = self.table.selected().map(|s| s.to_string()) else { return };
        let n = ctx.node.clone();
        match key {
            'R' => ctx.api_call(format!("Replicate {id}"), "POST", format!("/nodes/{n}/replication/{id}/schedule_now"), vec![]),
            'e' | 'E' => ctx.api_call(format!("Replication {id}"), "PUT", format!("/cluster/replication/{id}"), vec![("disable", if key == 'E' { "1" } else { "0" }.into())]),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}
