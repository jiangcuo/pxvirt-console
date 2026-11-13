//! System > PCI / USB / Hardware info.
//!
//! Lists come from the host (lspci/lsusb + sysfs, local or over ssh) and fall back to the
//! PVE API (/nodes/x/hardware/pci|usb) in API-only mode. Link speed, drivers, SR-IOV and
//! all actions (bind/unbind drivers, vfio-pci, reset, VFs) need host access: the API has
//! no such operations.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    Frame,
};
use std::collections::HashMap;

use super::{fv, scroll, View};
use crate::ctx::{ActionOutcome, Ctx};
use crate::exec::Exec;
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::form::{self, Form};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, style_bad, style_dim, style_good, style_warn, Kv, ACCENT};
use crate::util::{jarr, js};

/// Run a root shell snippet with positional arguments and report the first output line.
fn run_sh(ctx: &mut Ctx, exec: Exec, title: String, script: &'static str, args: Vec<String>) {
    let t = title.clone();
    ctx.act(title, move || {
        let mut a = vec!["-c".to_string(), script.to_string(), "sh".to_string()];
        a.extend(args);
        let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        let out = exec.output("sh", &refs)?;
        let first = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        Ok(ActionOutcome::Msg(if first.is_empty() { format!("{t}: OK") } else { format!("{t}: {first}") }))
    });
}

fn valid_pci_addr(a: &str) -> bool {
    !a.is_empty() && a.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
}

// ===========================================================================
// PCI
// ===========================================================================

#[derive(Clone, Default)]
struct PciDev {
    addr: String,
    class_code: u32,
    class_name: String,
    vendor: String,
    device: String,
    ids: String,
    subsystem: String,
    driver: String,
    modules: Vec<String>,
    iommu: String,
    link_cur: String,
    link_max: String,
    width_cur: String,
    width_max: String,
    vfs_total: u32,
    vfs_num: u32,
    physfn: String,
    driver_override: String,
    numa: String,
    reset_method: String,
    /// host resources on this device (netdevs, disks, mounts)
    used_by: Vec<String>,
}

impl PciDev {
    fn category(&self) -> &'static str {
        match self.class_code >> 8 {
            0x01 => "Storage",
            0x02 => "Network",
            0x03 => "Display",
            0x04 => "Multimedia",
            0x06 => "Bridge",
            0x0c => "Serial bus",
            0x12 => "Accelerator",
            0x08 => "System",
            0x0b => "Processor",
            _ => "Other",
        }
    }

    fn degraded(&self) -> bool {
        let num = |s: &str| s.split_whitespace().next().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
        (num(&self.link_cur) > 0.0 && num(&self.link_cur) < num(&self.link_max))
            || (!self.width_cur.is_empty() && self.width_cur.parse::<u32>().unwrap_or(0) < self.width_max.parse::<u32>().unwrap_or(0))
    }

    fn link_short(&self) -> String {
        if self.link_cur.is_empty() || self.link_cur.starts_with("Unknown") {
            return String::new();
        }
        let s = |x: &str| x.split_whitespace().next().unwrap_or("").to_string();
        format!("{}GT x{}", s(&self.link_cur), self.width_cur)
    }
}

/// "Name [abcd]" -> ("Name", "abcd")
fn split_id(s: &str) -> (String, String) {
    match (s.rfind(" ["), s.ends_with(']')) {
        (Some(i), true) => (s[..i].to_string(), s[i + 2..s.len() - 1].to_string()),
        _ => (s.to_string(), String::new()),
    }
}

const PCI_SCRIPT: &str = r#"export LC_ALL=C
lspci -Dvmmnnk
echo '@@@SYSFS'
for d in /sys/bus/pci/devices/*; do a=${d##*/}
 p=; [ -e $d/physfn ] && p=$(basename $(readlink $d/physfn))
 echo "$a|$(cat $d/current_link_speed 2>/dev/null)|$(cat $d/max_link_speed 2>/dev/null)|$(cat $d/current_link_width 2>/dev/null)|$(cat $d/max_link_width 2>/dev/null)|$(cat $d/sriov_totalvfs 2>/dev/null)|$(cat $d/sriov_numvfs 2>/dev/null)|$p|$(cat $d/driver_override 2>/dev/null)|$(cat $d/numa_node 2>/dev/null)|$(cat $d/reset_method 2>/dev/null)"
done
echo '@@@NET'
for n in /sys/class/net/*; do [ -e $n/device ] && echo "${n##*/} $(readlink -f $n/device)"; done
echo '@@@DEFROUTE'
ip -o route show default 2>/dev/null | awk '{print $5}'
echo '@@@BLOCK'
for b in /sys/block/*; do [ -e $b/device ] && echo "${b##*/} $(readlink -f $b/device)"; done
echo '@@@MOUNTS'
cat /proc/mounts"#;

