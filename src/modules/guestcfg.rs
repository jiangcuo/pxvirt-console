//! Guest configuration page (like the PVE web UI "Hardware" / "Options" panels).
//!
//! Every config item is a row and is edited with its own form. The data comes from
//! `/pending`, so values waiting for a reboot are shown next to the current ones, and every
//! write carries the config `digest`: a concurrent change by someone else is rejected by PVE
//! instead of being overwritten. Property strings (`virtio=..,bridge=vmbr0,tag=5`) are parsed,
//! only the fields shown in a form are changed and all other options are kept as they are.

use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    Frame,
};
use serde_json::Value;
use std::collections::BTreeMap;

use super::{fv, scroll};
use crate::ctx::{ActionOutcome, Ctx};
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::form::{self, Field, Form};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, style_dim, style_warn, Kv};
use crate::util::{fmt_bytes, jarr, jf, js};

// ===========================================================================
// Property strings
// ===========================================================================

/// A PVE property string: an optional positional first value plus `key=value` pairs (order kept).
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Prop {
    pub first: Option<String>,
    pub kv: Vec<(String, String)>,
}

impl Prop {
    pub fn parse(s: &str) -> Self {
        let mut p = Prop::default();
        for (i, part) in s.split(',').enumerate() {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match part.split_once('=') {
                Some((k, v)) => p.kv.push((k.trim().to_string(), v.trim().to_string())),
                None if i == 0 => p.first = Some(part.to_string()),
                None => p.kv.push((part.to_string(), String::new())),
            }
        }
        p
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }

    pub fn gets(&self, k: &str) -> String {
        self.get(k).unwrap_or("").to_string()
    }

    /// Set a value; an empty value removes the key.
    pub fn set(&mut self, k: &str, v: &str) {
        if v.is_empty() {
            self.kv.retain(|(a, _)| a != k);
        } else if let Some(e) = self.kv.iter_mut().find(|(a, _)| a == k) {
            e.1 = v.to_string();
        } else {
            self.kv.push((k.to_string(), v.to_string()));
        }
    }

    /// Boolean option: only written when it differs from the default.
    pub fn flag(&mut self, k: &str, on: bool, default_on: bool, on_val: &str, off_val: &str) {
        if on == default_on {
            self.kv.retain(|(a, _)| a != k);
        } else {
            self.set(k, if on { on_val } else { off_val });
        }
    }

    pub fn is_on(&self, k: &str, default_on: bool) -> bool {
        match self.get(k) {
            None => default_on,
            Some(v) => matches!(v, "1" | "on" | "yes" | "true"),
        }
    }

    pub fn remove(&mut self, k: &str) {
        self.kv.retain(|(a, _)| a != k);
    }

    pub fn render(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(f) = &self.first {
            parts.push(f.clone());
        }
        for (k, v) in &self.kv {
            parts.push(if v.is_empty() { k.clone() } else { format!("{k}={v}") });
        }
        parts.join(",")
    }
}

// ===========================================================================
// Data
// ===========================================================================

#[derive(Clone, Default)]
struct Entry {
    value: Option<String>,
    pending: Option<String>,
    delete: bool,
}

#[derive(Default)]
struct CfgData {
    e: BTreeMap<String, Entry>,
    digest: String,
    running: bool,
}

impl CfgData {
    fn cur(&self, k: &str) -> String {
        self.e.get(k).and_then(|e| e.value.clone()).unwrap_or_default()
    }

    /// Value after the pending changes are applied.
    fn eff(&self, k: &str) -> String {
        match self.e.get(k) {
            Some(e) if e.pending.is_some() => e.pending.clone().unwrap_or_default(),
            Some(e) if e.delete => String::new(),
            Some(e) => e.value.clone().unwrap_or_default(),
            None => String::new(),
        }
    }

    fn pend(&self, k: &str) -> bool {
        self.e.get(k).map(|e| e.pending.is_some() || e.delete).unwrap_or(false)
    }

    fn exists(&self, k: &str) -> bool {
        self.e.get(k).map(|e| e.value.is_some() || e.pending.is_some()).unwrap_or(false)
    }
}

/// Option lists for the forms (loaded once when the page opens).
#[derive(Default, Clone)]
struct Info {
    disk_stores: Vec<(String, String)>,
    ct_stores: Vec<(String, String)>,
    isos: Vec<(String, String)>,
    bridges: Vec<String>,
    cpus: Vec<String>,
    machines: Vec<String>,
    pci: Vec<(String, String)>,
    usb: Vec<(String, String)>,
    pci_maps: Vec<String>,
    usb_maps: Vec<String>,
}

/// Config with pending changes and digest, plus the run state.
fn fetch(api: &crate::api::Api, base: &str) -> anyhow::Result<CfgData> {
    let mut d = CfgData::default();
    for e in jarr(&api.get(&format!("{base}/pending"))?) {
        let k = js(&e, "key");
        let opt = |name: &str| e.get(name).filter(|v| !v.is_null()).map(|_| js(&e, name));
        let del = js(&e, "delete");
        let entry = Entry { value: opt("value"), pending: opt("pending"), delete: !del.is_empty() && del != "0" };
        if k == "digest" {
            d.digest = entry.value.clone().unwrap_or_default();
        }
        d.e.insert(k, entry);
    }
    let st = api.get(&format!("{base}/status/current")).unwrap_or(Value::Null);
    d.running = js(&st, "status") == "running";
    Ok(d)
}

fn load_info(api: &crate::api::Api, node: &str) -> Info {
    let mut i = Info::default();
    let get = |p: &str| api.get(p).map(|v| jarr(&v)).unwrap_or_default();
    for s in get(&format!("/nodes/{node}/storage?enabled=1")) {
        let id = js(&s, "storage");
        let content = js(&s, "content");
        let label = format!("{id} ({} {} free)", js(&s, "type"), fmt_bytes(jf(&s, "avail")));
        if content.contains("images") {
            i.disk_stores.push((id.clone(), label.clone()));
        }
        if content.contains("rootdir") {
            i.ct_stores.push((id.clone(), label.clone()));
        }
        if content.contains("iso") {
            for c in get(&format!("/nodes/{node}/storage/{id}/content?content=iso")) {
                let vol = js(&c, "volid");
                let short = vol.rsplit('/').next().unwrap_or(&vol).to_string();
                i.isos.push((vol, format!("{short}  ({id}, {})", fmt_bytes(jf(&c, "size")))));
            }
        }
    }
    i.bridges = get(&format!("/nodes/{node}/network?type=any_bridge")).iter().map(|b| js(b, "iface")).collect();
    i.bridges.sort();
    i.cpus = get(&format!("/nodes/{node}/capabilities/qemu/cpu")).iter().map(|c| js(c, "name")).collect();
    i.machines = get(&format!("/nodes/{node}/capabilities/qemu/machines")).iter().map(|m| js(m, "id")).collect();
    for d in get(&format!("/nodes/{node}/hardware/pci")) {
        let id = js(&d, "id");
        i.pci.push((id.clone(), format!("{id}  {} {}  (IOMMU {})", js(&d, "vendor_name"), js(&d, "device_name"), js(&d, "iommugroup"))));
    }
    for d in get(&format!("/nodes/{node}/hardware/usb")) {
        if js(&d, "class") == "9" {
            continue; // hubs
        }
        let (v, p) = (js(&d, "vendid").trim_start_matches("0x").to_string(), js(&d, "prodid").trim_start_matches("0x").to_string());
        if v.is_empty() {
            continue;
        }
        i.usb.push((format!("host={v}:{p}"), format!("{v}:{p}  {} {}", js(&d, "manufacturer"), js(&d, "product"))));
    }
    i.pci_maps = get("/cluster/mapping/pci").iter().map(|m| js(m, "id")).collect();
    i.usb_maps = get("/cluster/mapping/usb").iter().map(|m| js(m, "id")).collect();
    i
}

// ===========================================================================
// Item catalogue
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Debug)]
enum K {
    Mem,
    Cpu,
    CtMem,
    CtCpu,
    Bios,
    Vga,
    Machine,
    Scsihw,
    Disk,
    Cd,
    Efi,
    Tpm,
    Net,
    CtNet,
    Usb,
    Pci,
    Serial,
    Unused,
    Rootfs,
    Mp,
    Opt,
    Name,
    Hostname,
    Dns,
    Features,
    Cloud,
    Other,
    ReadOnly,
}

const MEM_KEYS: [&str; 3] = ["memory", "balloon", "shares"];
const CPU_KEYS: [&str; 8] = ["sockets", "cores", "cpu", "numa", "vcpus", "cpulimit", "cpuunits", "affinity"];
const CTMEM_KEYS: [&str; 2] = ["memory", "swap"];
const CTCPU_KEYS: [&str; 3] = ["cores", "cpulimit", "cpuunits"];
const NET_MODELS: [&str; 10] = ["virtio", "e1000", "e1000e", "rtl8139", "vmxnet3", "ne2k_pci", "pcnet", "i82551", "i82557b", "i82559er"];
const HIDDEN: [&str; 1] = ["digest"];

/// Options with their labels (QEMU).
const VM_OPTS: [(&str, &str); 15] = [
    ("name", "Name"),
    ("onboot", "Start at boot"),
    ("startup", "Start/shutdown order"),
    ("ostype", "OS type"),
    ("boot", "Boot order"),
    ("tablet", "Use tablet for pointer"),
    ("hotplug", "Hotplug"),
    ("acpi", "ACPI support"),
    ("kvm", "KVM hardware virtualization"),
    ("freeze", "Freeze CPU at startup"),
    ("localtime", "Use local time for RTC"),
    ("agent", "QEMU guest agent"),
    ("protection", "Protection"),
    ("tags", "Tags"),
    ("description", "Notes"),
];

const CT_OPTS: [(&str, &str); 12] = [
    ("onboot", "Start at boot"),
    ("startup", "Start/shutdown order"),
    ("ostype", "OS type"),
    ("arch", "Architecture"),
    ("unprivileged", "Unprivileged container"),
    ("features", "Features"),
    ("protection", "Protection"),
    ("console", "Console"),
    ("tty", "TTY count"),
    ("cmode", "Console mode"),
    ("tags", "Tags"),
    ("description", "Notes"),
];

const CLOUD_KEYS: [&str; 8] = ["ciuser", "cipassword", "citype", "ciupgrade", "sshkeys", "nameserver", "searchdomain", "cicustom"];

/// Boolean options: (key, default on).
const BOOL_OPTS: [(&str, bool); 8] = [
    ("onboot", false),
    ("tablet", true),
    ("acpi", true),
    ("kvm", true),
    ("freeze", false),
    ("localtime", false),
    ("protection", false),
    ("console", true),
];

fn split_key(k: &str) -> (&str, Option<u32>) {
    let pos = k.find(|c: char| c.is_ascii_digit()).unwrap_or(k.len());
    (&k[..pos], k[pos..].parse().ok())
}

fn is_dev(k: &str, prefixes: &[&str]) -> bool {
    let (p, n) = split_key(k);
    n.is_some() && prefixes.contains(&p)
}

fn is_cd(v: &str) -> bool {
    Prop::parse(v).get("media") == Some("cdrom")
}

const DISK_BUS: [&str; 4] = ["ide", "sata", "scsi", "virtio"];

fn bus_max(bus: &str) -> u32 {
    match bus {
        "ide" => 4,
        "sata" => 6,
        "scsi" => 31,
        "virtio" => 16,
        "net" => 32,
        "usb" => 5,
        "hostpci" => 16,
        "serial" => 4,
        "mp" => 256,
        _ => 10,
    }
}

/// Sort rank of device keys (bus order, then index).
fn dev_rank(k: &str) -> (u32, u32) {
    let (p, n) = split_key(k);
    let r = match p {
        "efidisk" => 0,
        "tpmstate" => 1,
        "rootfs" => 2,
        "mp" => 3,
        "ide" => 4,
        "sata" => 5,
        "scsi" => 6,
        "virtio" => 7,
        "net" => 8,
        "usb" => 9,
        "hostpci" => 10,
        "serial" => 11,
        "unused" => 13,
        _ => 12,
    };
    (r, n.unwrap_or(0))
}

struct Item {
    key: String,
    group: &'static str,
    label: String,
    kind: K,
}

