//! Create VM / CT wizards (forms filled from the current node's storages, ISOs, bridges).

use serde_json::Value;

use super::fv;
use super::pve::arch_norm;
use crate::api::Api;
use crate::ctx::Ctx;
use crate::ui::form::{self, Form};
use crate::util::{fmt_bytes, jarr, jf, js};

pub struct CreateInfo {
    pub node: String,
    pub arch: String,
    pub nextid: String,
    /// (storage, label) with content images
    pub disk_storages: Vec<(String, String)>,
    /// (storage, label) with content rootdir
    pub ct_storages: Vec<(String, String)>,
    pub isos: Vec<(String, String)>,
    pub templates: Vec<(String, String)>,
    pub bridges: Vec<(String, String)>,
}

pub fn load_info(api: &Api, node: &str, known_arch: &str) -> anyhow::Result<CreateInfo> {
    let nextid = api.get("/cluster/nextid").map(|v| v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string())).unwrap_or_default();
    let arch = if known_arch.is_empty() {
        api.get(&format!("/nodes/{node}/status")).ok().and_then(|s| s.get("current-kernel").map(|k| js(k, "machine"))).map(|a| arch_norm(&a)).unwrap_or_else(|| "x86_64".into())
    } else {
        known_arch.to_string()
    };
    let storages = jarr(&api.get(&format!("/nodes/{node}/storage?enabled=1"))?);
    let label = |s: &Value| format!("{} ({}, {} free)", js(s, "storage"), js(s, "type"), fmt_bytes(jf(s, "avail")));
    let disk_storages = storages.iter().filter(|s| js(s, "content").contains("images")).map(|s| (js(s, "storage"), label(s))).collect();
    let ct_storages = storages.iter().filter(|s| js(s, "content").contains("rootdir")).map(|s| (js(s, "storage"), label(s))).collect();
    let mut isos = vec![("none".to_string(), "(no CD/DVD)".to_string())];
    let mut templates = Vec::new();
    for s in &storages {
        let content = js(s, "content");
        let name = js(s, "storage");
        if content.contains("iso") {
            for c in jarr(&api.get(&format!("/nodes/{node}/storage/{name}/content?content=iso")).unwrap_or(Value::Null)) {
                let v = js(&c, "volid");
                isos.push((v.clone(), format!("{v} ({})", fmt_bytes(jf(&c, "size")))));
            }
        }
        if content.contains("vztmpl") {
            for c in jarr(&api.get(&format!("/nodes/{node}/storage/{name}/content?content=vztmpl")).unwrap_or(Value::Null)) {
                let v = js(&c, "volid");
                templates.push((v.clone(), v));
            }
        }
    }
    let bridges = jarr(&api.get(&format!("/nodes/{node}/network?type=any_bridge")).unwrap_or(Value::Null))
        .iter()
        .map(|b| (js(b, "iface"), format!("{} {}", js(b, "iface"), js(b, "comments").trim())))
        .collect();
    Ok(CreateInfo { node: node.to_string(), arch, nextid, disk_storages, ct_storages, isos, templates, bridges })
}

const ARCHES: [&str; 4] = ["x86_64", "aarch64", "loongarch64", "riscv64"];

pub fn vm_form(i: &CreateInfo) -> Form {
    let arches: Vec<(String, String)> = ARCHES
        .iter()
        .map(|a| (a.to_string(), if *a == i.arch { format!("{a} (native, KVM)") } else { format!("{a} (emulated, TCG)") }))
        .collect();
    Form::new(
        &format!("Create VM on {}", i.node),
        "create_vm",
        vec![
            form::info("Node", &format!("{} ({})", i.node, i.arch)),
            form::number("vmid", "VM ID", &i.nextid),
            form::text("name", "Name", "").req(true),
            form::select("arch", "Architecture", arches, &i.arch).help("other architectures run emulated (slow), root@pam only"),
            form::select("ostype", "Guest OS", form::opts(&["l26", "win11", "win10", "w2k22", "w2k19", "other"]), "l26"),
            form::select("iso", "CD/DVD image", i.isos.clone(), "none"),
            form::select("bios", "Firmware", vec![("seabios".into(), "SeaBIOS (legacy)".into()), ("ovmf".into(), "OVMF (UEFI)".into())], if i.arch == "x86_64" { "seabios" } else { "ovmf" })
                .help("non-x86 guests always use UEFI"),
            form::number("sockets", "Sockets", "1"),
            form::number("cores", "Cores", "2"),
            form::number("memory", "Memory (MiB)", "2048"),
            form::select("storage", "Disk storage", i.disk_storages.clone(), ""),
            form::number("disksize", "Disk size (GiB)", "32"),
            form::select("bus", "Disk bus", form::opts(&["scsi", "virtio", "sata"]), "scsi"),
            form::select("bridge", "Network bridge", i.bridges.clone(), "vmbr0"),
            form::select("netmodel", "NIC model", form::opts(&["virtio", "e1000", "rtl8139", "vmxnet3"]), "virtio"),
            form::text("vlan", "VLAN tag", ""),
            form::check("agent", "QEMU guest agent", true),
            form::check("onboot", "Start at boot", false),
            form::check("start", "Start after creation", false),
        ],
    )
}