fn load_pci_host(exec: &Exec) -> anyhow::Result<Vec<PciDev>> {
    let out = exec.output("sh", &["-c", PCI_SCRIPT])?;
    let mut sections: HashMap<&str, String> = HashMap::new();
    let mut cur = "LSPCI";
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("@@@") {
            cur = match name {
                "SYSFS" => "SYSFS",
                "NET" => "NET",
                "DEFROUTE" => "DEFROUTE",
                "BLOCK" => "BLOCK",
                _ => "MOUNTS",
            };
            continue;
        }
        let e = sections.entry(cur).or_default();
        e.push_str(line);
        e.push('\n');
    }
    let mut devs: Vec<PciDev> = Vec::new();
    let mut d = PciDev::default();
    for line in sections.get("LSPCI").map(|s| s.as_str()).unwrap_or("").lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if !d.addr.is_empty() {
                devs.push(std::mem::take(&mut d));
            }
            continue;
        }
        let Some((k, v)) = line.split_once(":\t") else { continue };
        match k {
            "Slot" => d.addr = v.to_string(),
            "Class" => {
                let (n, id) = split_id(v);
                d.class_name = n;
                d.class_code = u32::from_str_radix(&id, 16).unwrap_or(0);
            }
            "Vendor" => {
                let (n, id) = split_id(v);
                d.vendor = n;
                d.ids = id;
            }
            "Device" => {
                let (n, id) = split_id(v);
                d.device = n;
                d.ids = format!("{}:{id}", d.ids);
            }
            "SVendor" => d.subsystem = split_id(v).0,
            "SDevice" => d.subsystem = format!("{} {}", d.subsystem, split_id(v).0),
            "Driver" => d.driver = v.to_string(),
            "Module" => d.modules.push(v.to_string()),
            "IOMMUGroup" => d.iommu = v.to_string(),
            _ => {}
        }
    }
    let sysfs: HashMap<String, Vec<String>> = sections
        .get("SYSFS")
        .map(|s| s.lines().map(|l| l.split('|').map(|x| x.to_string()).collect::<Vec<_>>()).filter(|v| v.len() >= 11).map(|v| (v[0].clone(), v)).collect())
        .unwrap_or_default();
    let defroute: Vec<String> = sections.get("DEFROUTE").map(|s| s.split_whitespace().map(|x| x.to_string()).collect()).unwrap_or_default();
    let mounts = sections.get("MOUNTS").cloned().unwrap_or_default();
    for dev in devs.iter_mut() {
        if let Some(v) = sysfs.get(&dev.addr) {
            dev.link_cur = v[1].clone();
            dev.link_max = v[2].clone();
            dev.width_cur = v[3].clone();
            dev.width_max = v[4].clone();
            dev.vfs_total = v[5].parse().unwrap_or(0);
            dev.vfs_num = v[6].parse().unwrap_or(0);
            dev.physfn = v[7].clone();
            dev.driver_override = if v[8] == "(null)" { String::new() } else { v[8].clone() };
            dev.numa = v[9].clone();
            dev.reset_method = v[10].clone();
        }
        let tag = format!("/{}/", dev.addr);
        for l in sections.get("NET").map(|s| s.as_str()).unwrap_or("").lines() {
            if let Some((n, path)) = l.split_once(' ') {
                if format!("{path}/").contains(&tag) {
                    let mgmt = if defroute.iter().any(|r| r == n) { " (default route)" } else { "" };
                    dev.used_by.push(format!("net {n}{mgmt}"));
                }
            }
        }
        for l in sections.get("BLOCK").map(|s| s.as_str()).unwrap_or("").lines() {
            if let Some((b, path)) = l.split_once(' ') {
                if format!("{path}/").contains(&tag) {
                    let mounted: Vec<&str> = mounts
                        .lines()
                        .filter_map(|m| {
                            let mut p = m.split_whitespace();
                            let src = p.next()?;
                            let dst = p.next()?;
                            src.contains(b).then_some(dst)
                        })
                        .collect();
                    if mounted.is_empty() {
                        dev.used_by.push(format!("disk {b}"));
                    } else {
                        dev.used_by.push(format!("disk {b} mounted on {}", mounted.join(" ")));
                    }
                }
            }
        }
    }
    Ok(devs)
}

fn load_pci_api(ctx_api: &crate::api::Api, node: &str) -> anyhow::Result<Vec<PciDev>> {
    let v = ctx_api.get(&format!("/nodes/{node}/hardware/pci?pci-class-blacklist="))?;
    Ok(jarr(&v)
        .iter()
        .map(|p| PciDev {
            addr: js(p, "id"),
            class_code: u32::from_str_radix(js(p, "class").trim_start_matches("0x"), 16).unwrap_or(0) >> 8,
            class_name: String::new(),
            vendor: js(p, "vendor_name"),
            device: js(p, "device_name"),
            ids: format!("{}:{}", js(p, "vendor").trim_start_matches("0x"), js(p, "device").trim_start_matches("0x")),
            subsystem: format!("{} {}", js(p, "subsystem_vendor_name"), js(p, "subsystem_device_name")).trim().to_string(),
            iommu: js(p, "iommugroup"),
            ..Default::default()
        })
        .collect())
}