fn vol_store(v: &str) -> String {
    Prop::parse(v).first.unwrap_or_default().split_once(':').map(|x| x.0.to_string()).unwrap_or_default()
}

fn mib(v: &str) -> String {
    match v.parse::<f64>() {
        Ok(m) => fmt_bytes(m * 1024.0 * 1024.0),
        Err(_) => v.to_string(),
    }
}

fn yes_no(on: bool) -> String {
    tr(if on { "yes" } else { "no" }).to_string()
}

// ===========================================================================
// Page
// ===========================================================================

pub struct GuestCfg {
    node: String,
    vmid: String,
    qemu: bool,
    name: String,
    data: Loader<CfgData>,
    info: Loader<Info>,
    /// action waiting for the option lists
    want: Option<char>,
    table: Table,
    scroll: u16,
    /// tests: writes are recorded instead of sent
    #[cfg(test)]
    sent: std::cell::RefCell<Vec<(String, String, Vec<(String, String)>)>>,
}

impl GuestCfg {
    pub fn new(g: &Value, ctx: &Ctx) -> Self {
        let mut s = Self {
            node: js(g, "node"),
            vmid: js(g, "vmid"),
            qemu: js(g, "type") == "qemu",
            name: crate::util::guest_name(&js(g, "name")),
            data: Loader::default(),
            info: Loader::default(),
            want: None,
            table: Table::default(),
            scroll: 0,
            #[cfg(test)]
            sent: Default::default(),
        };
        s.load(ctx);
        let api = ctx.api.clone();
        let node = s.node.clone();
        s.info.load(move || Ok(load_info(&api, &node)));
        s
    }

    fn base(&self) -> String {
        format!("/nodes/{}/{}/{}", self.node, if self.qemu { "qemu" } else { "lxc" }, self.vmid)
    }

    fn path(&self) -> String {
        format!("/vms/{}", self.vmid)
    }

    fn load(&mut self, ctx: &Ctx) {
        let api = ctx.api.clone();
        let base = self.base();
        self.data.load(move || fetch(&api, &base));
    }

    fn d(&self) -> Option<&CfgData> {
        self.data.data.as_ref()
    }

    fn info(&self) -> Info {
        self.info.data.clone().unwrap_or_default()
    }

    // ---- items ----

    fn items(&self) -> Vec<Item> {
        let Some(d) = self.d() else { return vec![] };
        let mut v: Vec<Item> = Vec::new();
        let mut used: Vec<&str> = HIDDEN.to_vec();
        let push = |v: &mut Vec<Item>, key: &str, group: &'static str, label: &str, kind: K| v.push(Item { key: key.into(), group, label: label.into(), kind });
        let mut devs: Vec<&String> = d.e.keys().filter(|k| d.exists(k)).collect();
        devs.sort_by_key(|k| dev_rank(k));
        if self.qemu {
            push(&mut v, "@mem", "VM hardware", "Memory", K::Mem);
            push(&mut v, "@cpu", "VM hardware", "Processors", K::Cpu);
            push(&mut v, "bios", "VM hardware", "BIOS", K::Bios);
            push(&mut v, "vga", "VM hardware", "Display", K::Vga);
            push(&mut v, "machine", "VM hardware", "Machine", K::Machine);
            push(&mut v, "scsihw", "VM hardware", "SCSI controller", K::Scsihw);
            used.extend(MEM_KEYS);
            used.extend(CPU_KEYS);
            used.extend(["bios", "vga", "machine", "scsihw"]);
            for k in &devs {
                let kind = match split_key(k) {
                    (p, Some(_)) if DISK_BUS.contains(&p) => {
                        if is_cd(&d.eff(k)) || is_cd(&d.cur(k)) {
                            K::Cd
                        } else {
                            K::Disk
                        }
                    }
                    ("efidisk", Some(_)) => K::Efi,
                    ("tpmstate", Some(_)) => K::Tpm,
                    ("net", Some(_)) => K::Net,
                    ("usb", Some(_)) => K::Usb,
                    ("hostpci", Some(_)) => K::Pci,
                    ("serial", Some(_)) => K::Serial,
                    ("unused", Some(_)) => K::Unused,
                    ("audio" | "rng" | "parallel" | "virtiofs", Some(_)) => K::Other,
                    _ => continue,
                };
                push(&mut v, k, "VM hardware", k, kind);
                used.push(k.as_str());
            }
            for (k, label) in VM_OPTS {
                let kind = match k {
                    "name" => K::Name,
                    _ => K::Opt,
                };
                push(&mut v, k, "Options", label, kind);
                used.push(k);
            }
            for k in d.e.keys().filter(|k| d.exists(k)) {
                if CLOUD_KEYS.contains(&k.as_str()) || is_dev(k, &["ipconfig"]) {
                    push(&mut v, k, "Cloud-Init", k, K::Cloud);
                    used.push(k.as_str());
                }
            }
        } else {
            push(&mut v, "@ctmem", "Resources", "Memory", K::CtMem);
            push(&mut v, "@ctcpu", "Resources", "Cores", K::CtCpu);
            used.extend(CTMEM_KEYS);
            used.extend(CTCPU_KEYS);
            for k in &devs {
                let (kind, group) = match split_key(k) {
                    ("rootfs", None) => (K::Rootfs, "Resources"),
                    ("mp", Some(_)) => (K::Mp, "Resources"),
                    ("unused", Some(_)) => (K::Unused, "Resources"),
                    ("net", Some(_)) => (K::CtNet, "Network"),
                    _ => continue,
                };
                push(&mut v, k, group, k, kind);
                used.push(k.as_str());
            }
            push(&mut v, "hostname", "DNS", "Hostname", K::Hostname);
            push(&mut v, "@dns", "DNS", "DNS servers", K::Dns);
            used.extend(["hostname", "nameserver", "searchdomain"]);
            for (k, label) in CT_OPTS {
                let kind = match k {
                    "features" => K::Features,
                    "ostype" | "arch" | "unprivileged" => K::ReadOnly,
                    _ => K::Opt,
                };
                push(&mut v, k, "Options", label, kind);
                used.push(k);
            }
        }
        for k in d.e.keys().filter(|k| d.exists(k) && !used.contains(&k.as_str())) {
            let kind = if ["meta", "vmgenid", "lock"].contains(&k.as_str()) { K::ReadOnly } else { K::Other };
            push(&mut v, k, "Other", k, kind);
        }
        v
    }

    fn keys_of(&self, it: &Item) -> Vec<String> {
        match it.kind {
            K::Mem => MEM_KEYS.iter().map(|s| s.to_string()).collect(),
            K::Cpu => CPU_KEYS.iter().map(|s| s.to_string()).collect(),
            K::CtMem => CTMEM_KEYS.iter().map(|s| s.to_string()).collect(),
            K::CtCpu => CTCPU_KEYS.iter().map(|s| s.to_string()).collect(),
            K::Dns => vec!["nameserver".into(), "searchdomain".into()],
            _ => vec![it.key.clone()],
        }
    }

    fn pending(&self, it: &Item) -> bool {
        let Some(d) = self.d() else { return false };
        self.keys_of(it).iter().any(|k| d.pend(k))
    }

    /// One-line summary of an item, from the current (`eff = false`) or pending values.
    fn summary(&self, it: &Item, eff: bool) -> String {
        let Some(d) = self.d() else { return String::new() };
        let g = |k: &str| if eff { d.eff(k) } else { d.cur(k) };
        let or = |v: String, def: &str| if v.is_empty() { tr(def).to_string() } else { v };
        match it.kind {
            K::Mem => {
                let m = or(g("memory"), "512");
                let b = g("balloon");
                let mut s = mib(&m);
                if b == "0" {
                    s.push_str(&format!("  ({})", tr("ballooning off")));
                } else if !b.is_empty() && b != m {
                    s.push_str(&format!("  ({} {})", tr("min"), mib(&b)));
                }
                s
            }
            K::Cpu => {
                let so: u32 = g("sockets").parse().unwrap_or(1);
                let co: u32 = g("cores").parse().unwrap_or(1);
                let cpu = Prop::parse(&g("cpu"));
                let ty = cpu.first.clone().or(cpu.get("cputype").map(|s| s.to_string())).unwrap_or_else(|| "default (kvm64/x86-64-v2-AES)".into());
                let mut s = format!("{so} × {co} = {} vCPU  {ty}", so * co);
                if g("numa") == "1" {
                    s.push_str("  NUMA");
                }
                if !g("cpulimit").is_empty() {
                    s.push_str(&format!("  limit {}", g("cpulimit")));
                }
                s
            }
            K::CtMem => format!("{}  swap {}", mib(&or(g("memory"), "512")), mib(&or(g("swap"), "512"))),
            K::CtCpu => {
                let c = g("cores");
                let mut s = if c.is_empty() { tr("all host cores").to_string() } else { c };
                if !g("cpulimit").is_empty() {
                    s.push_str(&format!("  limit {}", g("cpulimit")));
                }
                s
            }
            K::Bios => or(g("bios"), "seabios (default)"),
            K::Vga => or(g("vga"), "default"),
            K::Machine => or(g("machine"), "default (i440fx)"),
            K::Scsihw => or(g("scsihw"), "lsi (default)"),
            K::Disk | K::Efi | K::Tpm | K::Rootfs | K::Mp => {
                let v = g(&it.key);
                if v.is_empty() {
                    return tr("(removed)").to_string();
                }
                let p = Prop::parse(&v);
                let mut s = p.first.clone().unwrap_or_default();
                for k in ["size", "mp", "efitype", "version"] {
                    if let Some(x) = p.get(k) {
                        s.push_str(&format!("  {x}"));
                    }
                }
                for k in ["iothread", "ssd", "ro"] {
                    if p.is_on(k, false) {
                        s.push_str(&format!("  {k}"));
                    }
                }
                if p.get("discard") == Some("on") {
                    s.push_str("  discard");
                }
                if let Some(c) = p.get("cache") {
                    s.push_str(&format!("  cache={c}"));
                }
                if !p.is_on("backup", true) {
                    s.push_str(&format!("  {}", tr("no backup")));
                }
                s
            }
            K::Cd => {
                let v = g(&it.key);
                if v.is_empty() {
                    return tr("(removed)").to_string();
                }
                let f = Prop::parse(&v).first.unwrap_or_default();
                match f.as_str() {
                    "none" | "" => format!("CD/DVD  {}", tr("(empty)")),
                    "cdrom" => format!("CD/DVD  {}", tr("physical drive")),
                    other => format!("CD/DVD  {}", other.rsplit('/').next().unwrap_or(other)),
                }
            }
            K::Net | K::CtNet => {
                let v = g(&it.key);
                if v.is_empty() {
                    return tr("(removed)").to_string();
                }
                let p = Prop::parse(&v);
                let mut s = String::new();
                if it.kind == K::Net {
                    if let Some((m, mac)) = p.kv.iter().find(|(k, _)| NET_MODELS.contains(&k.as_str())) {
                        s = format!("{m}  {mac}");
                    }
                } else {
                    s = format!("{}  {}", p.gets("name"), p.gets("hwaddr"));
                }
                for k in ["bridge", "ip", "ip6"] {
                    if let Some(x) = p.get(k) {
                        s.push_str(&format!("  {x}"));
                    }
                }
                if let Some(t) = p.get("tag") {
                    s.push_str(&format!("  VLAN {t}"));
                }
                if p.is_on("firewall", false) {
                    s.push_str("  fw");
                }
                if p.is_on("link_down", false) {
                    s.push_str(&format!("  {}", tr("disconnected")));
                }
                s
            }
            K::Dns => {
                let (ns, sd) = (g("nameserver"), g("searchdomain"));
                if ns.is_empty() && sd.is_empty() {
                    tr("use host settings").to_string()
                } else {
                    format!("{ns}  {sd}")
                }
            }
            K::Name | K::Hostname => crate::util::guest_name(&g(&it.key)),
            K::Opt | K::Features | K::ReadOnly | K::Cloud | K::Other | K::Usb | K::Pci | K::Serial | K::Unused => {
                let v = g(&it.key);
                if let Some((_, def)) = BOOL_OPTS.iter().find(|(k, _)| *k == it.key) {
                    return if v.is_empty() { format!("{} ({})", yes_no(*def), tr("default")) } else { yes_no(v == "1") };
                }
                match it.key.as_str() {
                    "description" => v.lines().next().unwrap_or("").to_string(),
                    "cipassword" if !v.is_empty() => "********".into(),
                    "boot" => Prop::parse(&v).get("order").map(|o| o.replace(';', ", ")).unwrap_or_else(|| or(v.trim().to_string(), "default")),
                    "hotplug" => or(v, "network, disk, usb (default)"),
                    "agent" => {
                        let p = Prop::parse(&v);
                        let on = p.first.as_deref() == Some("1") || p.is_on("enabled", false);
                        yes_no(on)
                    }
                    _ if it.kind == K::Unused => format!("{}  {v}", tr("unused disk")),
                    _ => or(v, "-"),
                }
            }
        }
    }

