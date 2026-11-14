//! Marvell mvcli / mnv_cli (Dell BOSS boot cards): `info -o hba|vd|pd`, blocks of
//! "key: value" lines separated by blank lines. Field names vary, so they are looked up loosely.

use anyhow::Result;

use super::query;
use crate::model::{Controller, PhysicalDrive, Role, VirtualDrive};
use crate::spec::{Found, ToolSpec};
use crate::text::{blocks, kv, pick};
use crate::Runner;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    let hba = query(r, f, "hba", "")?;
    let vd = query(r, f, "vd", "").unwrap_or_default();
    let pd = query(r, f, "pd", "").unwrap_or_default();
    Ok(vec![parse(f.spec, &hba, &vd, &pd)])
}

fn props(b: &[&str]) -> Vec<(String, String)> {
    b.iter().filter_map(|l| kv(l)).collect()
}

pub fn parse(spec: &ToolSpec, hba: &str, vd: &str, pd: &str) -> Controller {
    let hp: Vec<(String, String)> = blocks(hba).first().map(|b| props(b)).unwrap_or_default();
    let mut c = Controller {
        id: "0".into(),
        addr: vec![("ctl".into(), "0".into())],
        model: pick(&hp, &["Product", "Model", "Name"]),
        firmware: pick(&hp, &["Firmware version", "Firmware", "FW version"]),
        status: "-".into(),
        health: spec.health("ok"),
        props: hp,
        ..Default::default()
    };
    for b in blocks(vd) {
        let p = props(&b);
        let id = pick(&p, &["id", "VD ID"]);
        let state = pick(&p, &["status", "state"]);
        if id.is_empty() && state.is_empty() {
            continue;
        }
        c.vds.push(VirtualDrive {
            addr: vec![("ctl".into(), "0".into()), ("vd".into(), id.clone())],
            id,
            name: pick(&p, &["name"]),
            level: pick(&p, &["RAID mode", "RAID level"]),
            size: pick(&p, &["size"]),
            health: spec.health(&state),
            state,
            progress: pick(&p, &["BGA status", "background"]),
            props: p,
            ..Default::default()
        });
    }
    for b in blocks(pd) {
        let p = props(&b);
        let id = pick(&p, &["PD ID", "id"]);
        if id.is_empty() {
            continue;
        }
        let state = pick(&p, &["status", "state"]);
        let assigned = pick(&p, &["Assigned", "Belong to VD", "VD ID"]);
        c.pds.push(PhysicalDrive {
            addr: vec![("ctl".into(), "0".into()), ("pd".into(), id.clone())],
            id: id.clone(),
            model: pick(&p, &["model"]),
            serial: pick(&p, &["Serial"]),
            firmware: pick(&p, &["FW version", "Firmware"]),
            size: pick(&p, &["size"]),
            media: "SSD".into(),
            intf: "SATA".into(),
            health: if state.is_empty() { spec.health("ok") } else { spec.health(&state) },
            state: if state.is_empty() { "-".into() } else { state },
            role: if assigned.is_empty() || assigned.eq_ignore_ascii_case("no") { Role::Unconfigured } else { Role::Member },
            token: id,
            props: p,
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
    fn boss_blocks() {
        let hba = "Adapter ID:     0\nProduct:        1b4b-9230\nFirmware version:  2.5.13.3024\n";
        let vd = "id:                  0\nname:                VD_0\nstatus:              degraded\nRAID mode:           RAID1\nsize:                228872 MB\n";
        let pd = "PD ID:               0\nmodel:               MTFDDAV240TCB\nSerial:              1\nSize:                234431 MB\n\nPD ID:               1\nmodel:               MTFDDAV240TCB\nSize:                234431 MB\n";
        let c = parse(crate::spec("mvcli").unwrap(), hba, vd, pd);
        assert_eq!(c.firmware, "2.5.13.3024");
        assert_eq!((c.vds.len(), c.vds[0].health, c.vds[0].level.as_str()), (1, Health::Warning, "RAID1"));
        assert_eq!(c.pds.len(), 2);
    }
}