pub fn ct_form(i: &CreateInfo) -> Form {
    Form::new(
        &format!("Create CT on {}", i.node),
        "create_ct",
        vec![
            form::info("Node", &format!("{} ({})", i.node, i.arch)),
            form::number("vmid", "CT ID", &i.nextid),
            form::text("hostname", "Hostname", "").req(true),
            form::password("password", "Root password"),
            form::password("confirm", "Confirm password"),
            form::select("template", "Template", i.templates.clone(), "").req(true).help("download templates in PVE > Storage (T)"),
            form::select("storage", "Root disk storage", i.ct_storages.clone(), ""),
            form::number("disksize", "Disk size (GiB)", "8"),
            form::number("cores", "Cores", "1"),
            form::number("memory", "Memory (MiB)", "512"),
            form::number("swap", "Swap (MiB)", "512"),
            form::select("bridge", "Network bridge", i.bridges.clone(), "vmbr0"),
            form::text("ip", "IPv4 (CIDR or dhcp)", "dhcp").req(true),
            form::text("gw", "Gateway", ""),
            form::text("vlan", "VLAN tag", ""),
            form::check("unprivileged", "Unprivileged", true),
            form::check("nesting", "Nesting", true),
            form::check("onboot", "Start at boot", false),
            form::check("start", "Start after creation", false),
        ],
    )
}

pub fn submit(tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
    let node = ctx.node.clone();
    let node_arch = ctx.node_arch.get(&node).cloned().unwrap_or_else(|| "x86_64".into());
    if tag == "create_vm" {
        let arch = fv(v, "arch").to_string();
        let foreign = !arch.is_empty() && arch != node_arch;
        let storage = fv(v, "storage").to_string();
        if storage.is_empty() {
            ctx.popup(crate::ui::popup::Popup::error("Create VM", "no storage with content 'images' on this node"));
            return;
        }
        let bus = fv(v, "bus").to_string();
        let mut p: Vec<(&'static str, String)> = vec![
            ("vmid", fv(v, "vmid").into()),
            ("name", fv(v, "name").into()),
            ("ostype", fv(v, "ostype").into()),
            ("sockets", fv(v, "sockets").into()),
            ("cores", fv(v, "cores").into()),
            ("memory", fv(v, "memory").into()),
            ("scsihw", "virtio-scsi-single".into()),
        ];
        let disk_key: &'static str = match bus.as_str() {
            "virtio" => "virtio0",
            "sata" => "sata0",
            _ => "scsi0",
        };
        p.push((disk_key, format!("{storage}:{},iothread={}", fv(v, "disksize"), if bus == "sata" { "0" } else { "1" }).replace(",iothread=0", "")));
        let mut net = format!("{},bridge={}", fv(v, "netmodel"), fv(v, "bridge"));
        if !fv(v, "vlan").is_empty() {
            net.push_str(&format!(",tag={}", fv(v, "vlan")));
        }
        p.push(("net0", net));
        let mut bios = fv(v, "bios").to_string();
        if arch != "x86_64" {
            bios = "ovmf".into();
            p.push(("vga", "virtio".into()));
        }
        let iso = fv(v, "iso").to_string();
        // x86 uses IDE for the CD drive; other machines have no IDE controller
        let cd_key: &'static str = if arch == "x86_64" { "ide2" } else { "scsi2" };
        if iso != "none" && !iso.is_empty() {
            p.push((cd_key, format!("{iso},media=cdrom")));
            p.push(("boot", format!("order={disk_key};{cd_key}")));
        } else {
            p.push(("boot", format!("order={disk_key}")));
        }
        p.push(("bios", bios.clone()));
        if bios == "ovmf" {
            p.push(("efidisk0", format!("{storage}:1,efitype=4m{}", if arch == "x86_64" { ",pre-enrolled-keys=1" } else { "" })));
        }
        if foreign {
            p.push(("arch", arch.clone()));
            p.push(("kvm", "0".into()));
        }
        if fv(v, "agent") == "1" {
            p.push(("agent", "1".into()));
        }
        for k in ["onboot", "start"] {
            if fv(v, k) == "1" {
                p.push((k, "1".into()));
            }
        }
        ctx.api_call(format!("Create VM {} {}", fv(v, "vmid"), fv(v, "name")), "POST", format!("/nodes/{node}/qemu"), p);
    } else {
        if fv(v, "password") != fv(v, "confirm") {
            ctx.popup(crate::ui::popup::Popup::error("Create CT", "Passwords do not match"));
            return;
        }
        if fv(v, "password").len() < 5 {
            ctx.popup(crate::ui::popup::Popup::error("Create CT", "Password is too short (min. 5 characters)"));
            return;
        }
        let storage = fv(v, "storage").to_string();
        if storage.is_empty() {
            ctx.popup(crate::ui::popup::Popup::error("Create CT", "no storage with content 'rootdir' on this node"));
            return;
        }
        let ip = fv(v, "ip");
        let mut net = format!("name=eth0,bridge={},ip={ip}", fv(v, "bridge"));
        if !fv(v, "gw").is_empty() && ip != "dhcp" {
            net.push_str(&format!(",gw={}", fv(v, "gw")));
        }
        if !fv(v, "vlan").is_empty() {
            net.push_str(&format!(",tag={}", fv(v, "vlan")));
        }
        let mut p: Vec<(&'static str, String)> = vec![
            ("vmid", fv(v, "vmid").into()),
            ("hostname", fv(v, "hostname").into()),
            ("password", fv(v, "password").into()),
            ("ostemplate", fv(v, "template").into()),
            ("rootfs", format!("{storage}:{}", fv(v, "disksize"))),
            ("cores", fv(v, "cores").into()),
            ("memory", fv(v, "memory").into()),
            ("swap", fv(v, "swap").into()),
            ("net0", net),
            ("unprivileged", fv(v, "unprivileged").into()),
        ];
        if fv(v, "nesting") == "1" {
            p.push(("features", "nesting=1".into()));
        }
        for k in ["onboot", "start"] {
            if fv(v, k) == "1" {
                p.push((k, "1".into()));
            }
        }
        ctx.api_call(format!("Create CT {} {}", fv(v, "vmid"), fv(v, "hostname")), "POST", format!("/nodes/{node}/lxc"), p);
    }
}