    fn sel_item(&self) -> Option<Item> {
        let k = self.table.selected()?.to_string();
        self.items().into_iter().find(|i| i.key == k)
    }

    fn privilege(&self, kind: K) -> &'static str {
        match kind {
            K::Mem | K::CtMem => "VM.Config.Memory",
            K::Cpu | K::CtCpu => "VM.Config.CPU",
            K::Disk | K::Efi | K::Tpm | K::Unused | K::Rootfs | K::Mp => "VM.Config.Disk",
            K::Cd => "VM.Config.CDROM",
            K::Net | K::CtNet => "VM.Config.Network",
            K::Bios | K::Vga | K::Machine | K::Scsihw | K::Usb | K::Pci | K::Serial => "VM.Config.HWType",
            K::Cloud => "VM.Config.Cloudinit",
            _ => "VM.Config.Options",
        }
    }

    fn free_slot(&self, bus: &str) -> Option<String> {
        let d = self.d()?;
        (0..bus_max(bus)).map(|i| format!("{bus}{i}")).find(|k| !d.exists(k))
    }

    // ---- writes ----

    /// Write config changes. `alloc`: QEMU disk allocation runs as a task (POST).
    fn write(&self, ctx: &mut Ctx, title: String, mut set: Vec<(String, String)>, del: Vec<String>, alloc: bool) {
        if !del.is_empty() {
            set.push(("delete".into(), del.join(",")));
        }
        if let Some(d) = self.d() {
            if !d.digest.is_empty() {
                set.push(("digest".into(), d.digest.clone()));
            }
        }
        let method = if self.qemu && alloc { "POST" } else { "PUT" };
        self.call(ctx, title, method, format!("{}/config", self.base()), set);
    }

    fn call(&self, ctx: &mut Ctx, title: String, method: &'static str, path: String, p: Vec<(String, String)>) {
        #[cfg(test)]
        {
            let _ = (ctx, title);
            self.sent.borrow_mut().push((method.to_string(), path, p));
            return;
        }
        #[allow(unreachable_code)]
        let api = ctx.api.clone();
        let t2 = title.clone();
        ctx.audit(&format!("{title} ({path})"));
        ctx.act(title, move || {
            let pp: Vec<(&str, String)> = p.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
            let v = match method {
                "POST" => api.post(&path, &pp)?,
                "PUT" => api.put(&path, &pp)?,
                _ => api.delete(&path, &pp)?,
            };
            Ok(match crate::api::is_upid(&v) {
                Some(u) => ActionOutcome::Upid(u),
                None => ActionOutcome::Msg(format!("{t2}: OK")),
            })
        });
    }

    fn id(&self) -> String {
        format!("{} {}", if self.qemu { "VM" } else { "CT" }, self.vmid)
    }
}

// ===========================================================================
// View-like interface (driven by the Guests page)
// ===========================================================================

impl GuestCfg {
    pub fn refresh(&mut self, ctx: &mut Ctx) {
        self.load(ctx);
    }

