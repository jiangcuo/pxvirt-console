//! Huawei hiraidadm (SP186 / SP686C, Hi1880 RAID chip): `show allctrl`, `c<n> show vdlist`,
//! `c<n> show pdlist` tables. Column names from the Huawei RAID controller card user guide;
//! not verified against real output yet, so columns are looked up by name.

use anyhow::Result;

use super::query;
use crate::model::{Controller, PhysicalDrive, Role, VirtualDrive};
use crate::spec::{Found, ToolSpec};
use crate::text::{kv, pick, table};
use crate::Runner;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    let all = query(r, f, "ctrls", "")?;
    let mut ids = controller_ids(&all);
    if ids.is_empty() {
        ids.push("0".into());
    }
    let mut out = Vec::new();
    for id in ids {
        let vd = query(r, f, "vdlist", &id).unwrap_or_default();
        let pd = query(r, f, "pdlist", &id).unwrap_or_default();
        let st = query(r, f, "status", &id).unwrap_or_default();
        out.push(parse(f.spec, &id, &st, &vd, &pd));
    }
    Ok(out)
}

/// Controller ids from `show allctrl` (a table with an "ID"-like column, or "Controller Id : n" lines).
pub fn controller_ids(text: &str) -> Vec<String> {
    let rows = table(text, |l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        w.iter().any(|x| x.eq_ignore_ascii_case("ctrlid") || x.eq_ignore_ascii_case("id") || x.eq_ignore_ascii_case("index")) && w.len() > 2
    });
    let mut ids: Vec<String> = rows
        .iter()
        .filter_map(|r| r.iter().find(|(k, _)| ["ctrlid", "id", "index"].contains(&k.to_lowercase().as_str())).map(|(_, v)| v.clone()))
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
        .collect();
    if ids.is_empty() {
        ids = text
            .lines()
            .filter_map(kv)
            .filter(|(k, _)| k.to_lowercase().contains("controller id") || k.to_lowercase() == "ctrl id")
            .map(|(_, v)| v)
            .collect();
    }
    ids
}

fn col(row: &[(String, String)], names: &[&str]) -> String {
    pick(row, names)
}

pub fn parse(spec: &ToolSpec, id: &str, status: &str, vdlist: &str, pdlist: &str) -> Controller {
    let sp: Vec<(String, String)> = status.lines().filter_map(kv).collect();
    let st = pick(&sp, &["Status", "Controller Status", "Health"]);
    let mut c = Controller {
        id: id.to_string(),
        addr: vec![("ctl".into(), id.to_string())],
        model: pick(&sp, &["Product Name", "Model", "Name"]),
        firmware: pick(&sp, &["Firmware Version", "FW Version"]),
        health: spec.health(&st),
        status: if st.is_empty() { "-".into() } else { st },
        props: sp,
        ..Default::default()
    };
    for row in table(vdlist, |l| l.split_whitespace().any(|w| w.eq_ignore_ascii_case("vdid"))) {
        let vid = col(&row, &["VDID"]);
        if vid.is_empty() {
            continue;
        }
        let state = col(&row, &["Status"]);
        c.vds.push(VirtualDrive {
            addr: vec![("ctl".into(), id.to_string()), ("vd".into(), vid.clone())],
            id: vid,
            name: col(&row, &["Name"]),
            level: col(&row, &["Type"]),
            size: col(&row, &["Capacity"]),
            health: spec.health(&state),
            state,
            cache: format!("R:{} W:{}", col(&row, &["RCache"]), col(&row, &["WCache"])),
            props: row,
            ..Default::default()
        });
    }
    for row in table(pdlist, |l| l.split_whitespace().any(|w| w.eq_ignore_ascii_case("did"))) {
        let did = col(&row, &["DID"]);
        if did.is_empty() {
            continue;
        }
        let state = col(&row, &["Status"]);
        let ty = col(&row, &["Type"]).to_lowercase();
        let (enc, slot) = (col(&row, &["Enc", "EID"]), col(&row, &["Slot", "Slt"]));
        let role = match ty.as_str() {
            t if t.contains("member") => Role::Member,
            t if t.contains("spare") || t.contains("hsp") => Role::HotSpare,
            t if t.contains("free") || t.contains("ug") => Role::Unconfigured,
            t if t.contains("rawdrive") || t.contains("raw") || t.contains("jbod") => Role::Jbod,
            _ => Role::Other,
        };
        c.pds.push(PhysicalDrive {
            addr: vec![("ctl".into(), id.to_string()), ("did".into(), did.clone()), ("enc".into(), enc.clone()), ("slot".into(), slot.clone())],
            id: if enc.is_empty() { did.clone() } else { format!("{enc}:{slot}") },
            model: col(&row, &["Model"]),
            firmware: col(&row, &["FW"]),
            size: col(&row, &["Capacity", "Size"]),
            media: col(&row, &["Media"]),
            intf: col(&row, &["Intf"]),
            health: spec.health(&state),
            state,
            role,
            token: did,
            props: row,
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
    fn documented_columns() {
        let vd = "VDID  Name    Type   Status    Capacity   SUSz  SecSz  RCache  WCache\n------------------------------------------------------------------\n0     sys     RAID1  normal    446.6GB    256   512    No      WT\n1     data    RAID5  degraded  7.2TB      256   512    No      WB\n";
        let pd = "Index  Enc  Slot  DID  Intf  Media  Type    Status   LinkSpd  SecType  Spin  FW    Model\n--------------------------------------------------------------\n0      0    0     0    SAS   HDD    member  online   12Gbps   512E     Up    N003  ST4000\n1      0    1     1    SATA  SSD    free    online   6Gbps    512N     Up    S1    KIOXIA\n";
        let c = parse(crate::spec("hiraidadm").unwrap(), "0", "Status : Normal\n", vd, pd);
        assert_eq!(c.health, Health::Ok);
        assert_eq!(c.vds.len(), 2);
        assert_eq!(c.vds[1].health, Health::Warning);
        assert_eq!(c.pds.len(), 2);
        assert_eq!((c.pds[0].id.as_str(), c.pds[0].role), ("0:0", Role::Member));
        assert_eq!(c.pds[1].role, Role::Unconfigured);
        assert_eq!(controller_ids("Controller Id : 0\nController Id : 1\n"), vec!["0", "1"]);
    }
}
