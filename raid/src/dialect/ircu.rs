//! Broadcom sas2ircu / sas3ircu (IT / IR firmware HBAs): `LIST` and `<n> DISPLAY`.

use anyhow::Result;

use super::query;
use crate::model::{Controller, PhysicalDrive, Role, VirtualDrive};
use crate::spec::{Found, ToolSpec};
use crate::text::{kv, pick};
use crate::Runner;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    let list = query(r, f, "list", "")?;
    let mut out = Vec::new();
    for id in controller_ids(&list) {
        if let Ok(t) = query(r, f, "display", &id) {
            out.push(parse_display(f.spec, &id, &t));
        }
    }
    Ok(out)
}

/// Rows of the `LIST` table start with the adapter index.
pub fn controller_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let first = w.next()?;
            let second = w.next()?;
            (first.chars().all(|c| c.is_ascii_digit()) && second.starts_with("SAS")).then(|| first.to_string())
        })
        .collect()
}

pub fn parse_display(spec: &ToolSpec, id: &str, text: &str) -> Controller {
    let mut c = Controller { id: id.to_string(), addr: vec![("ctl".into(), id.to_string())], status: "-".into(), ..Default::default() };
    // blocks: controller, "IR volume N", "Device is a Hard disk"
    let mut blocks: Vec<(String, Vec<(String, String)>)> = vec![("controller".into(), vec![])];
    let mut section = String::new();
    for line in text.lines() {
        let t = line.trim();
        let low = t.to_lowercase();
        if t.is_empty() || t.chars().all(|c| c == '-') {
            continue;
        }
        if low.starts_with("ir volume information") || low.starts_with("physical device information") || low.starts_with("enclosure information") || low.starts_with("controller information") {
            section = low.clone();
            continue;
        }
        if section.starts_with("enclosure") {
            continue;
        }
        if low.starts_with("ir volume ") || low.starts_with("device is a") || low.starts_with("initiator at") {
            blocks.push((t.to_string(), vec![]));
            continue;
        }
        if low.starts_with("phy[") && low.contains("enclosure#/slot#") {
            if let Some((_, v)) = kv(t) {
                blocks.last_mut().expect("block").1.push(("member".into(), v));
            }
            continue;
        }
        if let Some(p) = kv(t) {
            blocks.last_mut().expect("block").1.push(p);
        }
    }
    for (head, p) in blocks {
        let h = head.to_lowercase();
        if h == "controller" {
            c.model = pick(&p, &["Controller type"]);
            c.firmware = pick(&p, &["Firmware version"]);
            c.mode = if pick(&p, &["Firmware version"]).is_empty() { String::new() } else { "HBA".into() };
            c.health = spec.health("ok");
            c.props = p;
        } else if h.starts_with("ir volume") {
            let vid = pick(&p, &["Volume ID"]);
            let state = pick(&p, &["Status of volume"]);
            c.mode = "IR".into();
            c.vds.push(VirtualDrive {
                addr: vec![("ctl".into(), id.to_string()), ("vd".into(), vid.clone())],
                id: vid,
                name: pick(&p, &["Volume Name"]),
                level: pick(&p, &["RAID level"]),
                size: format!("{} MB", pick(&p, &["Size (in MB)"])),
                health: spec.health(&state),
                state,
                members: p.iter().filter(|(k, _)| k == "member").map(|(_, v)| v.clone()).collect(),
                props: p,
                ..Default::default()
            });
        } else if h.starts_with("device is a hard disk") {
            let (e, s) = (pick(&p, &["Enclosure #"]), pick(&p, &["Slot #"]));
            let state = pick(&p, &["State"]);
            let st = state.to_lowercase();
            let token = format!("{e}:{s}");
            let role = if st.contains("opt") || st.contains("dgd") || st.contains("osy") {
                Role::Member
            } else if st.contains("hsp") {
                Role::HotSpare
            } else if st.contains("fld") || st.contains("mis") {
                Role::Failed
            } else {
                Role::Jbod
            };
            c.pds.push(PhysicalDrive {
                addr: vec![("ctl".into(), id.to_string()), ("enc".into(), e), ("slot".into(), s)],
                id: token.clone(),
                model: format!("{} {}", pick(&p, &["Manufacturer"]), pick(&p, &["Model Number"])).trim().to_string(),
                serial: pick(&p, &["Serial No"]),
                firmware: pick(&p, &["Firmware Revision"]),
                size: pick(&p, &["Size (in MB)/(in sectors)"]).split('/').next().map(|m| format!("{m} MB")).unwrap_or_default(),
                media: if pick(&p, &["Drive Type"]).contains("SSD") { "SSD".into() } else { "HDD".into() },
                intf: pick(&p, &["Protocol"]),
                health: spec.health(&state),
                state,
                role,
                token,
                props: p,
                ..Default::default()
            });
        }
    }
    // members of IR volumes
    let groups: Vec<(String, Vec<String>)> = c.vds.iter().map(|v| (v.id.clone(), v.members.clone())).collect();
    for pd in &mut c.pds {
        if let Some((vid, _)) = groups.iter().find(|(_, m)| m.contains(&pd.token)) {
            pd.group = format!("Volume {vid}");
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Health;

    const LIST: &str = "         Adapter      Vendor  Device                       SubSys  SubSys\n Index    Type          ID      ID    Pci Address          Ven ID  Dev ID\n -----  ------------  ------  ------  -----------------    ------  ------\n   0     SAS3008     1000h    97h   00h:03h:00h:00h      1028h   1f53h\nSAS3IRCU: Utility Completed Successfully.\n";

    const DISPLAY: &str = "Read configuration has been initiated for controller 0
------------------------------------------------------------------------
Controller information
------------------------------------------------------------------------
  Controller type                         : SAS3008
  BIOS version                            : 8.37.00.00
  Firmware version                        : 16.00.10.00
------------------------------------------------------------------------
IR Volume information
------------------------------------------------------------------------
IR volume 1
  Volume ID                               : 323
  Status of volume                        : Degraded (DGD)
  RAID level                              : RAID1
  Size (in MB)                            : 857353
  Physical hard disks                     :
  PHY[0] Enclosure#/Slot#                 : 2:0
  PHY[1] Enclosure#/Slot#                 : 2:1
------------------------------------------------------------------------
Physical device information
------------------------------------------------------------------------
Initiator at ID #0

Device is a Hard disk
  Enclosure #                             : 2
  Slot #                                  : 0
  State                                   : Optimal (OPT)
  Size (in MB)/(in sectors)               : 858483/1758174767
  Manufacturer                            : SEAGATE
  Model Number                            : ST900MM0168
  Serial No                               : W4002
  Protocol                                : SAS
  Drive Type                              : SAS_HDD

Device is a Hard disk
  Enclosure #                             : 2
  Slot #                                  : 1
  State                                   : Failed (FLD)

Device is a Hard disk
  Enclosure #                             : 2
  Slot #                                  : 2
  State                                   : Ready (RDY)
  Drive Type                              : SATA_SSD
------------------------------------------------------------------------
Enclosure information
------------------------------------------------------------------------
  Enclosure#                              : 1
SAS3IRCU: Command DISPLAY Completed Successfully.
";

    #[test]
    fn sas3008_ir() {
        assert_eq!(controller_ids(LIST), vec!["0"]);
        let c = parse_display(crate::spec("sas3ircu").unwrap(), "0", DISPLAY);
        assert_eq!((c.model.as_str(), c.mode.as_str()), ("SAS3008", "IR"));
        assert_eq!(c.vds.len(), 1);
        assert_eq!((c.vds[0].id.as_str(), c.vds[0].health, c.vds[0].members.len()), ("323", Health::Warning, 2));
        assert_eq!(c.pds.len(), 3);
        assert_eq!((c.pds[0].role, c.pds[0].group.as_str(), c.pds[0].size.as_str()), (Role::Member, "Volume 323", "858483 MB"));
        assert_eq!((c.pds[1].role, c.pds[1].health), (Role::Failed, Health::Critical));
        assert_eq!((c.pds[2].role, c.pds[2].health, c.pds[2].media.as_str()), (Role::Jbod, Health::Ok, "SSD"));
    }
}
