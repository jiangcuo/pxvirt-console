//! Disks module: physical disks + SMART, partitions/mounts, LVM, ZFS, Ceph.

use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::Line,
    Frame,
};
use serde_json::Value;
use std::collections::HashMap;

use super::{fv, scroll, View};
use crate::ctx::{ActionOutcome, Ctx};
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::form::{self, Form};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, draw_message_in, loading_or_error, status_style, style_bad, style_dim, style_good, style_warn, usage_style, Kv, ACCENT};
use crate::util::{bar, fmt_bytes, jarr, jb, jf, ji, js, urlenc};

pub fn views() -> Vec<Box<dyn View>> {
    vec![
        Box::new(Physical::default()),
        Box::new(Partitions::default()),
        Box::new(Lvm::default()),
        Box::new(Zfs::default()),
        Box::new(Ceph::default()),
        Box::new(super::diskio::DiskIo::default()),
        Box::new(super::netstorage::NetStorage::default()),
        Box::new(super::raid::HwRaid::default()),
    ]
}

#[cfg(test)]
mod zfs_tests {
    #[test]
    fn zpool_iostat_last_sample() {
        let out = "rpool\t100\t200\t1\t2\t1000\t2000\t500000\t800000\t1\t1\t-\t-\t-\t-\t-\t-\nrpool\t100\t200\t10\t20\t40960\t81920\t2000000\t4000000\t1\t1\t-\t-\t-\t-\t-\t-\n";
        let m = super::parse_zpool_iostat(out);
        assert_eq!(m["rpool"], [10.0, 20.0, 40960.0, 81920.0, 2.0, 4.0]);
    }
}

fn not_configured(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("not installed") || e.contains("not initialized") || e.contains("no such file") || e.contains("failed: exit code 1") || e.contains("rados_connect failed") || e.contains("got timeout")
}

// ===========================================================================
// Physical disks
// ===========================================================================

#[derive(Default)]
struct Physical {
    data: Loader<Vec<Value>>,
    table: Table,
    smart: HashMap<String, Loader<Value>>,
    scroll: u16,
}

impl Physical {
    fn sel(&self) -> Option<&Value> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.iter().find(|d| js(d, "devpath") == k)
    }

    fn selected_devs(&self) -> Vec<String> {
        self.table.targets()
    }
}