const PCI_FILTERS: [&str; 9] = ["devices", "Network", "Storage", "Display", "Serial bus", "Accelerator", "Multimedia", "Bridge", "all"];

#[derive(Default)]
pub struct Pci {
    data: Loader<Vec<PciDev>>,
    table: Table,
    filter: usize,
    scroll: u16,
    from_api: bool,
}

impl Pci {
    fn sel(&self) -> Option<&PciDev> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.iter().find(|d| d.addr == k)
    }

    fn host_ops(&self, ctx: &Ctx) -> Result<(), String> {
        ctx.need_root()?;
        ctx.need_exec("Sys.Modify").map(|_| ())
    }
}

impl View for Pci {
    fn title(&self) -> &'static str {
        "PCI"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let exec = ctx.exec();
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.from_api = exec.is_none();
        self.data.load(move || match exec {
            Some(e) => load_pci_host(&e),
            None => load_pci_api(&api, &node),
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

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("f", "type")]
    }

    fn on_key(&mut self, key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        if key.code == KeyCode::Char('f') {
            self.filter = (self.filter + 1) % PCI_FILTERS.len();
            return true;
        }
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let want = PCI_FILTERS[self.filter];
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flatten()
            .filter(|d| match want {
                "all" => true,
                "devices" => !matches!(d.category(), "Bridge" | "System" | "Processor"),
                c => d.category() == c,
            })
            .map(|d| {
                let drv_style = if d.driver == "vfio-pci" { Style::default().fg(ACCENT) } else if d.driver.is_empty() { style_dim() } else { Style::default() };
                let vfs = if d.vfs_total > 0 { format!("{}/{}", d.vfs_num, d.vfs_total) } else if !d.physfn.is_empty() { "VF".into() } else { String::new() };
                Row::new(d.addr.clone())
                    .c(d.addr.trim_start_matches("0000:").to_string())
                    .c(tr(d.category()).to_string())
                    .c(format!("{} {}", short_vendor(&d.vendor), d.device))
                    .cs(if d.driver.is_empty() { "-".to_string() } else { d.driver.clone() }, drv_style)
                    .cs(d.link_short(), if d.degraded() { style_warn() } else { Style::default() })
                    .c(d.iommu.clone())
                    .c(vfs)
                    .cs(if d.used_by.is_empty() { String::new() } else { "host".into() }, style_warn())
            })
            .collect();
        let cols = [
            col("Address", Constraint::Length(8)),
            col("Type", Constraint::Length(8)),
            col("Device", Constraint::Min(16)),
            col("Driver", Constraint::Length(11)),
            col("Link", Constraint::Length(10)),
            col("IOMMU", Constraint::Length(5)),
            col("VFs", Constraint::Length(5)),
            col("Use", Constraint::Length(4)),
        ];
        let mut title = format!("PCI [{}]", tr(want));
        if self.from_api {
            title.push_str(&format!(" · {}", tr("API: no link / driver details")));
        }
        self.table.draw(f, area, &title, &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(d) = self.sel().cloned() {
            kv.add("Address", d.addr.clone());
            kv.add("Class", format!("{} ({:04x})", if d.class_name.is_empty() { tr(d.category()).to_string() } else { d.class_name.clone() }, d.class_code));
            kv.add("Vendor", d.vendor.clone());
            kv.add("Device", d.device.clone());
            kv.add("IDs", d.ids.clone());
            if !d.subsystem.trim().is_empty() {
                kv.add("Subsystem", d.subsystem.clone());
            }
            kv.section("Driver");
            kv.styled("In use", if d.driver.is_empty() { "-".into() } else { d.driver.clone() }, if d.driver == "vfio-pci" { Style::default().fg(ACCENT) } else { Style::default() });
            if !d.driver_override.is_empty() {
                kv.styled("Override", d.driver_override.clone(), style_warn());
            }
            if !d.modules.is_empty() {
                kv.add("Kernel modules", d.modules.join(", "));
            }
            if !d.used_by.is_empty() {
                kv.section("Used by the host");
                for u in &d.used_by {
                    kv.styled("", u.clone(), style_warn());
                }
            }
            if !d.link_cur.is_empty() && !d.link_cur.starts_with("Unknown") {
                kv.section("PCIe link");
                let st = if d.degraded() { style_warn() } else { style_good() };
                kv.styled("Current", format!("{} x{}", d.link_cur, d.width_cur), st);
                kv.add("Maximum", format!("{} x{}", d.link_max, d.width_max));
                if d.degraded() {
                    kv.styled("", tr("running below its maximum (slot, riser or power saving)"), style_warn());
                }
            }
            if d.vfs_total > 0 || !d.physfn.is_empty() {
                kv.section("SR-IOV");
                if d.vfs_total > 0 {
                    kv.add("VFs enabled", format!("{} of {}", d.vfs_num, d.vfs_total));
                    let vfs: Vec<String> = self.data.data.iter().flatten().filter(|x| x.physfn == d.addr).map(|x| format!("{} {}", x.addr.trim_start_matches("0000:"), if x.driver.is_empty() { "-" } else { &x.driver })).collect();
                    for v in vfs {
                        kv.styled("", v, style_dim());
                    }
                } else {
                    kv.add("Virtual function of", d.physfn.clone());
                }
            }
            kv.section("IOMMU");
            kv.add("Group", if d.iommu.is_empty() { tr("none (IOMMU off?)").to_string() } else { d.iommu.clone() });
            if !d.iommu.is_empty() {
                let peers: Vec<String> = self.data.data.iter().flatten().filter(|x| x.iommu == d.iommu && x.addr != d.addr).map(|x| x.addr.trim_start_matches("0000:").to_string()).collect();
                if !peers.is_empty() {
                    kv.styled("Same group", peers.join(" "), style_warn());
                }
            }
            if !d.numa.is_empty() && d.numa != "-1" {
                kv.add("NUMA node", d.numa.clone());
            }
            if !d.reset_method.is_empty() {
                kv.add("Reset method", d.reset_method.clone());
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(d) = self.sel() else { return vec![] };
        let ops = self.host_ops(ctx);
        let bound = !d.driver.is_empty();
        vec![
            item('v', "Bind to vfio-pci (passthrough)").require(ops.clone()).disabled_if(d.driver == "vfio-pci", "already vfio-pci"),
            item('b', "Bind to driver...").require(ops.clone()),
            item('d', "Restore default driver").require(ops.clone()).disabled_if(d.driver_override.is_empty() && bound, "already default"),
            item('u', "Unbind driver").require(ops.clone()).disabled_if(!bound, "no driver"),
            item('s', "Set SR-IOV VFs").require(ops.clone()).disabled_if(d.vfs_total == 0, "no SR-IOV"),
            item('R', "Reset device").require(ops.clone()).disabled_if(d.reset_method.is_empty() && !self.from_api, "no reset method"),
            item('n', "Rescan PCI bus").require(ops),
        ]
    }

    fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        let Some(d) = self.sel().cloned() else { return };
        if !valid_pci_addr(&d.addr) {
            return;
        }
        if tag == "drv" {
            let mut opts = d.modules.clone();
            if !opts.iter().any(|m| m == "vfio-pci") {
                opts.push("vfio-pci".into());
            }
            if let Some(drv) = opts.get((key as u8).wrapping_sub(b'1') as usize) {
                self.confirm_bind(ctx, &d, drv);
            }
            return;
        }
        let risky = |what: &str| {
            let mut body = vec![format!("{what} {} ({} {})", d.addr, d.vendor, d.device)];
            if !d.used_by.is_empty() {
                body.push(String::new());
                body.push(tr("This device is used by the host:").to_string());
                body.extend(d.used_by.iter().map(|u| format!("  {u}")));
                body.push(tr("The host may lose network or storage access!").to_string());
            }
            body
        };
        match key {
            'v' => self.confirm_bind(ctx, &d, "vfio-pci"),
            'b' => {
                let mut opts = d.modules.clone();
                if !opts.iter().any(|m| m == "vfio-pci") {
                    opts.push("vfio-pci".into());
                }
                let items: Vec<MenuItem> = opts.iter().take(9).enumerate().map(|(i, m)| item(char::from(b'1' + i as u8), m)).collect();
                ctx.popup(Popup::menu("Bind to driver", "drv", items));
            }
            'd' => ctx.popup(Popup::confirm("Restore default driver", "default", risky(tr("Restore the default driver of")), !d.used_by.is_empty())),
            'u' => self.confirm(ctx, &d, "Unbind driver", "unbind", risky(tr("Unbind the driver of"))),
            'R' => self.confirm(ctx, &d, "Reset device", "reset", risky(tr("Reset"))),
            's' => ctx.popup(Popup::Form(Form::new(
                "Set SR-IOV VFs",
                "vfs",
                vec![
                    form::info("Device", &format!("{} {}", d.addr, d.device)),
                    form::number("num", "Number of VFs", &d.vfs_num.to_string()).help(&format!("0-{}; existing VFs are removed first", d.vfs_total)),
                ],
            ))),
            'n' => {
                if let Some(e) = ctx.exec() {
                    run_sh(ctx, e, "Rescan PCI bus".into(), "echo 1 > /sys/bus/pci/rescan", vec![]);
                }
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        let Some(d) = self.sel().cloned() else { return };
        let Some(exec) = ctx.exec() else { return };
        let a = d.addr.clone();
        match tag {
            t if t.starts_with("bind:") => {
                let drv = t[5..].to_string();
                ctx.audit(&format!("pci {a} bind {drv}"));
                run_sh(
                    ctx,
                    exec,
                    format!("{a} -> {drv}"),
                    "d=/sys/bus/pci/devices/$1; modprobe \"$2\" 2>/dev/null; echo \"$2\" > $d/driver_override && \
                     { [ -e $d/driver ] && echo \"$1\" > $d/driver/unbind; echo \"$1\" > /sys/bus/pci/drivers_probe; \
                     basename \"$(readlink $d/driver)\" 2>/dev/null || echo 'no driver bound'; }",
                    vec![a, drv],
                );
            }
            "default" => {
                ctx.audit(&format!("pci {a} default driver"));
                run_sh(
                    ctx,
                    exec,
                    format!("{a} -> default driver"),
                    "d=/sys/bus/pci/devices/$1; echo > $d/driver_override; [ -e $d/driver ] && echo \"$1\" > $d/driver/unbind; \
                     echo \"$1\" > /sys/bus/pci/drivers_probe; basename \"$(readlink $d/driver)\" 2>/dev/null || echo 'no driver bound'",
                    vec![a],
                );
            }
            "unbind" => {
                ctx.audit(&format!("pci {a} unbind"));
                run_sh(ctx, exec, format!("Unbind {a}"), "echo \"$1\" > /sys/bus/pci/devices/$1/driver/unbind", vec![a]);
            }
            "reset" => {
                ctx.audit(&format!("pci {a} reset"));
                run_sh(ctx, exec, format!("Reset {a}"), "echo 1 > /sys/bus/pci/devices/$1/reset", vec![a]);
            }
            _ => {}
        }
    }

    fn on_form(&mut self, _tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let Some(d) = self.sel().cloned() else { return };
        let Some(exec) = ctx.exec() else { return };
        let n: u32 = fv(v, "num").parse().unwrap_or(0).min(d.vfs_total);
        ctx.audit(&format!("pci {} sriov {n}", d.addr));
        run_sh(
            ctx,
            exec,
            format!("{} VFs = {n}", d.addr),
            "f=/sys/bus/pci/devices/$1/sriov_numvfs; echo 0 > $f && { [ \"$2\" -eq 0 ] || echo \"$2\" > $f; } && cat $f",
            vec![d.addr.clone(), n.to_string()],
        );
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

impl Pci {
    fn confirm(&self, ctx: &mut Ctx, d: &PciDev, title: &str, tag: &str, body: Vec<String>) {
        if d.used_by.is_empty() {
            ctx.popup(Popup::confirm(title, tag, body, true));
        } else {
            ctx.popup(Popup::confirm_typed(title, tag, body, d.addr.trim_start_matches("0000:")));
        }
    }

    fn confirm_bind(&self, ctx: &mut Ctx, d: &PciDev, drv: &str) {
        let mut body = vec![format!("{} {} -> {drv}", d.addr, d.device), tr("Takes effect now, until the next reboot.").to_string()];
        if drv == "vfio-pci" && !d.iommu.is_empty() {
            let peers: Vec<String> = self.data.data.iter().flatten().filter(|x| x.iommu == d.iommu && x.addr != d.addr).map(|x| x.addr.clone()).collect();
            if !peers.is_empty() {
                body.push(format!("{} {}", tr("Same IOMMU group:"), peers.join(" ")));
            }
        }
        if !d.used_by.is_empty() {
            body.push(String::new());
            body.push(tr("This device is used by the host:").to_string());
            body.extend(d.used_by.iter().map(|u| format!("  {u}")));
        }
        let tag = format!("bind:{drv}");
        self.confirm(ctx, d, "Bind to driver", &tag, body);
    }
}

fn short_vendor(v: &str) -> String {
    let v = v.replace("Corporation", "").replace(", Inc.", "").replace("Co., Ltd.", "").replace("Technology", "");
    let first: String = v.split(['[', '(']).next().unwrap_or("").trim().to_string();
    crate::util::truncate(&first, 18)
}

// ===========================================================================
// USB
// ===========================================================================

#[derive(Clone, Default)]
struct UsbDev {
    path: String,
    bus: String,
    dev: String,
    id: String,
    name: String,
    speed: String,
    class: String,
    drivers: Vec<String>,
    authorized: bool,
    is_hub: bool,
}

const USB_SCRIPT: &str = r#"export LC_ALL=C
for d in /sys/bus/usb/devices/*; do [ -e $d/idVendor ] || continue; n=${d##*/}
 dr=$(for i in $d/$n:*; do [ -e $i/driver ] && basename $(readlink $i/driver); done | sort -u | tr '\n' ' ')
 echo "$n|$(cat $d/busnum)|$(cat $d/devnum)|$(cat $d/idVendor):$(cat $d/idProduct)|$(cat $d/manufacturer 2>/dev/null)|$(cat $d/product 2>/dev/null)|$(cat $d/speed)|$(cat $d/bDeviceClass)|$dr|$(cat $d/authorized)|$(cat $d/maxchild)"
done
echo '@@@LSUSB'
lsusb 2>/dev/null"#;

fn usb_speed(s: &str) -> String {
    match s.trim() {
        "1.5" => "1.5M".into(),
        "12" => "12M (USB1)".into(),
        "480" => "480M (USB2)".into(),
        "5000" => "5G (USB3)".into(),
        "10000" => "10G (USB3.1)".into(),
        "20000" => "20G (USB3.2)".into(),
        o if o.is_empty() => String::new(),
        o => format!("{o}M"),
    }
}

fn load_usb_host(exec: &Exec) -> anyhow::Result<Vec<UsbDev>> {
    let out = exec.output("sh", &["-c", USB_SCRIPT])?;
    let (sys, ls) = out.split_once("@@@LSUSB").unwrap_or((&out, ""));
    // lsusb names: "Bus 001 Device 002: ID 046d:c092 Logitech, Inc. G102 ..."
    let names: HashMap<String, String> = ls
        .lines()
        .filter_map(|l| {
            let id = l.split(" ID ").nth(1)?;
            let (vp, name) = id.split_once(' ').unwrap_or((id, ""));
            Some((vp.to_string(), name.trim().to_string()))
        })
        .collect();
    let mut v: Vec<UsbDev> = sys
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split('|').collect();
            if p.len() < 11 {
                return None;
            }
            let own = format!("{} {}", p[4], p[5]).trim().to_string();
            let name = if own.is_empty() { names.get(p[3]).cloned().unwrap_or_default() } else { own };
            Some(UsbDev {
                path: p[0].into(),
                bus: p[1].into(),
                dev: p[2].into(),
                id: p[3].into(),
                name,
                speed: usb_speed(p[6]),
                class: p[7].into(),
                drivers: p[8].split_whitespace().map(|s| s.to_string()).collect(),
                authorized: p[9].trim() == "1",
                is_hub: p[7] == "09" || p[10].trim().parse::<u32>().unwrap_or(0) > 0,
            })
        })
        .collect();
    v.sort_by(|a, b| a.bus.cmp(&b.bus).then(a.path.cmp(&b.path)));
    Ok(v)
}

fn load_usb_api(api: &crate::api::Api, node: &str) -> anyhow::Result<Vec<UsbDev>> {
    let v = api.get(&format!("/nodes/{node}/hardware/usb"))?;
    Ok(jarr(&v)
        .iter()
        .map(|u| UsbDev {
            path: js(u, "usbpath"),
            bus: js(u, "busnum"),
            dev: js(u, "devnum"),
            id: format!("{}:{}", js(u, "vendid").trim_start_matches("0x"), js(u, "prodid").trim_start_matches("0x")),
            name: format!("{} {}", js(u, "manufacturer"), js(u, "product")).trim().to_string(),
            speed: usb_speed(&js(u, "speed")),
            class: js(u, "class"),
            authorized: true,
            is_hub: js(u, "class") == "9",
            ..Default::default()
        })
        .collect())
}

#[derive(Default)]
pub struct Usb {
    data: Loader<Vec<UsbDev>>,
    table: Table,
    show_hubs: bool,
}

impl Usb {
    fn sel(&self) -> Option<&UsbDev> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.iter().find(|d| d.path == k)
    }
}

impl View for Usb {
    fn title(&self) -> &'static str {
        "USB"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let exec = ctx.exec();
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.data.load(move || match exec {
            Some(e) => load_usb_host(&e),
            None => load_usb_api(&api, &node),
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

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("f", "hubs")]
    }

    fn on_key(&mut self, key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        if key.code == KeyCode::Char('f') {
            self.show_hubs = !self.show_hubs;
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
            .filter(|d| self.show_hubs || !d.is_hub)
            .map(|d| {
                Row::new(d.path.clone())
                    .c(d.path.clone())
                    .c(d.id.clone())
                    .c(d.name.clone())
                    .c(d.speed.clone())
                    .c(d.drivers.join(" "))
                    .cs(if d.authorized { "on" } else { "off" }, if d.authorized { style_good() } else { style_bad() })
                    .dim(d.is_hub)
            })
            .collect();
        let cols = [
            col("Port", Constraint::Length(9)),
            col("ID", Constraint::Length(10)),
            col("Device", Constraint::Min(20)),
            col("Speed", Constraint::Length(12)),
            col("Driver", Constraint::Length(14)),
            col("State", Constraint::Length(5)),
        ];
        let title = if self.show_hubs { "USB (+hubs)" } else { "USB" };
        self.table.draw(f, area, title, &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(d) = self.sel() {
            kv.add("Port", d.path.clone());
            kv.add("Bus / device", format!("{} / {}", d.bus, d.dev));
            kv.add("ID", d.id.clone());
            kv.add("Device", d.name.clone());
            kv.add("Speed", d.speed.clone());
            kv.add("Class", d.class.clone());
            kv.add("Drivers", d.drivers.join(", "));
            kv.status("State", if d.authorized { "enabled" } else { "disabled" });
            kv.blank();
            kv.styled("", tr("Pass it to a VM by port (survives re-plugging) or by ID."), style_dim());
        }
        draw_lines(f, area, "Details", focused, kv.lines(), 0);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(d) = self.sel() else { return vec![] };
        let ops = ctx.need_root().and(ctx.need_exec("Sys.Modify").map(|_| ()));
        vec![
            item('R', "Reset (re-enumerate)").require(ops.clone()),
            item(if d.authorized { 'x' } else { 'e' }, if d.authorized { "Disable device" } else { "Enable device" }).require(ops),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(d) = self.sel().cloned() else { return };
        if !d.path.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == ':') {
            return;
        }
        match key {
            'R' => ctx.popup(Popup::confirm("Reset USB device", "reset", vec![format!("{} {}", d.path, d.name)], false)),
            'x' => ctx.popup(Popup::confirm("Disable USB device", "off", vec![format!("{} {}", d.path, d.name), tr("Disconnects it until enabled again or replugged.").into()], true)),
            'e' => self.on_confirm("on", ctx),
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        let (Some(d), Some(exec)) = (self.sel().cloned(), ctx.exec()) else { return };
        ctx.audit(&format!("usb {} {tag}", d.path));
        let script: &'static str = match tag {
            "reset" => "a=/sys/bus/usb/devices/$1/authorized; echo 0 > $a && sleep 1 && echo 1 > $a",
            "off" => "echo 0 > /sys/bus/usb/devices/$1/authorized",
            _ => "echo 1 > /sys/bus/usb/devices/$1/authorized",
        };
        run_sh(ctx, exec, format!("USB {} {tag}", d.path), script, vec![d.path]);
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

// ===========================================================================
// Hardware info: board / firmware, CPU, memory modules, sensors, embedded
// ===========================================================================

const INFO_SCRIPT: &str = r#"export LC_ALL=C
echo '@@@DMI'; for f in sys_vendor product_name product_version product_serial board_vendor board_name board_version bios_vendor bios_version bios_date chassis_type; do v=$(cat /sys/class/dmi/id/$f 2>/dev/null) && echo "$f=$v"; done
echo '@@@DT'; [ -r /proc/device-tree/model ] && { echo "model=$(tr -d '\0' < /proc/device-tree/model)"; echo "compatible=$(tr '\0' ' ' < /proc/device-tree/compatible)"; }
echo '@@@CPU'; lscpu 2>/dev/null
echo '@@@DIMM'; if command -v dmidecode >/dev/null; then dmidecode -t 17 2>/dev/null; else echo 'NO_DMIDECODE'; fi
echo '@@@HWMON'; for h in /sys/class/hwmon/hwmon*; do n=$(cat $h/name 2>/dev/null); for i in $h/temp*_input $h/fan*_input $h/in*_input $h/power*_input; do [ -e "$i" ] || continue; b=${i%_input}; l=$(cat ${b}_label 2>/dev/null); echo "$n|${b##*/}|$l|$(cat $i 2>/dev/null)|$(cat ${b}_crit 2>/dev/null)"; done; done
echo '@@@THERMAL'; for t in /sys/class/thermal/thermal_zone*; do [ -e $t/temp ] && echo "$(cat $t/type)|$(cat $t/temp)"; done
echo '@@@PLATFORM'; [ -r /proc/device-tree/model ] && for p in /sys/bus/platform/devices/*; do dr=; [ -e $p/driver ] && dr=$(basename $(readlink $p/driver)); echo "${p##*/}|$dr"; done
true"#;

#[derive(Default)]
pub struct HwInfo {
    data: Loader<HashMap<String, String>>,
    scroll: u16,
}

impl View for HwInfo {
    fn title(&self) -> &'static str {
        "Hardware"
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
            let out = exec.output("sh", &["-c", INFO_SCRIPT])?;
            let mut m: HashMap<String, String> = HashMap::new();
            let mut cur = String::new();
            for l in out.lines() {
                if let Some(n) = l.strip_prefix("@@@") {
                    cur = n.to_string();
                    continue;
                }
                let e = m.entry(cur.clone()).or_default();
                e.push_str(l);
                e.push('\n');
            }
            Ok(m)
        });
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(10)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some((m, st)) = loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none()) {
            kv.styled("", m, st);
        }
        if let Some(d) = &self.data.data {
            let sec = |n: &str| d.get(n).cloned().unwrap_or_default();
            let kvs = |n: &str| -> HashMap<String, String> { sec(n).lines().filter_map(|l| l.split_once('=').map(|(a, b)| (a.to_string(), b.trim().to_string()))).collect() };
            let dmi = kvs("DMI");
            if !dmi.is_empty() {
                kv.section("System");
                let g = |k: &str| dmi.get(k).cloned().unwrap_or_default();
                kv.add("Manufacturer", g("sys_vendor"));
                kv.add("Product", format!("{} {}", g("product_name"), g("product_version")).trim().to_string());
                if !g("product_serial").is_empty() {
                    kv.add("Serial", g("product_serial"));
                }
                kv.add("Board", format!("{} {} {}", g("board_vendor"), g("board_name"), g("board_version")).trim().to_string());
                kv.add("Firmware", format!("{} {} ({})", g("bios_vendor"), g("bios_version"), g("bios_date")));
            }
            let dt = kvs("DT");
            if !dt.is_empty() {
                kv.section("Device tree (embedded)");
                kv.add("Model", dt.get("model").cloned().unwrap_or_default());
                kv.add("Compatible", dt.get("compatible").cloned().unwrap_or_default());
            }
            let cpu: HashMap<String, String> = sec("CPU").lines().filter_map(|l| l.split_once(':').map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))).collect();
            if !cpu.is_empty() {
                kv.section("CPU");
                for (label, key) in [
                    ("Model", "Model name"),
                    ("Architecture", "Architecture"),
                    ("Sockets", "Socket(s)"),
                    ("Cores per socket", "Core(s) per socket"),
                    ("Threads per core", "Thread(s) per core"),
                    ("Max MHz", "CPU max MHz"),
                    ("L3 cache", "L3 cache"),
                    ("Virtualization", "Virtualization"),
                    ("NUMA nodes", "NUMA node(s)"),
                ] {
                    if let Some(v) = cpu.get(key) {
                        kv.add(label, v.clone());
                    }
                }
            }
            kv.section("Memory modules");
            let dimm = sec("DIMM");
            if dimm.contains("NO_DMIDECODE") {
                kv.styled("", tr("install dmidecode to see the memory modules"), style_dim());
            } else {
                for block in dimm.split("Memory Device").skip(1) {
                    let f: HashMap<String, String> = block.lines().filter_map(|l| l.trim().split_once(": ").map(|(a, b)| (a.to_string(), b.to_string()))).collect();
                    let g = |k: &str| f.get(k).cloned().unwrap_or_default();
                    let size = g("Size");
                    if size.is_empty() {
                        continue;
                    }
                    if size.starts_with("No Module") {
                        kv.styled(&g("Locator"), tr("empty").to_string(), style_dim());
                    } else {
                        kv.add(&g("Locator"), format!("{size} {} {} {} {}", g("Type"), g("Configured Memory Speed"), g("Manufacturer"), g("Part Number").trim()));
                    }
                }
            }
            let hw = sec("HWMON");
            let thermal = sec("THERMAL");
            if !hw.trim().is_empty() || !thermal.trim().is_empty() {
                kv.section("Sensors");
                for l in hw.lines() {
                    let p: Vec<&str> = l.split('|').collect();
                    if p.len() < 5 {
                        continue;
                    }
                    let raw: f64 = p[3].trim().parse().unwrap_or(0.0);
                    let crit: f64 = p[4].trim().parse().unwrap_or(0.0);
                    let label = format!("{} {}", p[0], if p[2].is_empty() { p[1] } else { p[2] });
                    let (val, st) = if p[1].starts_with("temp") {
                        let c = raw / 1000.0;
                        let st = if crit > 0.0 && c >= crit / 1000.0 - 5.0 { style_bad() } else if c >= 80.0 { style_warn() } else { Style::default() };
                        (format!("{c:.1} °C"), st)
                    } else if p[1].starts_with("fan") {
                        (format!("{raw:.0} RPM"), Style::default())
                    } else if p[1].starts_with("power") {
                        (format!("{:.1} W", raw / 1_000_000.0), Style::default())
                    } else {
                        (format!("{:.3} V", raw / 1000.0), Style::default())
                    };
                    kv.styled(&label, val, st);
                }
                for l in thermal.lines() {
                    if let Some((t, v)) = l.split_once('|') {
                        let c = v.trim().parse::<f64>().unwrap_or(0.0) / 1000.0;
                        kv.styled(&format!("zone {t}"), format!("{c:.1} °C"), if c >= 80.0 { style_warn() } else { Style::default() });
                    }
                }
            }
            let plat = sec("PLATFORM");
            if !plat.trim().is_empty() {
                kv.section("Platform devices");
                for l in plat.lines().take(200) {
                    if let Some((n, drv)) = l.split_once('|') {
                        kv.styled(n, if drv.is_empty() { "-".to_string() } else { drv.to_string() }, if drv.is_empty() { style_dim() } else { Style::default() });
                    }
                }
            }
        }
        draw_lines(f, area, "Hardware", focused, kv.lines(), self.scroll);
    }

    fn has_detail(&self) -> bool {
        false
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn on_key(&mut self, key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => scroll(&mut self.scroll, -1),
            KeyCode::Down | KeyCode::Char('j') => scroll(&mut self.scroll, 1),
            KeyCode::PageUp => scroll(&mut self.scroll, -10),
            KeyCode::PageDown => scroll(&mut self.scroll, 10),
            _ => return false,
        }
        true
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert_eq!(split_id("Ethernet controller [0200]"), ("Ethernet controller".into(), "0200".into()));
        assert_eq!(split_id("Device"), ("Device".into(), "".into()));
    }

    #[test]
    fn link() {
        let d = PciDev { link_cur: "2.5 GT/s PCIe".into(), link_max: "8.0 GT/s PCIe".into(), width_cur: "4".into(), width_max: "4".into(), ..Default::default() };
        assert!(d.degraded());
        assert_eq!(d.link_short(), "2.5GT x4");
    }

    #[test]
    fn addr() {
        assert!(valid_pci_addr("0000:01:00.0"));
        assert!(!valid_pci_addr("0000:01:00.0; rm"));
    }
}