    pub fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let ch = self.data.poll();
        if self.info.poll() {
            if let Some(k) = self.want.take() {
                self.on_menu("ctx", k, ctx);
            }
            return true;
        }
        ch
    }

    pub fn table(&mut self) -> &mut Table {
        &mut self.table
    }

    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("Esc", "back"), ("e", "edit"), ("+", "add")]
    }

    /// Esc is handled by the Guests page (closes this page); Ctrl+R reloads via refresh().
    pub fn on_key(&mut self, _key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        false
    }

    pub fn menu_title(&self) -> String {
        match self.sel_item() {
            Some(it) => format!("{} · {}", self.id(), tr(&it.label)),
            None => self.id(),
        }
    }

    pub fn draw_list(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        let items = self.items();
        let mut last = "";
        let rows: Vec<Row> = items
            .iter()
            .map(|it| {
                let g = if it.group != last { tr(it.group) } else { "" };
                last = it.group;
                let cur = self.summary(it, false);
                let pend = self.pending(it);
                let val = if pend { format!("{cur}  →  {}", self.summary(it, true)) } else { cur };
                Row::new(it.key.clone())
                    .cs(g, Style::default().fg(crate::ui::ACCENT))
                    .c(tr(&it.label))
                    .cs(val, if pend { style_warn() } else if it.kind == K::ReadOnly { style_dim() } else { Style::default() })
            })
            .collect();
        let cols = [col("", Constraint::Length(10)), col("Item", Constraint::Length(22)), col("Value", Constraint::Min(20))];
        let title = format!("{} {} · {} (Esc = {})", self.id(), self.name, tr("Configuration"), tr("back"));
        self.table.draw(f, area, &title, &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none()));
    }

    pub fn draw_detail(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        let mut kv = Kv::new();
        if let (Some(it), Some(d)) = (self.sel_item(), self.d()) {
            kv.section(&it.label);
            for k in self.keys_of(&it) {
                let (cur, eff) = (d.cur(&k), d.eff(&k));
                if cur.is_empty() && eff.is_empty() {
                    continue;
                }
                let multi = self.keys_of(&it).len() > 1;
                if k == "description" || k == "cipassword" || it.kind == K::ReadOnly && k == "meta" {
                    kv.add(&k, if k == "cipassword" { "********".into() } else { cur.clone() });
                } else if multi {
                    kv.add(&k, cur.clone());
                } else {
                    let p = Prop::parse(&cur);
                    if p.kv.is_empty() || it.kind == K::Other || it.kind == K::Cloud {
                        kv.add("value", cur.clone());
                    } else {
                        if let Some(first) = &p.first {
                            kv.add("value", first.clone());
                        }
                        for (a, b) in &p.kv {
                            kv.add(a, b.clone());
                        }
                    }
                }
                if d.pend(&k) {
                    kv.styled(&format!("→ {k}"), if eff.is_empty() { tr("(will be removed)").to_string() } else { eff.clone() }, style_warn());
                }
            }
            if kv.items.len() <= 1 {
                kv.styled("", tr("default values"), style_dim());
            }
            if self.pending(&it) {
                kv.blank();
                kv.styled("", tr(if d.running { "Pending: applied when the guest is restarted (u = revert)" } else { "Pending: applied at the next start (u = revert)" }), style_warn());
            }
            if it.kind == K::ReadOnly {
                kv.blank();
                kv.styled("", tr("read-only"), style_dim());
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    pub fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    pub fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let path = self.path();
        let mut v = Vec::new();
        let it = self.sel_item();
        if let Some(it) = &it {
            let p = ctx.need_priv(&path, self.privilege(it.kind));
            let k = it.kind;
            let editable = !matches!(k, K::ReadOnly);
            v.push(item('e', "Edit").require(p.clone()).disabled_if(!editable, "read-only"));
            if matches!(k, K::Disk | K::Rootfs | K::Mp) {
                v.push(item('R', "Resize disk").require(p.clone()).disabled_if(k == K::Mp && vol_store(&self.d().map(|d| d.eff(&it.key)).unwrap_or_default()).is_empty(), "bind mount"));
            }
            if matches!(k, K::Disk | K::Efi | K::Tpm | K::Rootfs | K::Mp) {
                v.push(item('M', "Move disk").require(p.clone()));
            }
            if k == K::Cd {
                v.push(item('i', "Eject").require(p.clone()));
            }
            let removable = matches!(k, K::Disk | K::Cd | K::Efi | K::Tpm | K::Net | K::CtNet | K::Usb | K::Pci | K::Serial | K::Unused | K::Mp | K::Cloud | K::Other)
                || (k == K::Opt && self.d().map(|d| !d.cur(&it.key).is_empty()).unwrap_or(false));
            if removable {
                let label = match k {
                    K::Disk | K::Mp => "Detach disk",
                    K::Unused => "Delete volume",
                    K::Opt | K::Cloud | K::Other => "Reset to default",
                    _ => "Remove",
                };
                v.push(item(if k == K::Unused { 'X' } else { 'x' }, label).require(p.clone()));
            }
            v.push(item('u', "Revert pending change").require(p.clone()).disabled_if(!self.pending(it), "nothing pending"));
            v.push(item('E', "Edit raw value").require(p).disabled_if(it.key.starts_with('@'), "composite item"));
        }
        v.push(item('+', "Add device").require(ctx.need_priv(&path, if self.qemu { "VM.Config.HWType" } else { "VM.Config.Disk" })));
        if self.qemu {
            v.push(item('o', "Boot order").require(ctx.need_priv(&path, "VM.Config.Options")));
        }
        v.push(item('A', "Add raw key").require(ctx.need_priv(&path, "VM.Config.Options")));
        v
    }

    fn add_menu(&self, ctx: &Ctx) -> Popup {
        let path = self.path();
        let disk = ctx.need_priv(&path, "VM.Config.Disk");
        let items = if self.qemu {
            let d = self.d();
            let has = |k: &str| d.map(|d| d.exists(k)).unwrap_or(false);
            vec![
                item('d', "Hard disk").require(disk.clone()),
                item('c', "CD/DVD drive").require(ctx.need_priv(&path, "VM.Config.CDROM")),
                item('n', "Network device").require(ctx.need_priv(&path, "VM.Config.Network")),
                item('e', "EFI disk").require(disk.clone()).disabled_if(has("efidisk0"), "already present"),
                item('t', "TPM state").require(disk).disabled_if(has("tpmstate0"), "already present"),
                item('u', "USB device").require(ctx.need_priv(&path, "VM.Config.HWType")),
                item('p', "PCI device").require(ctx.need_root()),
                item('s', "Serial port").require(ctx.need_priv(&path, "VM.Config.HWType")),
            ]
        } else {
            vec![item('m', "Mount point").require(disk), item('n', "Network device").require(ctx.need_priv(&path, "VM.Config.Network"))]
        };
        Popup::menu(&format!("{} · {}", self.id(), tr("Add device")), "gc:add", items)
    }

    pub fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        // forms with option lists wait for them
        // forms with option lists (storages, bridges, ISOs...) wait until they are loaded
        let needs_info = tag == "gc:add" || matches!(key, 'e' | 'M' | 'o' | '+');
        if needs_info && self.info.data.is_none() {
            // from the add menu: reopen the add menu once the lists are there
            self.want = Some(if tag == "gc:add" { '+' } else { key });
            ctx.toast(tr("Loading..."));
            return;
        }
        if tag == "gc:add" {
            self.add_form(key, ctx);
            return;
        }
        match key {
            '+' => ctx.popup(self.add_menu(ctx)),
            'o' => {
                if let Some(f) = self.boot_form() {
                    ctx.popup(Popup::Form(f));
                }
            }
            'A' => ctx.popup(Popup::Form(Form::new(
                "Add raw key",
                "gc:rawadd",
                vec![form::text("key", "Key", "").req(true), form::text("value", "Value", "").req(true)],
            ))),
            _ => {
                let Some(it) = self.sel_item() else { return };
                match key {
                    'e' => self.edit_form(&it, ctx),
                    'E' => {
                        let v = self.d().map(|d| d.eff(&it.key)).unwrap_or_default();
                        ctx.popup(Popup::Form(Form::new(
                            &format!("{} {}", tr("Edit raw value"), it.key),
                            &format!("gc:raw:{}", it.key),
                            vec![form::text("value", &it.key, &v).help("empty = delete the key")],
                        )));
                    }
                    'R' => {
                        let size = Prop::parse(&self.d().map(|d| d.eff(&it.key)).unwrap_or_default()).gets("size");
                        ctx.popup(Popup::Form(Form::new(
                            &format!("{} {}", tr("Resize disk"), it.key),
                            &format!("gc:resize:{}", it.key),
                            vec![form::info("Current size", &size), form::number("add", "Add (GiB)", "8").help("disks can only grow")],
                        )));
                    }
                    'M' => {
                        let info = self.info();
                        let stores = if self.qemu { info.disk_stores } else { info.ct_stores };
                        let mut fields = vec![form::info("Disk", &it.key), form::select("storage", "Target storage", stores, "")];
                        if self.qemu {
                            fields.push(form::select("format", "Format", fmt_opts(), ""));
                        }
                        fields.push(form::check("delete", "Delete source", true));
                        ctx.popup(Popup::Form(Form::new(&format!("{} {}", tr("Move disk"), it.key), &format!("gc:move:{}", it.key), fields)));
                    }
                    'i' => {
                        let mut p = Prop::parse(&self.d().map(|d| d.eff(&it.key)).unwrap_or_default());
                        p.first = Some("none".into());
                        p.set("media", "cdrom");
                        self.write(ctx, format!("Eject {}", it.key), vec![(it.key.clone(), p.render())], vec![], false);
                    }
                    'x' | 'X' => {
                        let body = match it.kind {
                            K::Disk | K::Mp => vec![format!("Detach {} from {}?", it.key, self.id()), "The volume is kept as an unused disk.".into()],
                            K::Opt | K::Cloud | K::Other => vec![format!("Reset {} of {} to the default?", it.key, self.id())],
                            _ => vec![format!("Remove {} from {}?", it.key, self.id())],
                        };
                        if it.kind == K::Unused {
                            let vol = self.d().map(|d| d.eff(&it.key)).unwrap_or_default();
                            ctx.popup(Popup::confirm_typed(
                                "Delete volume",
                                &format!("gc:del:{}", it.key),
                                vec![format!("Destroy {vol} permanently?"), "The data on this volume is lost.".into()],
                                &it.key,
                            ));
                        } else {
                            ctx.popup(Popup::confirm("Remove", &format!("gc:del:{}", it.key), body, matches!(it.kind, K::Disk | K::Mp | K::Efi | K::Tpm)));
                        }
                    }
                    'u' => {
                        let keys: Vec<String> = self.keys_of(&it).into_iter().filter(|k| self.d().map(|d| d.pend(k)).unwrap_or(false)).collect();
                        if !keys.is_empty() {
                            self.write(ctx, format!("Revert {}", keys.join(",")), vec![("revert".into(), keys.join(","))], vec![], false);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if let Some(k) = tag.strip_prefix("gc:del:") {
            self.write(ctx, format!("Remove {k}"), vec![], vec![k.to_string()], false);
        }
    }

    pub fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let Some(rest) = tag.strip_prefix("gc:") else { return };
        let (op, key) = rest.split_once(':').unwrap_or((rest, ""));
        let key = key.to_string();
        let eff = |k: &str| self.d().map(|d| d.eff(k)).unwrap_or_default();
        let title = |what: &str| format!("{} {}: {what}", tr("Edit"), self.id());
        match op {
            "mem" => {
                let mut set = vec![("memory".to_string(), fv(v, "memory").to_string())];
                let mut del = vec![];
                for k in ["balloon", "shares"] {
                    if fv(v, k).is_empty() {
                        if !eff(k).is_empty() {
                            del.push(k.to_string());
                        }
                    } else {
                        set.push((k.into(), fv(v, k).into()));
                    }
                }
                self.write(ctx, title("memory"), set, del, false);
            }
            "cpu" => {
                let mut cpu = Prop::parse(&eff("cpu"));
                let ty = fv(v, "cputype");
                if cpu.get("cputype").is_some() {
                    cpu.set("cputype", ty);
                } else {
                    cpu.first = if ty.is_empty() { None } else { Some(ty.to_string()) };
                }
                let mut set = vec![("sockets".to_string(), fv(v, "sockets").to_string()), ("cores".to_string(), fv(v, "cores").to_string())];
                let mut del = vec![];
                let cpu_s = cpu.render();
                if cpu_s.is_empty() {
                    if !eff("cpu").is_empty() {
                        del.push("cpu".to_string());
                    }
                } else {
                    set.push(("cpu".into(), cpu_s));
                }
                set.push(("numa".into(), fv(v, "numa").into()));
                for k in ["vcpus", "cpulimit", "cpuunits", "affinity"] {
                    opt_set(&mut set, &mut del, k, fv(v, k), &eff(k));
                }
                self.write(ctx, title("processors"), set, del, false);
            }
            "ctmem" => self.write(ctx, title("memory"), vec![("memory".into(), fv(v, "memory").into()), ("swap".into(), fv(v, "swap").into())], vec![], false),
            "ctcpu" => {
                let (mut set, mut del) = (vec![], vec![]);
                for k in ["cores", "cpulimit", "cpuunits"] {
                    opt_set(&mut set, &mut del, k, fv(v, k), &eff(k));
                }
                self.write(ctx, title("cores"), set, del, false);
            }
            "single" => {
                let (mut set, mut del) = (vec![], vec![]);
                opt_set(&mut set, &mut del, &key, fv(v, "value"), &eff(&key));
                self.write(ctx, title(&key), set, del, false);
            }
            "bool" => self.write(ctx, title(&key), vec![(key.clone(), fv(v, "value").into())], vec![], false),
            "name" => {
                let n = crate::util::guest_name_encode(fv(v, "value"));
                self.write(ctx, title(&key), vec![(key.clone(), n)], vec![], false);
            }
            "vga" => {
                let mut p = Prop::parse(&eff("vga"));
                if p.get("type").is_some() {
                    p.set("type", fv(v, "type"));
                } else {
                    p.first = Some(fv(v, "type").to_string()).filter(|s| !s.is_empty());
                }
                p.set("memory", fv(v, "memory"));
                let (mut set, mut del) = (vec![], vec![]);
                opt_set(&mut set, &mut del, "vga", &p.render(), &eff("vga"));
                self.write(ctx, title("display"), set, del, false);
            }
            "machine" => {
                let mut p = Prop::parse(&eff("machine"));
                if p.get("type").is_some() {
                    p.set("type", fv(v, "type"));
                } else {
                    p.first = Some(fv(v, "type").to_string()).filter(|s| !s.is_empty());
                }
                if p.first.is_none() && p.get("type").is_none() {
                    p.kv.clear();
                }
                let (mut set, mut del) = (vec![], vec![]);
                opt_set(&mut set, &mut del, "machine", &p.render(), &eff("machine"));
                self.write(ctx, title("machine"), set, del, false);
            }
            "startup" => {
                let mut p = Prop::default();
                for k in ["order", "up", "down"] {
                    p.set(k, fv(v, k));
                }
                let (mut set, mut del) = (vec![], vec![]);
                opt_set(&mut set, &mut del, "startup", &p.render(), &eff("startup"));
                self.write(ctx, title("startup"), set, del, false);
            }
            "hotplug" => {
                let sel: Vec<&str> = fv(v, "value").split_whitespace().collect();
                let val = if sel.is_empty() { "0".to_string() } else { sel.join(",") };
                self.write(ctx, title("hotplug"), vec![("hotplug".into(), val)], vec![], false);
            }
            "agent" => {
                let mut p = Prop::parse(&eff("agent"));
                p.remove("enabled");
                p.first = Some(fv(v, "enabled").to_string());
                p.flag("fstrim_cloned_disks", fv(v, "fstrim") == "1", false, "1", "0");
                p.set("type", if fv(v, "type") == "virtio" { "" } else { fv(v, "type") });
                self.write(ctx, title("agent"), vec![("agent".into(), p.render())], vec![], false);
            }
            "desc" => {
                let text = fv(v, "value").replace("\\n", "\n");
                let (mut set, mut del) = (vec![], vec![]);
                opt_set(&mut set, &mut del, "description", &text, &eff("description"));
                self.write(ctx, title("notes"), set, del, false);
            }
            "dns" => {
                let (mut set, mut del) = (vec![], vec![]);
                for k in ["nameserver", "searchdomain"] {
                    opt_set(&mut set, &mut del, k, fv(v, k), &eff(k));
                }
                self.write(ctx, title("DNS"), set, del, false);
            }
            "features" => {
                let mut p = Prop::parse(&eff("features"));
                for k in ["nesting", "keyctl", "fuse"] {
                    p.flag(k, fv(v, k) == "1", false, "1", "0");
                }
                p.set("mount", fv(v, "mount"));
                let (mut set, mut del) = (vec![], vec![]);
                opt_set(&mut set, &mut del, "features", &p.render(), &eff("features"));
                self.write(ctx, title("features"), set, del, false);
            }
            "boot" => {
                let mut order: Vec<String> = Vec::new();
                for i in 0..8 {
                    let d = fv(v, &format!("b{i}"));
                    if !d.is_empty() && !order.iter().any(|o| o == d) {
                        order.push(d.to_string());
                    }
                }
                if order.is_empty() {
                    self.write(ctx, title("boot order"), vec![], vec!["boot".into()], false);
                } else {
                    self.write(ctx, title("boot order"), vec![("boot".into(), format!("order={}", order.join(";")))], vec![], false);
                }
            }
            "disk" => {
                let mut p = Prop::parse(&eff(&key));
                self.apply_disk_opts(&mut p, &key, v);
                self.write(ctx, title(&key), vec![(key.clone(), p.render())], vec![], false);
            }
            "ctdisk" => {
                let mut p = Prop::parse(&eff(&key));
                if key != "rootfs" {
                    p.set("mp", fv(v, "mp"));
                    p.flag("backup", fv(v, "backup") == "1", false, "1", "0");
                }
                p.flag("ro", fv(v, "ro") == "1", false, "1", "0");
                p.flag("replicate", fv(v, "replicate") == "1", true, "1", "0");
                p.set("mountoptions", fv(v, "mountoptions"));
                self.write(ctx, title(&key), vec![(key.clone(), p.render())], vec![], false);
            }
            "adddisk" => {
                let bus = fv(v, "bus");
                let Some(k) = self.free_slot(bus) else {
                    ctx.popup(Popup::error("Add device", &format!("no free {bus} slot")));
                    return;
                };
                let mut p = Prop { first: Some(format!("{}:{}", fv(v, "storage"), fv(v, "size"))), kv: vec![] };
                p.set("format", fv(v, "format"));
                self.apply_disk_opts(&mut p, &k, v);
                self.write(ctx, format!("Add {k} to {}", self.id()), vec![(k, p.render())], vec![], true);
            }
            "attach" => {
                let bus = fv(v, "bus");
                let Some(k) = self.free_slot(bus) else {
                    ctx.popup(Popup::error("Add device", &format!("no free {bus} slot")));
                    return;
                };
                let vol = eff(&key);
                let target = if self.qemu { k } else { self.free_slot("mp").unwrap_or_else(|| "mp0".into()) };
                let val = if self.qemu { vol } else { format!("{vol},mp={}", fv(v, "mp")) };
                self.write(ctx, format!("Attach {key} as {target}"), vec![(target, val)], vec![], true);
            }
            "cd" | "addcd" => {
                let k = if op == "cd" {
                    key.clone()
                } else {
                    match self.free_slot(fv(v, "bus")) {
                        Some(k) => k,
                        None => {
                            ctx.popup(Popup::error("Add device", "no free slot"));
                            return;
                        }
                    }
                };
                let mut p = if op == "cd" { Prop::parse(&eff(&k)) } else { Prop::default() };
                p.first = Some(fv(v, "iso").to_string());
                p.set("media", "cdrom");
                self.write(ctx, format!("{} {k}", tr("CD/DVD drive")), vec![(k, p.render())], vec![], false);
            }
            "net" | "addnet" => {
                let k = if op == "net" { key.clone() } else { self.free_slot("net").unwrap_or_default() };
                if k.is_empty() {
                    return;
                }
                let mut p = if op == "net" { Prop::parse(&eff(&k)) } else { Prop::default() };
                let old_mac = p.kv.iter().find(|(m, _)| NET_MODELS.contains(&m.as_str())).map(|x| x.1.clone()).unwrap_or_default();
                p.kv.retain(|(m, _)| !NET_MODELS.contains(&m.as_str()));
                let mac = if fv(v, "mac").is_empty() { old_mac } else { fv(v, "mac").to_string() };
                p.kv.insert(0, (fv(v, "model").to_string(), mac));
                p.set("bridge", fv(v, "bridge"));
                p.set("tag", fv(v, "tag"));
                p.flag("firewall", fv(v, "firewall") == "1", false, "1", "0");
                p.flag("link_down", fv(v, "link_down") == "1", false, "1", "0");
                for f in ["rate", "queues", "mtu"] {
                    p.set(f, fv(v, f));
                }
                self.write(ctx, format!("{} {k}", tr("Network device")), vec![(k, p.render())], vec![], false);
            }
            "ctnet" | "addctnet" => {
                let k = if op == "ctnet" { key.clone() } else { self.free_slot("net").unwrap_or_default() };
                if k.is_empty() {
                    return;
                }
                let mut p = if op == "ctnet" { Prop::parse(&eff(&k)) } else { Prop::default() };
                for f in ["name", "bridge", "hwaddr", "ip", "gw", "ip6", "gw6", "tag", "rate", "mtu"] {
                    p.set(f, fv(v, f));
                }
                p.flag("firewall", fv(v, "firewall") == "1", false, "1", "0");
                if p.get("type").is_none() {
                    p.set("type", "veth");
                }
                self.write(ctx, format!("{} {k}", tr("Network device")), vec![(k, p.render())], vec![], false);
            }
            "addmp" => {
                let Some(k) = self.free_slot("mp") else { return };
                let src = if fv(v, "storage") == "__bind" { fv(v, "hostpath").to_string() } else { format!("{}:{}", fv(v, "storage"), fv(v, "size")) };
                let mut p = Prop { first: Some(src), kv: vec![] };
                p.set("mp", fv(v, "mp"));
                p.flag("backup", fv(v, "backup") == "1", false, "1", "0");
                self.write(ctx, format!("Add {k} to {}", self.id()), vec![(k, p.render())], vec![], true);
            }
            "addefi" => {
                let mut p = Prop { first: Some(format!("{}:1", fv(v, "storage"))), kv: vec![] };
                p.set("efitype", fv(v, "efitype"));
                p.set("pre-enrolled-keys", if fv(v, "keys") == "1" { "1" } else { "0" });
                self.write(ctx, format!("Add efidisk0 to {}", self.id()), vec![("efidisk0".into(), p.render())], vec![], true);
            }
            "addtpm" => {
                let p = Prop { first: Some(format!("{}:1", fv(v, "storage"))), kv: vec![("version".into(), fv(v, "version").into())] };
                self.write(ctx, format!("Add tpmstate0 to {}", self.id()), vec![("tpmstate0".into(), p.render())], vec![], true);
            }
            "usb" | "addusb" => {
                let k = if op == "usb" { key.clone() } else { self.free_slot("usb").unwrap_or_default() };
                if k.is_empty() {
                    return;
                }
                let mut p = if op == "usb" { Prop::parse(&eff(&k)) } else { Prop::default() };
                p.first = None;
                p.remove("host");
                p.remove("mapping");
                let dev = fv(v, "device");
                match dev.split_once('=') {
                    Some((a, b)) => p.kv.insert(0, (a.to_string(), b.to_string())),
                    None => p.first = Some(dev.to_string()),
                }
                p.flag("usb3", fv(v, "usb3") == "1", false, "1", "0");
                self.write(ctx, format!("{} {k}", tr("USB device")), vec![(k, p.render())], vec![], false);
            }
            "pci" | "addpci" => {
                let k = if op == "pci" { key.clone() } else { self.free_slot("hostpci").unwrap_or_default() };
                if k.is_empty() {
                    return;
                }
                let mut p = if op == "pci" { Prop::parse(&eff(&k)) } else { Prop::default() };
                p.first = None;
                p.remove("host");
                p.remove("mapping");
                let mut dev = fv(v, "device").to_string();
                if let Some(m) = dev.strip_prefix("mapping=") {
                    p.kv.insert(0, ("mapping".into(), m.to_string()));
                } else {
                    if fv(v, "allfn") == "1" {
                        if let Some((a, _)) = dev.rsplit_once('.') {
                            dev = a.to_string();
                        }
                    }
                    p.first = Some(dev);
                }
                p.flag("pcie", fv(v, "pcie") == "1", false, "1", "0");
                p.flag("x-vga", fv(v, "xvga") == "1", false, "1", "0");
                p.flag("rombar", fv(v, "rombar") == "1", true, "1", "0");
                self.write(ctx, format!("{} {k}", tr("PCI device")), vec![(k, p.render())], vec![], false);
            }
            "serial" | "addserial" => {
                let k = if op == "serial" { key.clone() } else { self.free_slot("serial").unwrap_or_default() };
                if k.is_empty() {
                    return;
                }
                self.write(ctx, format!("{} {k}", tr("Serial port")), vec![(k, fv(v, "device").to_string())], vec![], false);
            }
            "resize" => {
                let add = fv(v, "add").trim().trim_start_matches('+').to_string();
                let p = vec![("disk".to_string(), key.clone()), ("size".to_string(), format!("+{add}G"))];
                self.call(ctx, format!("Resize {key} +{add}G"), "PUT", format!("{}/resize", self.base()), p);
            }
            "move" => {
                let mut p = vec![("storage".to_string(), fv(v, "storage").to_string()), ("delete".to_string(), fv(v, "delete").to_string())];
                let path = if self.qemu {
                    p.push(("disk".into(), key.clone()));
                    if !fv(v, "format").is_empty() {
                        p.push(("format".into(), fv(v, "format").into()));
                    }
                    format!("{}/move_disk", self.base())
                } else {
                    p.push(("volume".into(), key.clone()));
                    format!("{}/move_volume", self.base())
                };
                self.call(ctx, format!("Move {key} -> {}", fv(v, "storage")), "POST", path, p);
            }
            "raw" => {
                let (mut set, mut del) = (vec![], vec![]);
                opt_set(&mut set, &mut del, &key, fv(v, "value"), &eff(&key));
                self.write(ctx, title(&key), set, del, false);
            }
            "rawadd" => {
                let k = fv(v, "key").trim().to_string();
                self.write(ctx, title(&k), vec![(k.clone(), fv(v, "value").to_string())], vec![], DISK_BUS.contains(&split_key(&k).0));
            }
            _ => {}
        }
    }

    #[cfg(test)]
    fn clone_on_form(&self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        // on_form only reads the page; run it on a shallow copy that shares the record
        let mut c = GuestCfg {
            node: self.node.clone(),
            vmid: self.vmid.clone(),
            qemu: self.qemu,
            name: self.name.clone(),
            data: Loader::default(),
            info: Loader::default(),
            want: None,
            table: Table::default(),
            scroll: 0,
            sent: Default::default(),
        };
        c.data.data = self.data.data.as_ref().map(|d| CfgData { e: d.e.clone(), digest: d.digest.clone(), running: d.running });
        c.on_form(tag, v, ctx);
        self.sent.borrow_mut().extend(c.sent.into_inner());
    }

    fn apply_disk_opts(&self, p: &mut Prop, key: &str, v: &[(String, String)]) {
        let bus = split_key(key).0;
        p.set("cache", fv(v, "cache"));
        p.flag("discard", fv(v, "discard") == "1", false, "on", "ignore");
        if matches!(bus, "scsi" | "virtio") {
            p.flag("iothread", fv(v, "iothread") == "1", false, "1", "0");
        }
        if bus != "virtio" {
            p.flag("ssd", fv(v, "ssd") == "1", false, "1", "0");
        }
        p.flag("backup", fv(v, "backup") == "1", true, "1", "0");
        p.flag("replicate", fv(v, "replicate") == "1", true, "1", "0");
    }

    // ---- forms ----

    fn edit_form(&self, it: &Item, ctx: &mut Ctx) {
        let Some(d) = self.d() else { return };
        let info = self.info();
        let e = |k: &str| d.eff(k);
        let key = it.key.clone();
        let t = format!("{} · {}", self.id(), tr(&it.label));
        let tag = |op: &str| format!("gc:{op}:{key}");
        let fields: Vec<Field>;
        let op: String;
        match it.kind {
            K::Mem => {
                op = tag("mem");
                fields = vec![
                    form::number("memory", "Memory (MiB)", &or_def(e("memory"), "512")),
                    form::number("balloon", "Minimum memory (MiB)", &e("balloon")).req(false).help("ballooning: empty = same as memory, 0 = off"),
                    form::number("shares", "Shares", &e("shares")).req(false).help("empty = default (1000)"),
                ];
            }
            K::Cpu => {
                op = tag("cpu");
                let cpu = Prop::parse(&e("cpu"));
                let cur = cpu.first.clone().or(cpu.get("cputype").map(|s| s.to_string())).unwrap_or_default();
                let mut types: Vec<(String, String)> = vec![("".into(), "default".into())];
                for c in &info.cpus {
                    types.push((c.clone(), c.clone()));
                }
                if !cur.is_empty() && !info.cpus.contains(&cur) {
                    types.push((cur.clone(), cur.clone()));
                }
                fields = vec![
                    form::number("sockets", "Sockets", &or_def(e("sockets"), "1")),
                    form::number("cores", "Cores", &or_def(e("cores"), "1")),
                    form::select("cputype", "CPU type", types, &cur),
                    form::check("numa", "NUMA", e("numa") == "1"),
                    form::number("vcpus", "vCPUs (hotplug)", &e("vcpus")).req(false).help("empty = sockets × cores"),
                    form::text("cpulimit", "CPU limit", &e("cpulimit")).help("empty / 0 = unlimited, 1.5 = one and a half cores"),
                    form::number("cpuunits", "CPU units", &e("cpuunits")).req(false),
                    form::text("affinity", "CPU affinity", &e("affinity")).help("host cores, e.g. 0-3,8"),
                ];
            }
            K::CtMem => {
                op = tag("ctmem");
                fields = vec![form::number("memory", "Memory (MiB)", &or_def(e("memory"), "512")), form::number("swap", "Swap (MiB)", &or_def(e("swap"), "512"))];
            }
            K::CtCpu => {
                op = tag("ctcpu");
                fields = vec![
                    form::number("cores", "Cores", &e("cores")).req(false).help("empty = all host cores"),
                    form::text("cpulimit", "CPU limit", &e("cpulimit")).help("empty / 0 = unlimited"),
                    form::number("cpuunits", "CPU units", &e("cpuunits")).req(false),
                ];
            }
            K::Bios => {
                op = tag("single");
                fields = vec![form::select("value", "BIOS", vec![("".into(), "SeaBIOS (default)".into()), ("ovmf".into(), "OVMF (UEFI)".into())], &e("bios"))
                    .help("OVMF needs an EFI disk for persistent settings")];
            }
            K::Vga => {
                op = tag("vga");
                let p = Prop::parse(&e("vga"));
                let cur = p.first.clone().or(p.get("type").map(|s| s.to_string())).unwrap_or_default();
                let mut types = vec![("".to_string(), "default".to_string())];
                types.extend(form::opts(&["std", "virtio", "virtio-gl", "qxl", "qxl2", "qxl3", "qxl4", "vmware", "cirrus", "serial0", "serial1", "none"]));
                fields = vec![form::select("type", "Graphic card", types, &cur), form::number("memory", "Memory (MiB)", &p.gets("memory")).req(false).help("empty = default")];
            }
            K::Machine => {
                op = tag("machine");
                let p = Prop::parse(&e("machine"));
                let cur = p.first.clone().or(p.get("type").map(|s| s.to_string())).unwrap_or_default();
                let mut types = vec![("".to_string(), "default (i440fx)".to_string())];
                for m in ["q35", "pc", "virt"] {
                    types.push((m.to_string(), format!("{m} (latest)")));
                }
                for m in &info.machines {
                    if !types.iter().any(|(a, _)| a == m) {
                        types.push((m.clone(), m.clone()));
                    }
                }
                if !cur.is_empty() && !types.iter().any(|(a, _)| *a == cur) {
                    types.push((cur.clone(), cur.clone()));
                }
                fields = vec![form::select("type", "Machine", types, &cur)];
            }
            K::Scsihw => {
                op = tag("single");
                let mut o = vec![("".to_string(), "lsi (default)".to_string())];
                o.extend(form::opts(&["virtio-scsi-single", "virtio-scsi-pci", "lsi53c810", "megasas", "pvscsi"]));
                fields = vec![form::select("value", "SCSI controller", o, &e("scsihw"))];
            }
            K::Name | K::Hostname => {
                op = tag("name");
                fields = vec![form::text("value", &it.label, &crate::util::guest_name(&e(&key))).req(true).help("non-ASCII names are stored as punycode (xn--)")];
            }
            K::Dns => {
                op = tag("dns");
                fields = vec![
                    form::text("nameserver", "DNS servers", &e("nameserver")).help("space separated; empty = use host settings"),
                    form::text("searchdomain", "DNS domain", &e("searchdomain")),
                ];
            }
            K::Features => {
                op = tag("features");
                let p = Prop::parse(&e("features"));
                fields = vec![
                    form::check("nesting", "Nesting", p.is_on("nesting", false)),
                    form::check("keyctl", "keyctl", p.is_on("keyctl", false)),
                    form::check("fuse", "FUSE", p.is_on("fuse", false)),
                    form::text("mount", "Allowed mounts", &p.gets("mount")).help("e.g. nfs;cifs"),
                ];
            }
            K::Disk => {
                op = tag("disk");
                fields = disk_fields(&key, &Prop::parse(&e(&key)), false, &info);
            }
            K::Rootfs | K::Mp => {
                op = tag("ctdisk");
                let p = Prop::parse(&e(&key));
                let mut f = vec![form::info("Volume", p.first.as_deref().unwrap_or("")), form::info("Size", &p.gets("size"))];
                if key != "rootfs" {
                    f.push(form::text("mp", "Path in container", &p.gets("mp")).req(true));
                    f.push(form::check("backup", "Include in backup", p.is_on("backup", false)));
                }
                f.push(form::check("ro", "Read-only", p.is_on("ro", false)));
                f.push(form::check("replicate", "Replicate", p.is_on("replicate", true)));
                f.push(form::text("mountoptions", "Mount options", &p.gets("mountoptions")).help("e.g. noatime;nodev"));
                fields = f;
            }
            K::Cd => {
                op = tag("cd");
                let cur = Prop::parse(&e(&key)).first.unwrap_or_default();
                fields = vec![form::select("iso", "Media", iso_opts(&info, &cur), &cur)];
            }
            K::Net => {
                op = tag("net");
                fields = net_fields(&Prop::parse(&e(&key)), &info, false);
            }
            K::CtNet => {
                op = tag("ctnet");
                fields = ctnet_fields(&Prop::parse(&e(&key)), &info);
            }
            K::Efi | K::Tpm => {
                // nothing to edit in place: move / remove / raw edit
                ctx.popup(Popup::text(&t, &format!("{key}: {}\n\nM = move disk, x = remove, E = raw value", e(&key))));
                return;
            }
            K::Usb => {
                op = tag("usb");
                let p = Prop::parse(&e(&key));
                let cur = p.get("host").map(|h| format!("host={h}")).or(p.get("mapping").map(|m| format!("mapping={m}"))).or(p.first.clone()).unwrap_or_default();
                fields = usb_fields(&info, &cur, p.is_on("usb3", false));
            }
            K::Pci => {
                op = tag("pci");
                let p = Prop::parse(&e(&key));
                let cur = p.get("mapping").map(|m| format!("mapping={m}")).or(p.first.clone()).or(p.get("host").map(|s| s.to_string())).unwrap_or_default();
                fields = pci_fields(&info, &cur, &p);
            }
            K::Serial => {
                op = tag("serial");
                fields = vec![form::text("device", "Device", &e(&key)).req(true).help("socket, or a host device like /dev/ttyS0")];
            }
            K::Unused => {
                op = tag("attach");
                let mut f = vec![form::info("Volume", &e(&key))];
                if self.qemu {
                    f.push(form::select("bus", "Bus", form::opts(&["scsi", "virtio", "sata", "ide"]), "scsi"));
                } else {
                    f.push(form::select("bus", "Bus", form::opts(&["mp"]), "mp"));
                    f.push(form::text("mp", "Path in container", "/mnt/data").req(true));
                }
                fields = f;
            }
            K::Opt => match key.as_str() {
                "startup" => {
                    op = tag("startup");
                    let p = Prop::parse(&e("startup"));
                    fields = vec![
                        form::number("order", "Order", &p.gets("order")).req(false).help("lower starts first"),
                        form::number("up", "Startup delay (s)", &p.gets("up")).req(false),
                        form::number("down", "Shutdown timeout (s)", &p.gets("down")).req(false),
                    ];
                }
                "ostype" => {
                    op = tag("single");
                    fields = vec![form::select("value", "OS type", os_opts(&e("ostype")), &e("ostype"))];
                }
                "boot" => {
                    if let Some(f) = self.boot_form() {
                        ctx.popup(Popup::Form(f));
                    }
                    return;
                }
                "hotplug" => {
                    op = tag("hotplug");
                    let cur = match e("hotplug").as_str() {
                        "" | "1" => "network disk usb".to_string(),
                        "0" => String::new(),
                        other => other.replace(',', " "),
                    };
                    fields = vec![form::multi("value", "Hotplug", form::opts(&["network", "disk", "usb", "memory", "cpu", "cloudinit"]), &cur).help("memory / cpu hotplug needs NUMA and guest support")];
                }
                "agent" => {
                    op = tag("agent");
                    let p = Prop::parse(&e("agent"));
                    let on = p.first.as_deref() == Some("1") || p.is_on("enabled", false);
                    fields = vec![
                        form::check("enabled", "Enabled", on),
                        form::check("fstrim", "Run fstrim after moving a disk", p.is_on("fstrim_cloned_disks", false)),
                        form::select("type", "Type", form::opts(&["virtio", "isa"]), &or_def(p.gets("type"), "virtio")),
                    ];
                }
                "description" => {
                    op = tag("desc");
                    fields = vec![form::text("value", "Notes", &e("description").replace('\n', "\\n")).help("\\n = line break")];
                }
                "cmode" => {
                    op = tag("single");
                    let mut o = vec![("".to_string(), "tty (default)".to_string())];
                    o.extend(form::opts(&["console", "shell"]));
                    fields = vec![form::select("value", "Console mode", o, &e("cmode"))];
                }
                k if BOOL_OPTS.iter().any(|(b, _)| *b == k) => {
                    op = tag("bool");
                    let def = BOOL_OPTS.iter().find(|(b, _)| *b == k).map(|x| x.1).unwrap_or(false);
                    let cur = e(k);
                    fields = vec![form::check("value", &it.label, if cur.is_empty() { def } else { cur == "1" })];
                }
                _ => {
                    op = tag("single");
                    fields = vec![form::text("value", &it.label, &e(&key))];
                }
            },
            K::Cloud | K::Other => {
                op = tag("single");
                fields = vec![form::text("value", &key, &e(&key)).help("empty = delete the key")];
            }
            K::ReadOnly => return,
        }
        ctx.popup(Popup::Form(Form::new(&t, &op, fields)));
    }

    fn add_form(&self, key: char, ctx: &mut Ctx) {
        let info = self.info();
        let id = self.id();
        let (title, tag, fields): (&str, &str, Vec<Field>) = match key {
            'd' => {
                let mut f = vec![
                    form::select("bus", "Bus", form::opts(&["scsi", "virtio", "sata", "ide"]), "scsi"),
                    form::select("storage", "Storage", info.disk_stores.clone(), ""),
                    form::number("size", "Size (GiB)", "32"),
                    form::select("format", "Format", fmt_opts(), ""),
                ];
                f.extend(disk_fields("scsi0", &Prop::default(), true, &info));
                ("Hard disk", "gc:adddisk", f)
            }
            'c' => (
                "CD/DVD drive",
                "gc:addcd",
                vec![form::select("bus", "Bus", form::opts(&["ide", "sata", "scsi"]), "ide"), form::select("iso", "Media", iso_opts(&info, "none"), "none")],
            ),
            'n' if self.qemu => ("Network device", "gc:addnet", net_fields(&Prop::default(), &info, true)),
            'n' => ("Network device", "gc:addctnet", ctnet_fields(&Prop { first: None, kv: vec![("name".into(), format!("eth{}", self.free_slot("net").unwrap_or_default().trim_start_matches("net")))] }, &info)),
            'e' => (
                "EFI disk",
                "gc:addefi",
                vec![
                    form::select("storage", "Storage", info.disk_stores.clone(), ""),
                    form::select("efitype", "EFI vars size", form::opts(&["4m", "2m"]), "4m"),
                    form::check("keys", "Pre-enroll keys (Secure Boot)", true),
                ],
            ),
            't' => (
                "TPM state",
                "gc:addtpm",
                vec![form::select("storage", "Storage", info.disk_stores.clone(), ""), form::select("version", "Version", form::opts(&["v2.0", "v1.2"]), "v2.0")],
            ),
            'u' => ("USB device", "gc:addusb", usb_fields(&info, "", false)),
            'p' => ("PCI device", "gc:addpci", pci_fields(&info, "", &Prop::default())),
            's' => ("Serial port", "gc:addserial", vec![form::text("device", "Device", "socket").req(true).help("socket, or a host device like /dev/ttyS0")]),
            'm' => {
                let mut stores = info.ct_stores.clone();
                stores.push(("__bind".into(), "Bind mount (host directory)".into()));
                (
                    "Mount point",
                    "gc:addmp",
                    vec![
                        form::select("storage", "Storage", stores, ""),
                        form::number("size", "Size (GiB)", "8"),
                        form::text("hostpath", "Host path (bind mount)", "").help("only for bind mounts"),
                        form::text("mp", "Path in container", "/mnt/data").req(true),
                        form::check("backup", "Include in backup", false),
                    ],
                )
            }
            _ => return,
        };
        if fields.iter().any(|f| matches!(&f.kind, form::Kind::Select(o) if o.is_empty())) {
            ctx.popup(Popup::error(title, "no suitable storage / device found on this node"));
            return;
        }
        ctx.popup(Popup::Form(Form::new(&format!("{id} · {}", tr(title)), tag, fields)));
    }

    /// Devices that can boot: disks, CD drives, network devices, PCI passthrough.
    fn boot_candidates(&self) -> Vec<(String, String)> {
        let Some(d) = self.d() else { return vec![] };
        let mut v: Vec<(String, String)> = d
            .e
            .keys()
            .filter(|k| d.exists(k) && !d.eff(k).is_empty() && is_dev(k, &["ide", "sata", "scsi", "virtio", "net", "hostpci"]))
            .map(|k| (k.clone(), format!("{k}  {}", d.eff(k))))
            .collect();
        v.sort_by_key(|(k, _)| dev_rank(k));
        v
    }

    fn boot_form(&self) -> Option<Form> {
        let d = self.d()?;
        let cands = self.boot_candidates();
        let order: Vec<String> = Prop::parse(&d.eff("boot")).get("order").map(|o| o.split(';').map(|s| s.to_string()).collect()).unwrap_or_default();
        let mut opts = vec![("".to_string(), "(none)".to_string())];
        opts.extend(cands.iter().cloned());
        let n = cands.len().clamp(1, 8);
        let fields = (0..n)
            .map(|i| form::select(&format!("b{i}"), &format!("{}. {}", i + 1, tr("boot device")), opts.clone(), order.get(i).map(|s| s.as_str()).unwrap_or("")))
            .collect();
        Some(Form::new(&format!("{} · {}", self.id(), tr("Boot order")), "gc:boot:", fields))
    }
}

/// Set `k` when the new value is non-empty, delete it when it became empty.
fn opt_set(set: &mut Vec<(String, String)>, del: &mut Vec<String>, k: &str, new: &str, old: &str) {
    if new.is_empty() {
        if !old.is_empty() {
            del.push(k.to_string());
        }
    } else if new != old {
        set.push((k.to_string(), new.to_string()));
    }
}

fn or_def(v: String, d: &str) -> String {
    if v.is_empty() {
        d.to_string()
    } else {
        v
    }
}

fn fmt_opts() -> Vec<(String, String)> {
    vec![("".into(), "storage default".into()), ("raw".into(), "raw".into()), ("qcow2".into(), "qcow2".into()), ("vmdk".into(), "vmdk".into())]
}

fn os_opts(cur: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = [
        ("l26", "Linux 2.6 - 6.x"),
        ("win11", "Windows 11 / 2022 / 2025"),
        ("win10", "Windows 10 / 2016 / 2019"),
        ("win8", "Windows 8 / 2012"),
        ("win7", "Windows 7 / 2008 R2"),
        ("w2k8", "Windows Vista / 2008"),
        ("wxp", "Windows XP / 2003"),
        ("w2k", "Windows 2000"),
        ("solaris", "Solaris"),
        ("l24", "Linux 2.4"),
        ("other", "Other"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    if !cur.is_empty() && !v.iter().any(|(a, _)| a == cur) {
        v.push((cur.to_string(), cur.to_string()));
    }
    v
}

fn iso_opts(info: &Info, cur: &str) -> Vec<(String, String)> {
    let mut v = vec![("none".to_string(), "(empty)".to_string()), ("cdrom".to_string(), "Physical drive of the host".to_string())];
    v.extend(info.isos.iter().cloned());
    if !cur.is_empty() && !v.iter().any(|(a, _)| a == cur) {
        v.push((cur.to_string(), cur.to_string()));
    }
    v
}

fn disk_fields(key: &str, p: &Prop, new: bool, _info: &Info) -> Vec<Field> {
    let bus = split_key(key).0;
    let mut f = Vec::new();
    if !new {
        f.push(form::info("Volume", p.first.as_deref().unwrap_or("")));
        f.push(form::info("Size", &p.gets("size")));
    }
    let mut cache = vec![("".to_string(), "default (no cache)".to_string())];
    cache.extend(form::opts(&["directsync", "writethrough", "writeback", "unsafe", "none"]));
    f.push(form::select("cache", "Cache", cache, &p.gets("cache")));
    f.push(form::check("discard", "Discard", p.get("discard") == Some("on")));
    if new || matches!(bus, "scsi" | "virtio") {
        f.push(form::check("iothread", "IO thread", if new { true } else { p.is_on("iothread", false) }).help("scsi (virtio-scsi-single) and virtio only"));
    }
    if new || bus != "virtio" {
        f.push(form::check("ssd", "SSD emulation", p.is_on("ssd", false)));
    }
    f.push(form::check("backup", "Include in backup", p.is_on("backup", true)));
    f.push(form::check("replicate", "Replicate", p.is_on("replicate", true)));
    f
}

fn net_fields(p: &Prop, info: &Info, new: bool) -> Vec<Field> {
    let (model, mac) = p.kv.iter().find(|(k, _)| NET_MODELS.contains(&k.as_str())).cloned().unwrap_or(("virtio".into(), String::new()));
    let mut bridges: Vec<(String, String)> = info.bridges.iter().map(|b| (b.clone(), b.clone())).collect();
    let cur_br = p.gets("bridge");
    if !cur_br.is_empty() && !info.bridges.contains(&cur_br) {
        bridges.push((cur_br.clone(), cur_br.clone()));
    }
    if bridges.is_empty() {
        bridges.push(("vmbr0".into(), "vmbr0".into()));
    }
    vec![
        form::select("model", "Model", form::opts(&["virtio", "e1000", "e1000e", "rtl8139", "vmxnet3"]), &model),
        form::select("bridge", "Bridge", bridges, &cur_br),
        form::number("tag", "VLAN tag", &p.gets("tag")).req(false).help("empty = no VLAN"),
        form::check("firewall", "Firewall", if new { true } else { p.is_on("firewall", false) }),
        form::check("link_down", "Disconnected", p.is_on("link_down", false)),
        form::text("rate", "Rate limit (MB/s)", &p.gets("rate")),
        form::number("queues", "Multiqueue", &p.gets("queues")).req(false),
        form::number("mtu", "MTU", &p.gets("mtu")).req(false).help("1 = use the bridge MTU"),
        form::text("mac", "MAC address", &mac).help("empty = generate"),
    ]
}

fn ctnet_fields(p: &Prop, info: &Info) -> Vec<Field> {
    let mut bridges: Vec<(String, String)> = info.bridges.iter().map(|b| (b.clone(), b.clone())).collect();
    let cur_br = p.gets("bridge");
    if !cur_br.is_empty() && !info.bridges.contains(&cur_br) {
        bridges.push((cur_br.clone(), cur_br.clone()));
    }
    if bridges.is_empty() {
        bridges.push(("vmbr0".into(), "vmbr0".into()));
    }
    vec![
        form::text("name", "Name", &or_def(p.gets("name"), "eth0")).req(true),
        form::select("bridge", "Bridge", bridges, &cur_br),
        form::text("ip", "IPv4", &p.gets("ip")).help("dhcp, CIDR like 192.168.1.10/24, or empty"),
        form::text("gw", "IPv4 gateway", &p.gets("gw")),
        form::text("ip6", "IPv6", &p.gets("ip6")).help("dhcp, auto, CIDR, or empty"),
        form::text("gw6", "IPv6 gateway", &p.gets("gw6")),
        form::number("tag", "VLAN tag", &p.gets("tag")).req(false),
        form::check("firewall", "Firewall", p.is_on("firewall", false)),
        form::text("rate", "Rate limit (MB/s)", &p.gets("rate")),
        form::number("mtu", "MTU", &p.gets("mtu")).req(false),
        form::text("hwaddr", "MAC address", &p.gets("hwaddr")).help("empty = generate"),
    ]
}

fn usb_fields(info: &Info, cur: &str, usb3: bool) -> Vec<Field> {
    let mut o: Vec<(String, String)> = info.usb_maps.iter().map(|m| (format!("mapping={m}"), format!("mapping: {m}"))).collect();
    o.extend(info.usb.iter().cloned());
    o.push(("spice".into(), "SPICE USB redirection".into()));
    if !cur.is_empty() && !o.iter().any(|(a, _)| a == cur) {
        o.push((cur.to_string(), cur.to_string()));
    }
    vec![form::select("device", "Device", o, cur), form::check("usb3", "USB 3", usb3)]
}

fn pci_fields(info: &Info, cur: &str, p: &Prop) -> Vec<Field> {
    let mut o: Vec<(String, String)> = info.pci_maps.iter().map(|m| (format!("mapping={m}"), format!("mapping: {m}"))).collect();
    o.extend(info.pci.iter().cloned());
    if !cur.is_empty() && !o.iter().any(|(a, _)| a == cur) {
        o.push((cur.to_string(), cur.to_string()));
    }
    let allfn = !cur.is_empty() && !cur.starts_with("mapping=") && !cur.contains('.');
    vec![
        form::select("device", "Device", o, cur),
        form::check("allfn", "All functions", allfn),
        form::check("pcie", "PCI-Express (q35)", p.is_on("pcie", false)),
        form::check("xvga", "Primary GPU (x-vga)", p.is_on("x-vga", false)),
        form::check("rombar", "ROM-Bar", p.is_on("rombar", true)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn test_ctx() -> Ctx {
        Ctx::test()
    }

    /// A page with injected data: (key, value, pending, delete).
    fn fake(qemu: bool, entries: &[(&str, Option<&str>, Option<&str>, bool)]) -> GuestCfg {
        let mut d = CfgData { digest: "abc".into(), running: true, ..Default::default() };
        for (k, v, p, del) in entries {
            d.e.insert(k.to_string(), Entry { value: v.map(|s| s.to_string()), pending: p.map(|s| s.to_string()), delete: *del });
        }
        let mut g = GuestCfg {
            node: "n1".into(),
            vmid: "108".into(),
            qemu,
            name: "运维跳板机".into(),
            data: Loader::default(),
            info: Loader::default(),
            want: None,
            table: Table::default(),
            scroll: 0,
            sent: Default::default(),
        };
        g.data.data = Some(d);
        g.info.data = Some(Info {
            disk_stores: vec![("local-lvm".into(), "local-lvm".into())],
            ct_stores: vec![("local-lvm".into(), "local-lvm".into())],
            bridges: vec!["vmbr0".into(), "vmbr1".into()],
            ..Default::default()
        });
        g
    }

    fn vm() -> GuestCfg {
        fake(
            true,
            &[
                ("memory", Some("8192"), None, false),
                ("balloon", Some("2048"), None, false),
                ("sockets", Some("1"), None, false),
                ("cores", Some("4"), Some("8"), false),
                ("cpu", Some("host"), None, false),
                ("bios", Some("ovmf"), None, false),
                ("machine", Some("q35"), None, false),
                ("scsihw", Some("virtio-scsi-single"), None, false),
                ("scsi0", Some("local-lvm:vm-108-disk-1,iothread=1,size=64G,discard=on"), None, false),
                ("scsi10", Some("local-lvm:vm-108-disk-3,size=8G"), None, false),
                ("ide2", Some("local:iso/virtio-win.iso,media=cdrom,size=700M"), None, false),
                ("net0", Some("virtio=BC:24:11:AA:BB:CC,bridge=vmbr0,firewall=1,tag=18,queues=4"), None, false),
                ("efidisk0", Some("local-lvm:vm-108-disk-0,efitype=4m,pre-enrolled-keys=1,size=4M"), None, false),
                ("usb0", Some("host=1d6b:0002,usb3=1"), None, false),
                ("hostpci0", Some("0000:01:00,pcie=1,x-vga=1"), None, false),
                ("unused0", Some("local-lvm:vm-108-disk-2"), None, false),
                ("boot", Some("order=scsi0;ide2;net0"), None, false),
                ("name", Some("xn--nqvwfw60e0wndgc"), None, false),
                ("onboot", Some("1"), None, false),
                ("agent", Some("1,fstrim_cloned_disks=1"), None, false),
                ("serial0", None, Some("socket"), false),
                ("tablet", Some("0"), None, true),
                ("digest", Some("abc"), None, false),
            ],
        )
    }

    fn render(g: &mut GuestCfg) -> String {
        use ratatui::backend::TestBackend;
        let mut t = ratatui::Terminal::new(TestBackend::new(150, 40)).unwrap();
        t.draw(|f| {
            let a = f.area();
            let (l, r) = (Rect::new(0, 0, 95, a.height), Rect::new(95, 0, 55, a.height));
            g.draw_list(f, l, true);
            g.draw_detail(f, r, false);
        })
        .unwrap();
        let b = t.backend().buffer().clone();
        (0..40).map(|y| (0..150).map(|x| b[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn page_lists_all_items() {
        let mut g = vm();
        let keys: Vec<String> = g.items().iter().map(|i| i.key.clone()).collect();
        // devices sorted by bus and number, composite rows first
        let pos = |k: &str| keys.iter().position(|x| x == k).unwrap_or(usize::MAX);
        assert!(pos("@mem") < pos("@cpu"));
        assert!(pos("efidisk0") < pos("ide2") && pos("ide2") < pos("scsi0") && pos("scsi0") < pos("scsi10") && pos("scsi10") < pos("net0"));
        assert!(pos("serial0") < pos("unused0"));
        assert!(!keys.contains(&"digest".to_string()));
        let out = render(&mut g);
        if std::env::var("SHOW").is_ok() {
            println!("{out}");
        }
        assert!(out.contains("8.0G"), "{out}");
        assert!(out.contains("1 × 4 = 4 vCPU  host  →  1 × 8 = 8 vCPU  host"), "{out}");
        assert!(out.contains("CD/DVD  virtio-win.iso"), "{out}");
        assert!(out.contains("virtio  BC:24:11:AA:BB:CC  vmbr0  VLAN 18  fw"), "{out}");
        assert!(out.replace(" ", "").contains("运维跳板机"), "{out}");
        assert!(out.contains("scsi0, ide2, net0"), "{out}");
    }

    fn sent(g: &GuestCfg) -> Vec<(String, String, Vec<(String, String)>)> {
        g.sent.borrow().clone()
    }

    fn vals(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    fn param<'a>(p: &'a [(String, String)], k: &str) -> Option<&'a str> {
        p.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str())
    }

    #[test]
    fn net_edit_keeps_mac_and_other_options() {
        let g = vm();
        let mut ctx = test_ctx();
        g.clone_on_form("gc:net:net0", &vals(&[("model", "virtio"), ("bridge", "vmbr1"), ("tag", ""), ("firewall", "1"), ("link_down", "0"), ("rate", ""), ("queues", "4"), ("mtu", ""), ("mac", "BC:24:11:AA:BB:CC")]), &mut ctx);
        let s = sent(&g);
        let (m, path, p) = &s[0];
        assert_eq!((m.as_str(), path.as_str()), ("PUT", "/nodes/n1/qemu/108/config"));
        assert_eq!(param(p, "net0"), Some("virtio=BC:24:11:AA:BB:CC,bridge=vmbr1,firewall=1,queues=4"));
        assert_eq!(param(p, "digest"), Some("abc"));
    }

    #[test]
    fn add_disk_uses_free_slot_and_post() {
        let g = vm();
        let mut ctx = test_ctx();
        g.clone_on_form(
            "gc:adddisk:",
            &vals(&[("bus", "scsi"), ("storage", "local-lvm"), ("size", "32"), ("format", ""), ("cache", ""), ("discard", "1"), ("iothread", "1"), ("ssd", "0"), ("backup", "1"), ("replicate", "1")]),
            &mut ctx,
        );
        let s = sent(&g);
        let (m, _, p) = &s[0];
        assert_eq!(m, "POST");
        assert_eq!(param(p, "scsi1"), Some("local-lvm:32,discard=on,iothread=1"));
    }

    #[test]
    fn cpu_memory_boot_and_options() {
        let g = vm();
        let mut ctx = test_ctx();
        g.clone_on_form("gc:cpu:@cpu", &vals(&[("sockets", "2"), ("cores", "4"), ("cputype", "x86-64-v3"), ("numa", "1"), ("vcpus", ""), ("cpulimit", ""), ("cpuunits", ""), ("affinity", "")]), &mut ctx);
        g.clone_on_form("gc:mem:@mem", &vals(&[("memory", "16384"), ("balloon", ""), ("shares", "")]), &mut ctx);
        g.clone_on_form("gc:boot:", &vals(&[("b0", "net0"), ("b1", "scsi0"), ("b2", "net0"), ("b3", "")]), &mut ctx);
        g.clone_on_form("gc:name:name", &vals(&[("value", "中文-vm")]), &mut ctx);
        g.clone_on_form("gc:agent:agent", &vals(&[("enabled", "1"), ("fstrim", "0"), ("type", "virtio")]), &mut ctx);
        g.clone_on_form("gc:resize:scsi0", &vals(&[("add", "10")]), &mut ctx);
        let s = sent(&g);
        assert_eq!(param(&s[0].2, "cpu"), Some("x86-64-v3"));
        assert_eq!(param(&s[0].2, "sockets"), Some("2"));
        assert_eq!(param(&s[1].2, "delete"), Some("balloon"));
        assert_eq!(param(&s[2].2, "boot"), Some("order=net0;scsi0"));
        assert_eq!(crate::util::guest_name(param(&s[3].2, "name").unwrap()), "中文-vm");
        assert_eq!(param(&s[4].2, "agent"), Some("1"));
        assert_eq!((s[5].1.as_str(), param(&s[5].2, "size")), ("/nodes/n1/qemu/108/resize", Some("+10G")));
    }

    /// Real PVE: `sudo GC_LIVE=VMID GC_NODE=node test-binary live_pve --ignored --nocapture`
    /// on a throwaway VM. Every step goes through the page's own form handling.
    #[test]
    #[ignore]
    fn live_pve() {
        let (Ok(id), Ok(node)) = (std::env::var("GC_LIVE"), std::env::var("GC_NODE")) else { return };
        let api = crate::api::Api::pvesh();
        let base = format!("/nodes/{node}/qemu/{id}");
        let info = load_info(&api, &node);
        let store = info.disk_stores.first().map(|s| s.0.clone()).expect("a storage for images");
        let page = || {
            let mut g = fake(true, &[]);
            g.node = node.clone();
            g.vmid = id.clone();
            g.data.data = Some(fetch(&api, &base).expect("fetch"));
            g
        };
        let steps: Vec<(&str, Vec<(&str, String)>, &str, &str)> = vec![
            ("gc:mem:@mem", vec![("memory", "1024".into()), ("balloon", "512".into()), ("shares", "".into())], "balloon", "512"),
            ("gc:cpu:@cpu", vec![("sockets", "1".into()), ("cores", "2".into()), ("cputype", "host".into()), ("numa", "0".into()), ("vcpus", "".into()), ("cpulimit", "".into()), ("cpuunits", "".into()), ("affinity", "".into())], "cpu", "host"),
            ("gc:name:name", vec![("value", "配置测试".into())], "name", "xn--"),
            ("gc:bool:onboot", vec![("value", "1".into())], "onboot", "1"),
            ("gc:startup:startup", vec![("order", "3".into()), ("up", "10".into()), ("down", "".into())], "startup", "order=3,up=10"),
            ("gc:agent:agent", vec![("enabled", "1".into()), ("fstrim", "1".into()), ("type", "virtio".into())], "agent", "1,fstrim_cloned_disks=1"),
            ("gc:vga:vga", vec![("type", "std".into()), ("memory", "32".into())], "vga", "std,memory=32"),
            ("gc:machine:machine", vec![("type", "q35".into())], "machine", "q35"),
            ("gc:single:scsihw", vec![("value", "virtio-scsi-single".into())], "scsihw", "virtio-scsi-single"),
            ("gc:single:bios", vec![("value", "ovmf".into())], "bios", "ovmf"),
            ("gc:addefi:", vec![("storage", store.clone()), ("efitype", "4m".into()), ("keys", "1".into())], "efidisk0", "efitype=4m"),
            ("gc:addtpm:", vec![("storage", store.clone()), ("version", "v2.0".into())], "tpmstate0", "version=v2.0"),
            (
                "gc:adddisk:",
                vec![("bus", "scsi".into()), ("storage", store.clone()), ("size", "1".into()), ("format", "qcow2".into()), ("cache", "".into()), ("discard", "1".into()), ("iothread", "1".into()), ("ssd", "0".into()), ("backup", "1".into()), ("replicate", "1".into())],
                "scsi0",
                "size=1G",
            ),
            ("gc:disk:scsi0", vec![("cache", "writeback".into()), ("discard", "1".into()), ("iothread", "1".into()), ("ssd", "1".into()), ("backup", "0".into()), ("replicate", "1".into())], "scsi0", "cache=writeback"),
            ("gc:resize:scsi0", vec![("add", "1".into())], "scsi0", "size=2G"),
            ("gc:addcd:", vec![("bus", "ide".into()), ("iso", "none".into())], "ide0", "none,media=cdrom"),
            ("gc:addserial:", vec![("device", "socket".into())], "serial0", "socket"),
            ("gc:addusb:", vec![("device", "spice".into()), ("usb3", "0".into())], "usb0", "spice"),
            ("gc:boot:", vec![("b0", "scsi0".into()), ("b1", "ide0".into())], "boot", "order=scsi0;ide0"),
            ("gc:desc:description", vec![("value", "line1\\nline2".into())], "description", "line1\nline2"),
            (
                "gc:addnet:",
                vec![("model", "virtio".into()), ("bridge", "vmbr0".into()), ("tag", "18".into()), ("firewall", "1".into()), ("link_down", "0".into()), ("rate", "".into()), ("queues", "".into()), ("mtu", "".into()), ("mac", "".into())],
                "net0",
                "tag=18",
            ),
        ];
        let mut failed = Vec::new();
        for (tag, vals, key, want) in steps {
            let g = page();
            let v: Vec<(String, String)> = vals.iter().map(|(a, b)| (a.to_string(), b.clone())).collect();
            let mut ctx = test_ctx();
            g.clone_on_form(tag, &v, &mut ctx);
            for (m, path, p) in g.sent.borrow().iter() {
                let pp: Vec<(&str, String)> = p.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                let r = match m.as_str() {
                    "POST" => api.post(path, &pp),
                    _ => api.put(path, &pp),
                };
                if let Err(e) = r {
                    println!("FAIL {tag}: {m} {path} {p:?}: {e:#}");
                }
            }
            let now = page().d().map(|d| d.eff(key)).unwrap_or_default();
            let ok = now.contains(want);
            println!("{} {tag:<22} {key} = {now}", if ok { "ok  " } else { "FAIL" });
            if !ok {
                failed.push(tag);
            }
        }
        // detach -> unused, attach again, detach and destroy the volume
        for (tag, key) in [("gc:del:scsi0", "unused0"), ("gc:attach:unused0", "scsi0"), ("gc:del:scsi0", "unused0"), ("gc:del:unused0", "")] {
            let mut g = page();
            let mut ctx = test_ctx();
            if tag.starts_with("gc:del:") {
                g.on_confirm(tag, &mut ctx);
            } else {
                g.on_form(tag, &[("bus".into(), "scsi".into())], &mut ctx);
            }
            for (m, path, p) in g.sent.borrow().iter() {
                let pp: Vec<(&str, String)> = p.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                if let Err(e) = if m == "POST" { api.post(path, &pp) } else { api.put(path, &pp) } {
                    println!("FAIL {tag}: {e:#}");
                }
            }
            let d = page();
            let ok = if key.is_empty() { !d.d().unwrap().exists("unused0") } else { d.d().unwrap().exists(key) };
            println!("{} {tag:<22} -> {key}", if ok { "ok  " } else { "FAIL" });
            if !ok {
                failed.push(tag);
            }
        }
        assert!(failed.is_empty(), "failed: {failed:?}");
    }

    #[test]
    fn ct_items_and_mountpoint() {
        let g = fake(
            false,
            &[
                ("memory", Some("1024"), None, false),
                ("swap", Some("512"), None, false),
                ("rootfs", Some("local-lvm:vm-200-disk-0,size=8G"), None, false),
                ("net0", Some("name=eth0,bridge=vmbr0,hwaddr=BC:24:11:00:00:02,ip=dhcp,type=veth"), None, false),
                ("hostname", Some("web"), None, false),
                ("unprivileged", Some("1"), None, false),
                ("features", Some("nesting=1"), None, false),
            ],
        );
        let kinds: Vec<(String, K)> = g.items().iter().map(|i| (i.key.clone(), i.kind)).collect();
        assert!(kinds.contains(&("rootfs".into(), K::Rootfs)));
        assert!(kinds.contains(&("net0".into(), K::CtNet)));
        assert!(kinds.contains(&("unprivileged".into(), K::ReadOnly)));
        let mut ctx = test_ctx();
        g.clone_on_form("gc:addmp:", &vals(&[("storage", "local-lvm"), ("size", "16"), ("hostpath", ""), ("mp", "/data"), ("backup", "1")]), &mut ctx);
        g.clone_on_form("gc:features:features", &vals(&[("nesting", "1"), ("keyctl", "1"), ("fuse", "0"), ("mount", "")]), &mut ctx);
        let s = sent(&g);
        assert_eq!(s[0].0, "PUT");
        assert_eq!(param(&s[0].2, "mp0"), Some("local-lvm:16,mp=/data,backup=1"));
        assert_eq!(param(&s[1].2, "features"), Some("nesting=1,keyctl=1"));
    }

    #[test]
    fn prop_round_trip_keeps_unknown_options() {
        let s = "virtio=BC:24:11:00:00:01,bridge=vmbr0,firewall=1,queues=4";
        let mut p = Prop::parse(s);
        assert_eq!(p.render(), s);
        p.set("tag", "18");
        p.flag("firewall", false, false, "1", "0");
        assert_eq!(p.render(), "virtio=BC:24:11:00:00:01,bridge=vmbr0,queues=4,tag=18");
    }

    #[test]
    fn prop_positional_first() {
        let p = Prop::parse("local-lvm:vm-100-disk-0,iothread=1,size=32G");
        assert_eq!(p.first.as_deref(), Some("local-lvm:vm-100-disk-0"));
        assert_eq!(p.get("size"), Some("32G"));
        assert!(p.is_on("iothread", false));
        assert!(p.is_on("backup", true));
        assert_eq!(Prop::parse("virtio").render(), "virtio");
    }

    #[test]
    fn keys_and_slots() {
        assert_eq!(split_key("scsi10"), ("scsi", Some(10)));
        assert_eq!(split_key("rootfs"), ("rootfs", None));
        assert!(dev_rank("scsi2") < dev_rank("scsi10"));
        assert!(is_cd("none,media=cdrom"));
        assert_eq!(vol_store("local-lvm:vm-1-disk-0,size=8G"), "local-lvm");
    }

    #[test]
    fn opt_set_only_writes_changes() {
        let (mut s, mut d) = (vec![], vec![]);
        opt_set(&mut s, &mut d, "a", "", "x");
        opt_set(&mut s, &mut d, "b", "1", "1");
        opt_set(&mut s, &mut d, "c", "2", "");
        assert_eq!(d, vec!["a".to_string()]);
        assert_eq!(s, vec![("c".to_string(), "2".to_string())]);
    }
}
