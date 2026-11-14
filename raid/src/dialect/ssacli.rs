//! HPE Smart Storage Administrator CLI: `ssacli ctrl all show config detail` (indented text).
//!
//! ```text
//! Smart Array P408i-a SR Gen10 in Slot 0 (Embedded)
//!    Slot: 0
//!    Controller Status: OK
//!    Array: A
//!       Logical Drive: 1
//!          Status: OK
//!       physicaldrive 1I:3:1
//!          Status: OK
//!    Unassigned
//!       physicaldrive 2I:3:7
//! ```

use anyhow::Result;

use super::query;
use crate::model::{Battery, Controller, PhysicalDrive, Role, VirtualDrive};
use crate::spec::{Found, ToolSpec};
use crate::text::{int, pick, tree, Node};
use crate::Runner;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    Ok(parse(f.spec, &query(r, f, "config", "")?))
}

fn slot_of(header: &str) -> Option<String> {
    let i = header.find(" in Slot ")?;
    Some(header[i + 9..].split_whitespace().next()?.to_string())
}

pub fn parse(spec: &ToolSpec, text: &str) -> Vec<Controller> {
    let mut out = Vec::new();
    for top in tree(text) {
        let Some(slot) = slot_of(&top.text) else { continue };
        let p = top.props();
        let status = pick(&p, &["Controller Status"]);
        let mut c = Controller {
            id: slot.clone(),
            addr: vec![("ctl".into(), slot.clone())],
            model: top.text.split(" in Slot ").next().unwrap_or("").to_string(),
            serial: pick(&p, &["Serial Number"]),
            firmware: pick(&p, &["Firmware Version"]),
            pci: pick(&p, &["PCI Address (Domain:Bus:Device.Function)", "PCI Address"]),
            driver: pick(&p, &["Driver Name"]),
            mode: pick(&p, &["Controller Mode"]),
            health: spec.health(&status),
            status,
            props: p.clone(),
            ..Default::default()
        };
        let bat = pick(&p, &["Battery/Capacitor Status"]);
        if !bat.is_empty() {
            c.battery = Some(Battery { kind: "Battery/Capacitor".into(), health: spec.health(&bat), state: bat, props: vec![] });
        }
        let mut path = Vec::new();
        top.walk(&mut path, &mut |n: &Node, parents: &[&str]| {
            let array = parents.iter().rev().find_map(|t| t.strip_prefix("Array: ")).unwrap_or("").to_string();
            if let Some(ld) = n.text.strip_prefix("Logical Drive: ") {
                let lp = n.props();
                let status = pick(&lp, &["Status"]);
                let ft = pick(&lp, &["Fault Tolerance"]);
                c.vds.push(VirtualDrive {
                    addr: vec![("ctl".into(), slot.clone()), ("vd".into(), ld.to_string()), ("array".into(), array.clone())],
                    id: ld.to_string(),
                    name: pick(&lp, &["Logical Drive Label"]),
                    level: if ft.is_empty() { String::new() } else { format!("RAID {ft}") },
                    size: pick(&lp, &["Size"]),
                    health: spec.health(&status),
                    progress: if status.to_lowercase().contains('%') { status.clone() } else { String::new() },
                    state: status,
                    cache: pick(&lp, &["Caching"]),
                    os_dev: pick(&lp, &["Disk Name"]),
                    props: lp,
                    ..Default::default()
                });
            } else if let Some(id) = n.text.strip_prefix("physicaldrive ") {
                let id = id.split_whitespace().next().unwrap_or("").to_string();
                let pp = n.props();
                let status = pick(&pp, &["Status"]);
                let ty = pick(&pp, &["Drive Type"]).to_lowercase();
                let role = if status.to_lowercase().contains("fail") {
                    Role::Failed
                } else if ty.contains("spare") {
                    Role::HotSpare
                } else if ty.contains("unassigned") {
                    Role::Unconfigured
                } else if ty.contains("hba") || ty.contains("unconfigured") {
                    Role::Jbod
                } else if !array.is_empty() {
                    Role::Member
                } else {
                    Role::Other
                };
                c.pds.push(PhysicalDrive {
                    addr: vec![("ctl".into(), slot.clone()), ("pd".into(), id.clone()), ("array".into(), array.clone())],
                    id: id.clone(),
                    model: pick(&pp, &["Model"]),
                    serial: pick(&pp, &["Serial Number"]),
                    firmware: pick(&pp, &["Firmware Revision"]),
                    size: pick(&pp, &["Size"]),
                    media: {
                        let i = pick(&pp, &["Interface Type"]).to_lowercase();
                        if i.contains("ssd") || i.contains("solid state") || !pick(&pp, &["SSD Smart Trip Wearout"]).is_empty() { "SSD".into() } else { "HDD".into() }
                    },
                    intf: pick(&pp, &["Interface Type"]),
                    health: spec.health(&status),
                    state: status,
                    role,
                    group: if array.is_empty() { String::new() } else { format!("Array {array}") },
                    temp_c: int(&pick(&pp, &["Current Temperature (C)"])).map(|t| t as i32),
                    os_dev: pick(&pp, &["Disk Name"]),
                    token: id,
                    props: pp,
                    ..Default::default()
                });
            }
        });
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Health;

    const SAMPLE: &str = "
Smart Array P408i-a SR Gen10 in Slot 0 (Embedded)
   Bus Interface: PCI
   Slot: 0
   Serial Number: PEYHN0ARCC3083
   Controller Status: OK
   Firmware Version: 2.65
   Battery/Capacitor Status: OK
   Controller Mode: Mixed

   Array: A
      Interface Type: SAS
      Status: OK

      Logical Drive: 1
         Size: 558.73 GB
         Fault Tolerance: 1
         Status: Interim Recovery Mode
         Caching:  Enabled
         Disk Name: /dev/sda

      physicaldrive 1I:1:1
         Port: 1I
         Box: 1
         Bay: 1
         Status: OK
         Drive Type: Data Drive
         Interface Type: SAS
         Size: 600 GB
         Firmware Revision: HPD3
         Serial Number: S7M0
         Model: HP      EG000600JWJNP
         Current Temperature (C): 31

      physicaldrive 1I:1:2
         Status: Failed
         Drive Type: Data Drive
         Size: 600 GB

      physicaldrive 1I:1:4
         Status: OK
         Drive Type: Spare Drive
         Size: 600 GB

   Unassigned

      physicaldrive 2I:3:7
         Status: OK
         Drive Type: Unassigned Drive
         Interface Type: Solid State SATA
         Size: 480 GB
";

    #[test]
    fn p408_degraded() {
        let c = parse(crate::spec("ssacli").unwrap(), SAMPLE);
        assert_eq!(c.len(), 1);
        let c = &c[0];
        assert_eq!((c.id.as_str(), c.model.as_str(), c.health), ("0", "Smart Array P408i-a SR Gen10", Health::Ok));
        assert_eq!(c.vds.len(), 1);
        assert_eq!((c.vds[0].level.as_str(), c.vds[0].health, c.vds[0].os_dev.as_str()), ("RAID 1", Health::Warning, "/dev/sda"));
        assert_eq!(c.pds.len(), 4);
        assert_eq!((c.pds[0].role, c.pds[0].temp_c, c.pds[0].group.as_str()), (Role::Member, Some(31), "Array A"));
        assert_eq!((c.pds[1].role, c.pds[1].health), (Role::Failed, Health::Critical));
        assert_eq!(c.pds[2].role, Role::HotSpare);
        assert_eq!((c.pds[3].role, c.pds[3].media.as_str()), (Role::Unconfigured, "SSD"));
        assert_eq!(c.overall(), Health::Critical);
    }
}