impl View for Physical {
    fn title(&self) -> &'static str {
        "Physical"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.data.load(move || Ok(jarr(&api.get(&format!("/nodes/{node}/disks/list"))?)));
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let mut ch = self.data.poll();
        for l in self.smart.values_mut() {
            ch |= l.poll();
        }
        // lazily load SMART for the selected disk
        if let Some(dev) = self.table.selected().map(|s| s.to_string()) {
            let l = self.smart.entry(dev.clone()).or_default();
            if l.stale(std::time::Duration::from_secs(300)) && l.error.is_none() {
                let api = ctx.api.clone();
                let node = ctx.node.clone();
                l.load(move || api.get(&format!("/nodes/{node}/disks/smart?disk={}", urlenc(&dev))));
            }
        }
        ch
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(60)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let m = ctx.metrics.lock().unwrap().clone();
        if !self.table.multi {
            self.table.multi = true;
        }
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flatten()
            .map(|d| {
                let health = js(d, "health");
                let wear = js(d, "wearout");
                let wear_s = if wear.is_empty() || wear == "N/A" { "-".into() } else { format!("{wear}%") };
                let wear_st = match wear.parse::<i64>() {
                    Ok(w) if w < 10 => style_bad(),
                    Ok(w) if w < 30 => style_warn(),
                    _ => Style::default(),
                };
                let used = js(d, "used");
                let row = Row::new(js(d, "devpath"))
                    .c(js(d, "devpath"))
                    .c(js(d, "type"))
                    .c(fmt_bytes(jf(d, "size")))
                    .c(js(d, "model"))
                    .cs(health.clone(), status_style(&health));
                super::diskio::cells(row, m.disk(&js(d, "devpath")))
                    .c(js(d, "serial"))
                    .cs(wear_s, wear_st)
                    .cs(if used.is_empty() { "unused".into() } else { used.clone() }, if used.is_empty() { style_good() } else { Style::default() })
            })
            .collect();
        let mut cols = vec![
            col("Device", Constraint::Length(14)),
            col("Type", Constraint::Length(5)),
            col("Size", Constraint::Length(7)),
            col("Model", Constraint::Min(14)),
            col("Health", Constraint::Length(7)),
        ];
        cols.extend(super::diskio::cols());
        cols.extend([col("Serial", Constraint::Length(16)), col("Wearout", Constraint::Length(7)), col("Usage", Constraint::Length(12))]);
        self.table.draw(f, area, "Physical disks", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        let io = self.sel().and_then(|d| ctx.metrics.lock().unwrap().disk(&js(d, "devpath")).cloned());
        if let Some(st) = &io {
            super::diskio::detail(&mut kv, st);
        }
        if let Some(d) = self.sel() {
            let dev = js(d, "devpath");
            kv.add("Device", dev.clone());
            kv.add("Model", js(d, "model"));
            kv.add("Vendor", js(d, "vendor"));
            kv.add("Serial", js(d, "serial"));
            kv.add("WWN", js(d, "wwn"));
            kv.add("by-id", js(d, "by_id_link"));
            kv.add("Size", format!("{} ({} bytes)", fmt_bytes(jf(d, "size")), js(d, "size")));
            kv.add("Type", format!("{}{}", js(d, "type"), if ji(d, "rpm") > 0 { format!(" {} rpm", js(d, "rpm")) } else { String::new() }));
            kv.add("GPT", if jb(d, "gpt") { "yes" } else { "no" });
            kv.add("Used by", js(d, "used"));
            if ji(d, "osdid") >= 0 {
                kv.add("Ceph OSD", js(d, "osdid"));
            }
            kv.section("SMART");
            match self.smart.get(&dev) {
                Some(l) if l.data.is_some() => {
                    let s = l.data.as_ref().unwrap();
                    kv.status("Health", js(s, "health"));
                    let text = js(s, "text");
                    if !text.is_empty() {
                        for line in text.lines().filter(|l| l.contains(':')).take(40) {
                            if let Some((k, v)) = line.split_once(':') {
                                kv.add(k.trim(), v.trim());
                            }
                        }
                    }
                    for a in s.get("attributes").map(jarr).unwrap_or_default() {
                        let fail = js(&a, "fail");
                        kv.styled(
                            &format!("{} {}", js(&a, "id"), js(&a, "name")),
                            format!("{} (raw {}){}", js(&a, "value"), js(&a, "raw"), if fail != "-" && !fail.is_empty() { format!(" FAIL {fail}") } else { String::new() }),
                            if fail != "-" && !fail.is_empty() { style_bad() } else { Style::default() },
                        );
                    }
                }
                Some(l) if l.error.is_some() => {
                    kv.styled("", l.error.clone().unwrap_or_default(), style_dim());
                }
                _ => {
                    kv.styled("", tr("Loading..."), style_dim());
                }
            }
        }
        let mut lines = kv.lines();
        if let Some(st) = &io {
            lines.extend(super::diskio::graphs(st, area.width as usize));
        }
        draw_lines(f, area, "Details", focused, lines, self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("Space", "mark")]
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let m = ctx.need_node_priv("Sys.Modify");
        let unused = self.sel().map(|d| js(d, "used").is_empty()).unwrap_or(false);
        vec![
            item('s', "Show full SMART report").require(ctx.need_node_priv("Sys.Audit")),
            item('P', "Initialize disk with GPT").require(m.clone()).disabled_if(!unused, "disk is in use"),
            item('W', "Wipe disk").require(m.clone()).disabled_if(self.sel().is_none(), "no disk"),
            item('z', "Create ZFS pool (marked disks)").require(m.clone()),
            item('t', "Create LVM-thin pool").require(m.clone()).disabled_if(!unused, "disk is in use"),
            item('v', "Create LVM volume group").require(m.clone()).disabled_if(!unused, "disk is in use"),
            item('d', "Create directory storage").require(m).disabled_if(!unused, "disk is in use"),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(dev) = self.sel().map(|d| js(d, "devpath")) else { return };
        let n = ctx.node.clone();
        match key {
            's' => {
                let api = ctx.api.clone();
                let d2 = dev.clone();
                ctx.act(format!("SMART {dev}"), move || {
                    let v = api.get(&format!("/nodes/{n}/disks/smart?disk={}", urlenc(&d2)))?;
                    let mut text = format!("health: {}\n\n", js(&v, "health"));
                    text.push_str(&js(&v, "text"));
                    for a in v.get("attributes").map(jarr).unwrap_or_default() {
                        text.push_str(&format!(
                            "{:>3} {:<28} value {:>4} worst {:>4} thresh {:>4} raw {}\n",
                            js(&a, "id"),
                            js(&a, "name"),
                            js(&a, "value"),
                            js(&a, "worst"),
                            js(&a, "threshold"),
                            js(&a, "raw")
                        ));
                    }
                    Ok(ActionOutcome::Text(format!("SMART {d2}"), text))
                });
            }
            'P' => ctx.popup(Popup::confirm("Initialize GPT", &format!("gpt:{dev}"), vec![format!("Write a new GPT partition table to {dev}?")], true)),
            'W' => ctx.popup(Popup::confirm_typed(
                "Wipe disk",
                &format!("wipe:{dev}"),
                vec![format!("ALL DATA on {dev} will be destroyed."), "Partitions, LVM, ZFS labels and signatures are removed.".into()],
                &dev,
            )),
            'z' => {
                let devs = self.selected_devs();
                ctx.popup(Popup::Form(Form::new(
                    "Create ZFS pool",
                    "zfs",
                    vec![
                        form::info("Devices", &devs.join(" ")),
                        form::text("name", "Name", "tank").req(true),
                        form::select("raidlevel", "RAID level", form::opts(&["single", "mirror", "raid10", "raidz", "raidz2", "raidz3", "draid", "draid2", "draid3"]), if devs.len() > 1 { "mirror" } else { "single" }),
                        form::select("compression", "Compression", form::opts(&["on", "off", "lz4", "zstd", "gzip"]), "on"),
                        form::number("ashift", "ashift", "12"),
                        form::check("add_storage", "Add as PVE storage", true),
                    ],
                )));
            }
            't' => ctx.popup(Popup::Form(Form::new(
                "Create LVM-thin",
                "lvmthin",
                vec![form::info("Device", &dev), form::text("name", "Name", "data2").req(true), form::check("add_storage", "Add as PVE storage", true)],
            ))),
            'v' => ctx.popup(Popup::Form(Form::new(
                "Create LVM volume group",
                "lvm",
                vec![form::info("Device", &dev), form::text("name", "Name", "vg2").req(true), form::check("add_storage", "Add as PVE storage", true)],
            ))),
            'd' => ctx.popup(Popup::Form(Form::new(
                "Create directory storage",
                "dir",
                vec![
                    form::info("Device", &dev),
                    form::text("name", "Name", "disk2").req(true),
                    form::select("filesystem", "Filesystem", form::opts(&["ext4", "xfs"]), "ext4"),
                    form::check("add_storage", "Add as PVE storage", true),
                ],
            ))),
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        let n = ctx.node.clone();
        if let Some(dev) = tag.strip_prefix("gpt:") {
            ctx.api_call(format!("Init GPT {dev}"), "POST", format!("/nodes/{n}/disks/initgpt"), vec![("disk", dev.into())]);
        } else if let Some(dev) = tag.strip_prefix("wipe:") {
            ctx.api_call(format!("Wipe {dev}"), "PUT", format!("/nodes/{n}/disks/wipedisk"), vec![("disk", dev.into())]);
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let n = ctx.node.clone();
        let dev = self.sel().map(|d| js(d, "devpath")).unwrap_or_default();
        let name = fv(v, "name").to_string();
        let add = fv(v, "add_storage").to_string();
        match tag {
            "zfs" => {
                let devs = self.selected_devs().join(",");
                ctx.api_call(
                    format!("Create ZFS pool {name}"),
                    "POST",
                    format!("/nodes/{n}/disks/zfs"),
                    vec![
                        ("name", name.clone()),
                        ("raidlevel", fv(v, "raidlevel").into()),
                        ("devices", devs),
                        ("compression", fv(v, "compression").into()),
                        ("ashift", fv(v, "ashift").into()),
                        ("add_storage", add),
                    ],
                );
                self.table.marked.clear();
            }
            "lvmthin" => ctx.api_call(format!("Create LVM-thin {name}"), "POST", format!("/nodes/{n}/disks/lvmthin"), vec![("name", name.clone()), ("device", dev), ("add_storage", add)]),
            "lvm" => ctx.api_call(format!("Create VG {name}"), "POST", format!("/nodes/{n}/disks/lvm"), vec![("name", name.clone()), ("device", dev), ("add_storage", add)]),
            "dir" => ctx.api_call(
                format!("Create directory {name}"),
                "POST",
                format!("/nodes/{n}/disks/directory"),
                vec![("name", name.clone()), ("device", dev), ("filesystem", fv(v, "filesystem").into()), ("add_storage", add)],
            ),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.data.clear();
        self.smart.clear();
    }
}

// ===========================================================================
// Partitions (lsblk tree + usage)
// ===========================================================================

#[derive(Clone)]
struct Blk {
    depth: usize,
    name: String,
    path: String,
    size: f64,
    kind: String,
    fstype: String,
    mount: String,
    label: String,
    model: String,
    used: Option<(f64, f64)>,
}

#[derive(Default)]
struct Partitions {
    data: Loader<Vec<Blk>>,
    table: Table,
}

fn flatten_blk(v: &Value, depth: usize, usage: &HashMap<String, (f64, f64)>, out: &mut Vec<Blk>) {
    for d in jarr(v) {
        let mount = js(&d, "mountpoint");
        out.push(Blk {
            depth,
            name: js(&d, "name"),
            path: js(&d, "path"),
            size: jf(&d, "size"),
            kind: js(&d, "type"),
            fstype: js(&d, "fstype"),
            label: js(&d, "label"),
            model: js(&d, "model"),
            used: usage.get(&mount).cloned(),
            mount,
        });
        if let Some(c) = d.get("children") {
            flatten_blk(c, depth + 1, usage, out);
        }
    }
}

impl View for Partitions {
    fn title(&self) -> &'static str {
        "Partitions"
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
            let out = exec.output("lsblk", &["-J", "-b", "-o", "NAME,PATH,SIZE,TYPE,FSTYPE,MOUNTPOINT,LABEL,MODEL"])?;
            let v: Value = serde_json::from_str(&out)?;
            let df = exec.output("df", &["-B1", "--output=target,size,used"]).unwrap_or_default();
            let usage: HashMap<String, (f64, f64)> = df
                .lines()
                .skip(1)
                .filter_map(|l| {
                    let p: Vec<&str> = l.split_whitespace().collect();
                    if p.len() >= 3 {
                        Some((p[0].to_string(), (p[2].parse().unwrap_or(0.0), p[1].parse().unwrap_or(0.0))))
                    } else {
                        None
                    }
                })
                .collect();
            let mut out = Vec::new();
            flatten_blk(v.get("blockdevices").unwrap_or(&Value::Null), 0, &usage, &mut out);
            Ok(out)
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

    fn has_detail(&self) -> bool {
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flatten()
            .map(|b| {
                let indent = if b.depth == 0 { String::new() } else { format!("{}└─", "  ".repeat(b.depth - 1)) };
                let (usage, st) = match b.used {
                    Some((u, t)) if t > 0.0 => (format!("{} {:.0}%", bar(u / t, 10), u / t * 100.0), usage_style(u / t)),
                    _ => (String::new(), Style::default()),
                };
                Row::new(b.path.clone())
                    .cs(format!("{indent}{}", b.name), if b.depth == 0 { Style::default().fg(ACCENT) } else { Style::default() })
                    .c(b.kind.clone())
                    .c(fmt_bytes(b.size))
                    .c(b.fstype.clone())
                    .c(b.mount.clone())
                    .cs(usage, st)
                    .c(if b.depth == 0 { b.model.clone() } else { b.label.clone() })
            })
            .collect();
        let cols = [
            col("Name", Constraint::Length(26)),
            col("Type", Constraint::Length(5)),
            col("Size", Constraint::Length(7)),
            col("FS", Constraint::Length(12)),
            col("Mountpoint", Constraint::Min(14)),
            col("Usage", Constraint::Length(16)),
            col("Model/Label", Constraint::Length(18)),
        ];
        self.table.draw(f, area, "Block devices", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

// ===========================================================================
// LVM
// ===========================================================================

#[derive(Default)]
struct LvmData {
    vgs: Vec<Value>,
    thin: Vec<Value>,
    lvs: Vec<Value>,
}

#[derive(Default)]
struct Lvm {
    data: Loader<LvmData>,
    table: Table,
}

impl View for Lvm {
    fn title(&self) -> &'static str {
        "LVM"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let exec = ctx.exec();
        self.data.load(move || {
            let lvm = api.get(&format!("/nodes/{node}/disks/lvm"))?;
            let thin = jarr(&api.get(&format!("/nodes/{node}/disks/lvmthin")).unwrap_or(Value::Null));
            let lvs = exec
                .and_then(|e| {
                    e.output("lvs", &["--reportformat", "json", "--units", "b", "--nosuffix", "-o", "lv_name,vg_name,lv_size,lv_attr,pool_lv,data_percent,metadata_percent,origin"]).ok()
                })
                .and_then(|o| serde_json::from_str::<Value>(&o).ok())
                .and_then(|v| v.get("report").and_then(|r| r.get(0)).and_then(|r| r.get("lv")).cloned())
                .map(|v| jarr(&v))
                .unwrap_or_default();
            Ok(LvmData { vgs: lvm.get("children").map(jarr).unwrap_or_default(), thin, lvs })
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
        let mut rows = Vec::new();
        if let Some(d) = &self.data.data {
            for vg in &d.vgs {
                let (size, free) = (jf(vg, "size"), jf(vg, "free"));
                let frac = if size > 0.0 { 1.0 - free / size } else { 0.0 };
                rows.push(
                    Row::new(format!("vg:{}", js(vg, "name")))
                        .cs(js(vg, "name"), Style::default().fg(ACCENT))
                        .c("VG")
                        .c(fmt_bytes(size))
                        .cs(format!("{} {:.0}%", bar(frac, 10), frac * 100.0), usage_style(frac))
                        .c(format!("free {}", fmt_bytes(free)))
                        .c(""),
                );
                for t in d.thin.iter().filter(|t| js(t, "vg") == js(vg, "name")) {
                    let ds = jf(t, "lv_size");
                    let df = if ds > 0.0 { jf(t, "used") / ds } else { 0.0 };
                    let ms = jf(t, "metadata_size");
                    let mf = if ms > 0.0 { jf(t, "metadata_used") / ms } else { 0.0 };
                    rows.push(
                        Row::new(format!("thin:{}/{}", js(t, "vg"), js(t, "lv")))
                            .c(format!("  └─{}", js(t, "lv")))
                            .c("thin")
                            .c(fmt_bytes(ds))
                            .cs(format!("{} {:.0}%", bar(df, 10), df * 100.0), usage_style(df))
                            .cs(format!("meta {:.0}%", mf * 100.0), usage_style(mf))
                            .c(""),
                    );
                }
                for lv in d.lvs.iter().filter(|l| js(l, "vg_name") == js(vg, "name") && !js(l, "lv_attr").starts_with('t')) {
                    let dp = js(lv, "data_percent");
                    rows.push(
                        Row::new(format!("lv:{}/{}", js(lv, "vg_name"), js(lv, "lv_name")))
                            .c(format!("    {}", js(lv, "lv_name")))
                            .c(js(lv, "lv_attr"))
                            .c(fmt_bytes(jf(lv, "lv_size")))
                            .c(if dp.is_empty() { String::new() } else { format!("{dp}%") })
                            .c(js(lv, "pool_lv"))
                            .c(js(lv, "origin"))
                            .dim(true),
                    );
                }
            }
        }
        let cols = [
            col("Name", Constraint::Min(18)),
            col("Type", Constraint::Length(10)),
            col("Size", Constraint::Length(8)),
            col("Usage", Constraint::Length(16)),
            col("Free/Meta/Pool", Constraint::Length(16)),
            col("Origin", Constraint::Length(12)),
        ];
        self.table.draw(f, area, "LVM", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let (Some(k), Some(d)) = (self.table.selected(), &self.data.data) {
            if let Some(name) = k.strip_prefix("vg:") {
                if let Some(vg) = d.vgs.iter().find(|v| js(v, "name") == name) {
                    kv.add("Volume group", name);
                    kv.add("Size", fmt_bytes(jf(vg, "size")));
                    kv.add("Free", fmt_bytes(jf(vg, "free")));
                    kv.section("Physical volumes");
                    for pv in vg.get("children").map(jarr).unwrap_or_default() {
                        kv.add(&js(&pv, "name"), format!("{} (free {})", fmt_bytes(jf(&pv, "size")), fmt_bytes(jf(&pv, "free"))));
                    }
                }
            } else if let Some(n) = k.strip_prefix("thin:") {
                if let Some(t) = d.thin.iter().find(|t| format!("{}/{}", js(t, "vg"), js(t, "lv")) == n) {
                    kv.add("Thin pool", n);
                    kv.add("Data", format!("{} / {}", fmt_bytes(jf(t, "used")), fmt_bytes(jf(t, "lv_size"))));
                    let ms = jf(t, "metadata_size");
                    let mf = if ms > 0.0 { jf(t, "metadata_used") / ms } else { 0.0 };
                    kv.styled("Metadata", format!("{} / {} ({:.0}%)", fmt_bytes(jf(t, "metadata_used")), fmt_bytes(ms), mf * 100.0), usage_style(mf));
                    if mf > 0.8 {
                        kv.styled("", "Metadata almost full: extend it (lvextend --poolmetadatasize) before the pool goes read-only", style_bad());
                    }
                }
            } else if let Some(n) = k.strip_prefix("lv:") {
                if let Some(l) = d.lvs.iter().find(|l| format!("{}/{}", js(l, "vg_name"), js(l, "lv_name")) == n) {
                    if let Value::Object(m) = l {
                        for (k, v) in m {
                            kv.add(k, v.as_str().unwrap_or("").to_string());
                        }
                    }
                }
            }
        }
        if kv.items.is_empty() && self.data.data.as_ref().map(|d| d.vgs.is_empty()).unwrap_or(false) {
            kv.styled("", tr("not configured"), style_dim());
        }
        draw_lines(f, area, "Details", focused, kv.lines(), 0);
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

// ===========================================================================
// ZFS
// ===========================================================================

#[derive(Default)]
struct Zfs {
    pools: Loader<Vec<Value>>,
    detail: HashMap<String, Loader<Value>>,
    datasets: Loader<Vec<Vec<String>>>,
    arc: Loader<HashMap<String, f64>>,
    /// pool -> read ops/s, write ops/s, read B/s, write B/s, read wait ms, write wait ms
    iostat: Loader<HashMap<String, [f64; 6]>>,
    table: Table,
    scroll: u16,
}

/// `zpool iostat -Hpl 1 2`: the last line of each pool is the 1 s interval.
fn parse_zpool_iostat(out: &str) -> HashMap<String, [f64; 6]> {
    let mut m = HashMap::new();
    for l in out.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 11 {
            continue;
        }
        let n = |i: usize| f[i].trim().parse::<f64>().unwrap_or(0.0);
        m.insert(f[0].to_string(), [n(3), n(4), n(5), n(6), n(7) / 1e6, n(8) / 1e6]);
    }
    m
}

fn vdev_lines(v: &Value, depth: usize, kv: &mut Kv) {
    for c in v.get("children").map(jarr).unwrap_or_default() {
        let state = js(&c, "state");
        let errs = format!("r{} w{} c{}", js(&c, "read"), js(&c, "write"), js(&c, "cksum"));
        let msg = js(&c, "msg");
        kv.styled(&format!("{}{}", "  ".repeat(depth), js(&c, "name")), format!("{state}  {errs} {msg}"), status_style(if state == "ONLINE" { "ok" } else { "error" }));
        vdev_lines(&c, depth + 1, kv);
    }
}

impl View for Zfs {
    fn title(&self) -> &'static str {
        "ZFS"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.pools.load(move || Ok(jarr(&api.get(&format!("/nodes/{node}/disks/zfs"))?)));
        if let Some(exec) = ctx.exec() {
            let e2 = exec.clone();
            let e3 = exec.clone();
            self.iostat.load(move || Ok(parse_zpool_iostat(&e3.output("zpool", &["iostat", "-Hpl", "1", "2"])?)));
            self.datasets.load(move || {
                let out = exec.output("zfs", &["list", "-Hp", "-o", "name,used,avail,refer,mountpoint,compressratio,type", "-t", "filesystem,volume"])?;
                Ok(out.lines().map(|l| l.split('\t').map(|s| s.to_string()).collect()).collect())
            });
            self.arc.load(move || {
                let s = e2.read_file("/proc/spl/kstat/zfs/arcstats").unwrap_or_default();
                Ok(s.lines()
                    .skip(2)
                    .filter_map(|l| {
                        let p: Vec<&str> = l.split_whitespace().collect();
                        (p.len() == 3).then(|| (p[0].to_string(), p[2].parse().unwrap_or(0.0)))
                    })
                    .collect())
            });
        }
        for l in self.detail.values_mut() {
            l.at = None;
        }
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let mut ch = self.pools.poll() | self.datasets.poll() | self.arc.poll();
        for l in self.detail.values_mut() {
            ch |= l.poll();
        }
        if let Some(p) = self.table.selected().map(|s| s.to_string()) {
            let l = self.detail.entry(p.clone()).or_default();
            if l.stale(std::time::Duration::from_secs(15)) {
                let api = ctx.api.clone();
                let node = ctx.node.clone();
                l.load(move || api.get(&format!("/nodes/{node}/disks/zfs/{}", urlenc(&p))));
            }
        }
        ch
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(20)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        if let Some(e) = &self.pools.error {
            if not_configured(e) {
                draw_message_in(f, area, "ZFS", focused, &format!("ZFS: {} ({})", tr("not configured"), e.lines().next().unwrap_or("")), style_dim());
                return;
            }
        }
        let rows: Vec<Row> = self
            .pools
            .data
            .iter()
            .flatten()
            .map(|p| {
                let (size, alloc) = (jf(p, "size"), jf(p, "alloc"));
                let frac = if size > 0.0 { alloc / size } else { 0.0 };
                let health = js(p, "health");
                let io = self.iostat.data.as_ref().and_then(|m| m.get(&js(p, "name"))).copied();
                let row = Row::new(js(p, "name"))
                    .cs(js(p, "name"), Style::default().fg(ACCENT))
                    .cs(health.clone(), status_style(if health == "ONLINE" { "ok" } else { "error" }))
                    .c(fmt_bytes(size))
                    .cs(format!("{} {:.0}%", bar(frac, 10), frac * 100.0), usage_style(frac))
                    .c(fmt_bytes(jf(p, "free")))
                    .c(format!("{}%", js(p, "frag")))
                    .c(format!("{}x", js(p, "dedup")));
                match io {
                    Some(v) => {
                        let lat = v[4].max(v[5]);
                        row.c(crate::util::fmt_rate(v[2])).c(crate::util::fmt_rate(v[3])).c(format!("{:.0}", v[0] + v[1])).cs(super::diskio::ms(lat), super::diskio::lat_style(lat))
                    }
                    None => row.c("").c("").c("").c(""),
                }
            })
            .collect();
        let cols = [
            col("Pool", Constraint::Min(12)),
            col("Health", Constraint::Length(9)),
            col("Size", Constraint::Length(8)),
            col("Used", Constraint::Length(16)),
            col("Free", Constraint::Length(8)),
            col("Frag", Constraint::Length(5)),
            col("Dedup", Constraint::Length(6)),
            col("Read", Constraint::Length(9)),
            col("Write", Constraint::Length(9)),
            col("IOPS", Constraint::Length(6)),
            col("Latency", Constraint::Length(7)),
        ];
        self.table.draw(f, area, "ZFS pools", &cols, &rows, focused, loading_or_error(&self.pools.error, self.pools.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(p) = self.table.selected() {
            if let Some(v) = self.iostat.data.as_ref().and_then(|m| m.get(p)) {
                kv.section("IO");
                kv.add("Read", format!("{}  {:.0} IOPS  {} {}", crate::util::fmt_rate(v[2]), v[0], tr("latency"), super::diskio::ms(v[4])));
                kv.add("Write", format!("{}  {:.0} IOPS  {} {}", crate::util::fmt_rate(v[3]), v[1], tr("latency"), super::diskio::ms(v[5])));
            }
            if let Some(d) = self.detail.get(p).and_then(|l| l.data.as_ref()) {
                kv.status("State", js(d, "state"));
                for k in ["status", "action", "scan", "errors"] {
                    let v = js(d, k);
                    if !v.is_empty() {
                        kv.add(k, v);
                    }
                }
                kv.section("vdevs");
                vdev_lines(d, 0, &mut kv);
            }
            if let Some(ds) = &self.datasets.data {
                kv.section("Datasets");
                for r in ds.iter().filter(|r| r.first().map(|n| n == p || n.starts_with(&format!("{p}/"))).unwrap_or(false)) {
                    let g = |i: usize| r.get(i).cloned().unwrap_or_default();
                    let num = |i: usize| fmt_bytes(g(i).parse().unwrap_or(0.0));
                    kv.add(&g(0), format!("{} used {} avail ratio {} {}", g(6), num(1), num(2), g(5)));
                }
            }
        }
        if let Some(a) = &self.arc.data {
            if !a.is_empty() {
                kv.section("ARC");
                let size = a.get("size").cloned().unwrap_or(0.0);
                let max = a.get("c_max").cloned().unwrap_or(0.0);
                let hits = a.get("hits").cloned().unwrap_or(0.0);
                let miss = a.get("misses").cloned().unwrap_or(0.0);
                kv.add("Size", format!("{} / max {}", fmt_bytes(size), fmt_bytes(max)));
                if hits + miss > 0.0 {
                    kv.add("Hit ratio", format!("{:.1}%", hits / (hits + miss) * 100.0));
                }
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        if self.table.selected().is_none() {
            return vec![];
        }
        let e = ctx.need_exec("Sys.Modify").map(|_| ());
        vec![
            item('s', "Start scrub").require(e.clone()),
            item('S', "Stop scrub").require(e.clone()),
            item('c', "Clear errors (zpool clear)").require(e.clone()),
            item('t', "Show zpool status -v").require(ctx.need_exec("Sys.Audit").map(|_| ())),
            item('i', "Show zpool iostat -v").require(ctx.need_exec("Sys.Audit").map(|_| ())),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(pool) = self.table.selected().map(|s| s.to_string()) else { return };
        let Some(exec) = ctx.exec() else { return };
        match key {
            's' => ctx.exec_call(format!("zpool scrub {pool}"), exec, "zpool", vec!["scrub".into(), pool]),
            'S' => ctx.exec_call(format!("zpool scrub -s {pool}"), exec, "zpool", vec!["scrub".into(), "-s".into(), pool]),
            'c' => ctx.popup(Popup::confirm("zpool clear", &format!("clear:{pool}"), vec![format!("Clear error counters of {pool}?")], false)),
            't' | 'i' => {
                let args: Vec<String> = if key == 't' { vec!["status".into(), "-v".into(), pool.clone()] } else { vec!["iostat".into(), "-v".into(), pool.clone()] };
                ctx.act(format!("zpool {}", args[0]), move || {
                    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                    let out = exec.output("zpool", &a)?;
                    Ok(ActionOutcome::Text(format!("zpool {} {pool}", a[0]), out))
                });
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if let (Some(pool), Some(exec)) = (tag.strip_prefix("clear:"), ctx.exec()) {
            ctx.exec_call(format!("zpool clear {pool}"), exec, "zpool", vec!["clear".into(), pool.into()]);
        }
    }

    fn reset(&mut self) {
        self.pools.clear();
        self.detail.clear();
        self.datasets.clear();
        self.arc.clear();
    }
}

// ===========================================================================
// Ceph
// ===========================================================================

#[derive(Default)]
struct CephData {
    status: Value,
    osds: Vec<Value>,
    pools: Vec<Value>,
    flags: String,
}

#[derive(Default)]
struct Ceph {
    data: Loader<CephData>,
    table: Table,
    scroll: u16,
}

fn flatten_osd(v: &Value, host: &str, out: &mut Vec<Value>) {
    for c in v.get("children").map(jarr).unwrap_or_default() {
        let t = js(&c, "type");
        if t == "osd" {
            let mut c2 = c.clone();
            if let Value::Object(m) = &mut c2 {
                m.insert("hostname".into(), Value::String(host.to_string()));
            }
            out.push(c2);
        } else {
            let h = if t == "host" { js(&c, "name") } else { host.to_string() };
            flatten_osd(&c, &h, out);
        }
    }
}

impl View for Ceph {
    fn title(&self) -> &'static str {
        "Ceph"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.data.load(move || {
            let status = api.get(&format!("/nodes/{node}/ceph/status"))?;
            let osd = api.get(&format!("/nodes/{node}/ceph/osd")).unwrap_or(Value::Null);
            let mut osds = Vec::new();
            if let Some(root) = osd.get("root") {
                flatten_osd(root, "", &mut osds);
            }
            let flags = js(&osd, "flags");
            let pools = jarr(&api.get(&format!("/nodes/{node}/ceph/pool")).unwrap_or(Value::Null));
            Ok(CephData { status, osds, pools, flags })
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
        if let Some(e) = &self.data.error {
            if not_configured(e) {
                draw_message_in(f, area, "Ceph", focused, &format!("Ceph: {} ({})", tr("not configured"), e.lines().next().unwrap_or("")), style_dim());
                return;
            }
        }
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flat_map(|d| d.osds.iter())
            .map(|o| {
                let up = js(o, "status") == "up";
                let inn = jf(o, "in") > 0.0;
                let pct = jf(o, "percent_used");
                Row::new(js(o, "id"))
                    .c(js(o, "name"))
                    .c(js(o, "hostname"))
                    .cs(if up { "up" } else { "down" }, if up { style_good() } else { style_bad() })
                    .cs(if inn { "in" } else { "out" }, if inn { style_good() } else { style_warn() })
                    .c(js(o, "device_class"))
                    .cs(format!("{} {pct:.0}%", bar(pct / 100.0, 10)), usage_style(pct / 100.0))
                    .c(fmt_bytes(jf(o, "total_space")))
                    .c(js(o, "apply_latency_ms"))
            })
            .collect();
        let cols = [
            col("OSD", Constraint::Length(8)),
            col("Host", Constraint::Length(12)),
            col("Status", Constraint::Length(6)),
            col("In", Constraint::Length(4)),
            col("Class", Constraint::Length(6)),
            col("Used", Constraint::Length(16)),
            col("Size", Constraint::Length(8)),
            col("Lat ms", Constraint::Min(6)),
        ];
        self.table.draw(f, area, "Ceph OSDs", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut lines: Vec<Line> = Vec::new();
        if let Some(d) = &self.data.data {
            let mut kv = Kv::new();
            let h = d.status.get("health").cloned().unwrap_or(Value::Null);
            let hs = js(&h, "status");
            kv.styled("Health", hs.clone(), if hs == "HEALTH_OK" { style_good() } else if hs == "HEALTH_WARN" { style_warn() } else { style_bad() });
            if let Some(Value::Object(checks)) = h.get("checks") {
                for (k, c) in checks {
                    let msg = c.get("summary").map(|s| js(s, "message")).unwrap_or_default();
                    kv.styled(k, msg, if js(c, "severity") == "HEALTH_ERR" { style_bad() } else { style_warn() });
                }
            }
            if !d.flags.is_empty() {
                kv.styled("Flags", d.flags.clone(), style_warn());
            }
            if let Some(pg) = d.status.get("pgmap") {
                kv.section("PGs");
                kv.add("PGs", js(pg, "num_pgs"));
                for s in pg.get("pgs_by_state").map(jarr).unwrap_or_default() {
                    let name = js(&s, "state_name");
                    kv.styled(&name, js(&s, "count"), if name == "active+clean" { style_good() } else { style_warn() });
                }
                let (used, total) = (jf(pg, "bytes_used"), jf(pg, "bytes_total"));
                if total > 0.0 {
                    kv.styled("Usage", format!("{} / {}", fmt_bytes(used), fmt_bytes(total)), usage_style(used / total));
                }
                let rd = jf(pg, "read_bytes_sec");
                let wr = jf(pg, "write_bytes_sec");
                kv.add("IO", format!("R {}/s W {}/s", fmt_bytes(rd), fmt_bytes(wr)));
            }
            if let Some(m) = d.status.get("monmap") {
                kv.section("Monitors");
                for mon in m.get("mons").map(jarr).unwrap_or_default() {
                    kv.add(&js(&mon, "name"), js(&mon, "public_addr"));
                }
            }
            kv.section("Pools");
            for p in &d.pools {
                let frac = jf(p, "percent_used");
                kv.styled(&js(p, "pool_name"), format!("size {}/{}  pgs {}  used {:.1}%", js(p, "size"), js(p, "min_size"), js(p, "pg_num"), frac * 100.0), usage_style(frac));
            }
            lines = kv.lines();
        }
        draw_lines(f, area, "Ceph status", focused, lines, self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        if self.data.data.is_none() {
            return vec![];
        }
        let m = ctx.need_priv("/", "Sys.Modify");
        let noout = self.data.data.as_ref().map(|d| d.flags.contains("noout")).unwrap_or(false);
        let sel = self.table.selected().is_some();
        vec![
            item('i', "Mark OSD in").require(m.clone()).disabled_if(!sel, "no OSD"),
            item('o', "Mark OSD out").require(m.clone()).disabled_if(!sel, "no OSD"),
            item('N', if noout { "Unset noout (end maintenance)" } else { "Set noout (maintenance)" }).require(m),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let n = ctx.node.clone();
        let id = self.table.selected().unwrap_or("").to_string();
        match key {
            'i' => ctx.api_call(format!("OSD {id} in"), "POST", format!("/nodes/{n}/ceph/osd/{id}/in"), vec![]),
            'o' => ctx.popup(Popup::confirm("OSD out", &format!("out:{id}"), vec![format!("Mark osd.{id} out? Data will be rebalanced.")], true)),
            'N' => {
                let noout = self.data.data.as_ref().map(|d| d.flags.contains("noout")).unwrap_or(false);
                ctx.api_call("noout flag", "PUT", "/cluster/ceph/flags/noout".into(), vec![("value", if noout { "0" } else { "1" }.into())]);
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if let Some(id) = tag.strip_prefix("out:") {
            let n = ctx.node.clone();
            ctx.api_call(format!("OSD {id} out"), "POST", format!("/nodes/{n}/ceph/osd/{id}/out"), vec![]);
        }
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}
