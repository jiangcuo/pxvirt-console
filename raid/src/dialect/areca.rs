//! Areca cli64: `sys info` (key : value), `vsf info` (volume sets), `disk info` (drives).
//! Tables have free-text columns ("Raid Set # 000"), so rows are parsed from both ends.

use anyhow::Result;

use super::query;
use crate::model::{Controller, PhysicalDrive, Role, VirtualDrive};
use crate::spec::{Found, ToolSpec};
use crate::text::{kv, pick};
use crate::Runner;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    let sys = query(r, f, "sys", "1")?;
    let vsf = query(r, f, "vsf", "1").unwrap_or_default();
    let disk = query(r, f, "disk", "1").unwrap_or_default();
    Ok(vec![parse(f.spec, "1", &sys, &vsf, &disk)])
}

fn rows(text: &str) -> impl Iterator<Item = Vec<&str>> {
    text.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|w| w.first().map(|x| x.chars().all(|c| c.is_ascii_digit())).unwrap_or(false))
}

fn is_cap(s: &str) -> bool {
    let u = s.to_uppercase();
    (u.ends_with("GB") || u.ends_with("TB") || u.ends_with("MB")) && u[..u.len() - 2].parse::<f64>().is_ok()
}

pub fn parse(spec: &ToolSpec, id: &str, sys: &str, vsf: &str, disk: &str) -> Controller {
    let sp: Vec<(String, String)> = sys.lines().filter_map(kv).filter(|(k, _)| !k.starts_with("GuiErrMsg")).collect();
    let mut c = Controller {
        id: id.to_string(),
        addr: vec![("ctl".into(), id.to_string())],
        model: pick(&sp, &["Controller Name", "Main Processor"]),
        serial: pick(&sp, &["Serial Number"]),
        firmware: pick(&sp, &["Firmware Version"]),
        status: "-".into(),
        health: spec.health("normal"),
        props: sp,
        ..Default::default()
    };
    // # Name Raid-Set-Name... Level Capacity Ch/Id/Lun State
    for w in rows(vsf) {
        if w.len() < 6 {
            continue;
        }
        let n = w.len();
        let state = w[n - 1].to_string();
        c.vds.push(VirtualDrive {
            addr: vec![("ctl".into(), id.to_string()), ("vd".into(), w[0].to_string())],
            id: w[0].to_string(),
            name: w[1].to_string(),
            level: w[n - 4].to_string(),
            size: w[n - 3].to_string(),
            health: spec.health(&state),
            progress: state.split_once('(').map(|x| x.1.trim_end_matches(')').to_string()).unwrap_or_default(),
            state,
            props: vec![("Raid set".into(), w[2..n - 4].join(" ")), ("Ch/Id/Lun".into(), w[n - 2].to_string())],
            ..Default::default()
        });
    }
    // # [Enc#] [Slot#] Model... Capacity Usage...
    for w in rows(disk) {
        let Some(ci) = w.iter().position(|x| is_cap(x)) else { continue };
        let usage = w[ci + 1..].join(" ");
        if usage == "N.A." || w[ci].starts_with("0.0") {
            continue; // empty slot
        }
        let lead = w[1..ci].iter().take_while(|x| x.chars().all(|c| c.is_ascii_digit()) || x.starts_with("Slot#") || x.starts_with("PHY#")).count();
        let u = usage.to_lowercase();
        let (role, state) = if u.starts_with("raid set") {
            (Role::Member, "Normal".to_string())
        } else if u.contains("hotspare") {
            (Role::HotSpare, "Hot spare".to_string())
        } else if u.contains("free") {
            (Role::Unconfigured, "Free".to_string())
        } else if u.contains("pass") {
            (Role::Jbod, "Pass through".to_string())
        } else if u.contains("fail") {
            (Role::Failed, "Failed".to_string())
        } else {
            (Role::Other, usage.clone())
        };
        c.pds.push(PhysicalDrive {
            addr: vec![("ctl".into(), id.to_string()), ("pd".into(), w[0].to_string())],
            id: w[0].to_string(),
            model: w[1 + lead..ci].join(" "),
            size: w[ci].to_string(),
            health: spec.health(&state),
            state,
            role,
            group: if role == Role::Member { usage.clone() } else { String::new() },
            token: w[0].to_string(),
            props: vec![("Usage".into(), usage)],
            ..Default::default()
        });
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Health;

    #[test]
    fn arc1882() {
        let vsf = "  # Name             Raid Name       Level   Capacity Ch/Id/Lun  State         \n===============================================================================\n  1 ARC-1882-VOL#000 Raid Set # 000  Raid1+0  480.1GB 00/00/00   Normal\n  2 ARC-1882-VOL#002 Raid Set # 002  Raid5    600.1GB 00/00/01   Degraded\n===============================================================================\nGuiErrMsg<0x00>: Success.\n";
        let disk = "  # Enc# Slot#   ModelName                        Capacity  Usage\n===============================================================================\n  1  01  Slot#1  SAMSUNG MZ7KM480HAHP-0E005       480.1GB  Raid Set # 000\n  2  01  Slot#2  N.A.                                0.0GB  N.A.\n  3  01  Slot#3  ST4000NM0035                    4000.8GB  Free\n  4  01  Slot#4  ST4000NM0035                    4000.8GB  Failed\nGuiErrMsg<0x00>: Success.\n";
        let c = parse(crate::spec("areca").unwrap(), "1", "Firmware Version : V1.56 2019-07-30\nSerial Number : A1\n", vsf, disk);
        assert_eq!(c.firmware, "V1.56 2019-07-30");
        assert_eq!(c.vds.len(), 2);
        assert_eq!((c.vds[0].level.as_str(), c.vds[0].health), ("Raid1+0", Health::Ok));
        assert_eq!(c.vds[1].health, Health::Warning);
        assert_eq!(c.pds.len(), 3);
        assert_eq!((c.pds[0].model.as_str(), c.pds[0].role), ("SAMSUNG MZ7KM480HAHP-0E005", Role::Member));
        assert_eq!(c.pds[1].role, Role::Unconfigured);
        assert_eq!((c.pds[2].role, c.pds[2].health), (Role::Failed, Health::Critical));
    }
}
