//! HighPoint hptraidconf: `query arrays` and `query devices` (whitespace tables, the last
//! column is free text).

use anyhow::Result;

use super::query;
use crate::model::{Controller, PhysicalDrive, Role, VirtualDrive};
use crate::spec::{Found, ToolSpec};
use crate::Runner;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    let arrays = query(r, f, "arrays", "")?;
    let devices = query(r, f, "devices", "").unwrap_or_default();
    Ok(vec![parse(f.spec, &arrays, &devices)])
}

fn data_rows(text: &str) -> impl Iterator<Item = Vec<&str>> {
    text.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|w| w.first().map(|x| x.starts_with(|c: char| c.is_ascii_digit())).unwrap_or(false))
}

pub fn parse(spec: &ToolSpec, arrays: &str, devices: &str) -> Controller {
    let mut c = Controller { id: "1".into(), addr: vec![("ctl".into(), "1".into())], model: "HighPoint RAID".into(), status: "-".into(), health: spec.health("normal"), ..Default::default() };
    // ID Capacity(GB) Type Status Block Sector Cache Name
    for w in data_rows(arrays) {
        if w.len() < 4 {
            continue;
        }
        let state = w[3].to_string();
        c.vds.push(VirtualDrive {
            addr: vec![("ctl".into(), "1".into()), ("vd".into(), w[0].to_string())],
            id: w[0].to_string(),
            level: w[2].to_string(),
            size: format!("{} GB", w[1]),
            health: spec.health(&state),
            state,
            name: w.get(7..).map(|x| x.join(" ")).unwrap_or_default(),
            ..Default::default()
        });
    }
    // ID Capacity MaxFree Flag Status ModelNumber
    for w in data_rows(devices) {
        if w.len() < 5 {
            continue;
        }
        let state = w[4].to_string();
        let flag = w[3].to_uppercase();
        let role = match state.to_uppercase().as_str() {
            "SPARE" => Role::HotSpare,
            "DISABLED" => Role::Failed,
            _ if flag == "RAID" => Role::Member,
            _ if flag == "SINGLE" => Role::Unconfigured,
            _ => Role::Other,
        };
        c.pds.push(PhysicalDrive {
            addr: vec![("ctl".into(), "1".into()), ("pd".into(), w[0].to_string())],
            id: w[0].to_string(),
            size: format!("{} GB", w[1]),
            model: w.get(5..).map(|x| x.join(" ")).unwrap_or_default(),
            health: spec.health(&state),
            state,
            role,
            token: w[0].to_string(),
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
    fn ssd7104() {
        let a = "ID   Capacity(GB)  Type   Status    Block  Sector  Cache  Name\n-------------------------------------------------------------\n1    500.03        RAID1  NORMAL    --     512B    NONE   RAID_1_0\n2    1000.00       RAID0  CRITICAL  64k    512B    NONE   scratch\n";
        let d = "ID      Capacity  MaxFree  Flag    Status   ModelNumber\n-------------------------------------------------------------\n1/E1/1  250.05    0        RAID    NORMAL   Samsung SSD 970 EVO\n1/E1/2  250.05    0        RAID    DISABLED Samsung SSD 970 EVO\n";
        let c = parse(crate::spec("hptraidconf").unwrap(), a, d);
        assert_eq!(c.vds.len(), 2);
        assert_eq!((c.vds[0].name.as_str(), c.vds[1].health), ("RAID_1_0", Health::Critical));
        assert_eq!(c.pds.len(), 2);
        assert_eq!((c.pds[0].model.as_str(), c.pds[0].role), ("Samsung SSD 970 EVO", Role::Member));
        assert_eq!((c.pds[1].role, c.pds[1].health), (Role::Failed, Health::Critical));
    }
}
