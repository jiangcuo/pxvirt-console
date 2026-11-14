//! nvme-cli (`-o json`). Each NVMe controller (nvme0) becomes a controller with its namespaces as
//! virtual drives and the SSD itself as one physical drive carrying the SMART health.
//!
//! nvme-cli 2.x: `list -v` = subsystems -> controllers -> namespaces; counters in `smart-log`
//! are strings with thousands separators, temperatures are Kelvin.
//! nvme-cli 1.x: `list` = flat namespace list, grouped here by controller name.

use anyhow::Result;
use serde_json::Value;

use super::{bytes, json_of, props_of, query, s};
use crate::model::{Controller, Health, PhysicalDrive, Role, VirtualDrive};
use crate::spec::Found;
use crate::Runner;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    // nvme-cli 1.x has no verbose JSON list
    let list = query(r, f, "list", "").and_then(|o| json_of(&o)).or_else(|_| query(r, f, "list_flat", "").and_then(|o| json_of(&o)))?;
    let mut ctls = parse_list(&list);
    for c in &mut ctls {
        if let Some(v) = query(r, f, "smart", &c.id).ok().and_then(|o| json_of(&o).ok()) {
            apply_smart(c, &v);
        }
    }
    Ok(ctls)
}

/// Number from a JSON number or a "1,234" string.
fn num(v: &Value, k: &str) -> f64 {
    match v.get(k) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.replace(',', "").trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn ctl_of_ns(path: &str) -> String {
    // /dev/nvme0n1 -> nvme0
    let n = path.rsplit('/').next().unwrap_or(path);
    match n.rfind('n') {
        Some(i) if i > 4 => n[..i].to_string(),
        _ => n.to_string(),
    }
}

pub fn parse_list(v: &Value) -> Vec<Controller> {
    let mut out: Vec<Controller> = Vec::new();
    for dev in v.get("Devices").and_then(|d| d.as_array()).into_iter().flatten() {
        if let Some(subs) = dev.get("Subsystems").and_then(|s| s.as_array()) {
            // nvme-cli 2.x verbose
            for sub in subs {
                let namespaces: Vec<&Value> = sub.get("Namespaces").and_then(|n| n.as_array()).map(|a| a.iter().collect()).unwrap_or_default();
                for ctl in sub.get("Controllers").and_then(|c| c.as_array()).into_iter().flatten() {
                    let name = s(ctl, "Controller");
                    let mut c = controller(&name, &s(ctl, "ModelNumber"), &s(ctl, "SerialNumber"), &s(ctl, "Firmware"));
                    c.pci = s(ctl, "Address");
                    c.mode = format!("NVMe {}", s(ctl, "Transport"));
                    c.props.push(("Subsystem".into(), s(sub, "Subsystem")));
                    c.props.push(("NQN".into(), s(sub, "SubsystemNQN")));
                    let own: Vec<&Value> = ctl.get("Namespaces").and_then(|n| n.as_array()).map(|a| a.iter().collect()).unwrap_or_default();
                    for ns in if own.is_empty() { &namespaces } else { &own } {
                        c.vds.push(namespace(&name, &s(ns, "NameSpace"), num(ns, "NSID"), num(ns, "PhysicalSize"), num(ns, "UsedBytes"), num(ns, "SectorSize")));
                    }
                    if let Some(pd) = c.pds.first_mut() {
                        pd.size = c.vds.first().map(|v| v.size.clone()).unwrap_or_default();
                    }
                    out.push(c);
                }
            }
        } else {
            // nvme-cli 1.x flat list
            let path = s(dev, "DevicePath");
            let name = ctl_of_ns(&path);
            if !out.iter().any(|c| c.id == name) {
                out.push(controller(&name, &s(dev, "ModelNumber"), &s(dev, "SerialNumber"), &s(dev, "Firmware")));
            }
            let c = out.iter_mut().find(|c| c.id == name).expect("just added");
            let ns = path.rsplit('/').next().unwrap_or("").to_string();
            c.vds.push(namespace(&name, &ns, num(dev, "NameSpace"), num(dev, "PhysicalSize"), num(dev, "UsedBytes"), num(dev, "SectorSize")));
        }
    }
    out
}

fn controller(name: &str, model: &str, serial: &str, fw: &str) -> Controller {
    Controller {
        id: name.to_string(),
        addr: vec![("ctl".into(), name.to_string())],
        model: model.trim().to_string(),
        serial: serial.trim().to_string(),
        firmware: fw.trim().to_string(),
        driver: "nvme".into(),
        status: "-".into(),
        health: Health::Unknown,
        pds: vec![PhysicalDrive {
            addr: vec![("ctl".into(), name.to_string())],
            id: name.to_string(),
            model: model.trim().to_string(),
            serial: serial.trim().to_string(),
            firmware: fw.trim().to_string(),
            media: "SSD".into(),
            intf: "NVMe".into(),
            role: Role::Jbod,
            os_dev: format!("/dev/{name}"),
            token: name.to_string(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn namespace(ctl: &str, ns: &str, nsid: f64, size: f64, used: f64, sector: f64) -> VirtualDrive {
    VirtualDrive {
        addr: vec![("ctl".into(), ctl.to_string()), ("ns".into(), ns.to_string()), ("nsid".into(), format!("{nsid:.0}"))],
        id: ns.to_string(),
        level: "namespace".into(),
        size: bytes(size),
        state: "active".into(),
        health: Health::Ok,
        os_dev: format!("/dev/{ns}"),
        props: vec![("NSID".into(), format!("{nsid:.0}")), ("Used".into(), bytes(used)), ("Sector size".into(), format!("{sector:.0}"))],
        ..Default::default()
    }
}

/// Critical warning bits of the SMART / health log.
fn critical_bits(cw: u64) -> Vec<&'static str> {
    let names = [
        "spare below threshold",
        "temperature",
        "reliability degraded",
        "read-only",
        "volatile backup failed",
        "persistent memory read-only",
    ];
    names.iter().enumerate().filter(|(i, _)| cw & (1 << i) != 0).map(|(_, n)| *n).collect()
}

pub fn apply_smart(c: &mut Controller, v: &Value) {
    let cw = num(v, "critical_warning") as u64;
    let used = num(v, "percent_used");
    let spare = num(v, "avail_spare");
    let thresh = num(v, "spare_thresh");
    let media = num(v, "media_errors");
    let temp_k = num(v, "temperature");
    let mut problems: Vec<String> = critical_bits(cw).iter().map(|s| s.to_string()).collect();
    let mut health = if cw != 0 { Health::Critical } else { Health::Ok };
    if used >= 90.0 {
        health = health.max(Health::Warning);
        problems.push(format!("{used:.0}% of the rated endurance used"));
    }
    if media > 0.0 {
        health = health.max(Health::Warning);
        problems.push(format!("{media:.0} media errors"));
    }
    let status = if problems.is_empty() { "healthy".to_string() } else { problems.join(", ") };
    c.status = status.clone();
    c.health = health;
    // data units are thousands of 512-byte blocks
    let written = num(v, "data_units_written") * 512_000.0;
    let read = num(v, "data_units_read") * 512_000.0;
    c.props.extend([
        ("Endurance used".to_string(), format!("{used:.0}%")),
        ("Available spare".to_string(), format!("{spare:.0}% (threshold {thresh:.0}%)")),
        ("Data written".to_string(), bytes(written)),
        ("Data read".to_string(), bytes(read)),
        ("Power-on hours".to_string(), format!("{:.0}", num(v, "power_on_hours"))),
        ("Power cycles".to_string(), format!("{:.0}", num(v, "power_cycles"))),
        ("Unsafe shutdowns".to_string(), format!("{:.0}", num(v, "unsafe_shutdowns"))),
        ("Media errors".to_string(), format!("{media:.0}")),
    ]);
    if let Some(pd) = c.pds.first_mut() {
        pd.state = status;
        pd.health = health;
        if temp_k > 0.0 {
            pd.temp_c = Some((temp_k - 273.0).round() as i32);
        }
        pd.props = props_of(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::testutil::{fixture, found, Fake};

    #[test]
    fn samsung_pm1733() {
        let d = "nvme/samsung-pm1733";
        let r = Fake::new(&[("list -v -o json", &fixture(&format!("{d}/list_v.json"))), ("smart-log /dev/nvme0 -o json", &fixture(&format!("{d}/smart_log.json")))]);
        let c = crate::scan(&r, &found("nvme")).unwrap();
        assert_eq!(c.len(), 1);
        let c = &c[0];
        assert_eq!((c.id.as_str(), c.model.as_str(), c.pci.as_str()), ("nvme0", "SAMSUNG MZPLJ1T6HBJR-0007C", "0000:86:00.0"));
        assert_eq!((c.health, c.status.as_str()), (Health::Ok, "healthy"));
        assert_eq!(c.vds.len(), 1);
        assert_eq!((c.vds[0].id.as_str(), c.vds[0].size.as_str(), c.vds[0].os_dev.as_str()), ("nvme0n1", "1.60 TB", "/dev/nvme0n1"));
        assert_eq!(c.pds[0].temp_c, Some(38));
        assert!(c.props.iter().any(|(k, v)| k == "Power-on hours" && v == "11017"));
    }

    #[test]
    fn flat_list_and_warnings() {
        let v: Value = serde_json::from_str(r#"{"Devices":[{"DevicePath":"/dev/nvme1n1","ModelNumber":"X","SerialNumber":"S","Firmware":"F","NameSpace":1,"PhysicalSize":1000,"UsedBytes":1,"SectorSize":512},{"DevicePath":"/dev/nvme1n2","ModelNumber":"X","SerialNumber":"S","Firmware":"F","NameSpace":2,"PhysicalSize":1000,"UsedBytes":1,"SectorSize":512}]}"#).unwrap();
        let mut c = parse_list(&v);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].vds.len(), 2);
        apply_smart(&mut c[0], &serde_json::json!({"critical_warning": 4, "percent_used": 95, "media_errors": "3", "temperature": 330}));
        assert_eq!(c[0].health, Health::Critical);
        assert_eq!(c[0].status, "reliability degraded, 95% of the rated endurance used, 3 media errors");
        assert_eq!(c[0].pds[0].temp_c, Some(57));
    }
}
