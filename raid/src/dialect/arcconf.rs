//! Microchip / Adaptec arcconf: `arcconf LIST` and `arcconf GETCONFIG <n> AL` ("key : value" text).
//!
//! Field names changed case between versions ("Logical device number" / "Logical Device number",
//! "Status of logical device" / "Status of Logical Device"), so keys are matched case-insensitively.

use anyhow::Result;

use super::query;
use crate::model::{Battery, Controller, PhysicalDrive, Role, VirtualDrive};
use crate::spec::{Found, ToolSpec};
use crate::text::{int, kv, pick};
use crate::Runner;

type Props = Vec<(String, String)>;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    let list = query(r, f, "list", "")?;
    let ids = controller_ids(&list);
    let mut out = Vec::new();
    for id in ids {
        if let Ok(t) = query(r, f, "config", &id) {
            out.push(parse_config(f.spec, &id, &t));
        }
    }
    Ok(out)
}

/// "Controller 1:  : Adaptec ..." lines of `arcconf LIST`.
pub fn controller_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let t = l.trim();
            let rest = t.strip_prefix("Controller ")?;
            let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            (!id.is_empty() && rest[id.len()..].trim_start().starts_with(':')).then_some(id)
        })
        .collect()
}

#[derive(PartialEq)]
enum Sec {
    Ctl,
    Ld,
    Pd,
    Other,
}

pub fn parse_config(spec: &ToolSpec, id: &str, text: &str) -> Controller {
    let mut c = Controller { id: id.to_string(), addr: vec![("ctl".into(), id.to_string())], ..Default::default() };
    let mut sec = Sec::Other;
    let mut cur: Vec<(String, String)> = Vec::new();
    let mut members: Vec<String> = Vec::new();
    let mut ctl_props: Vec<(String, String)> = Vec::new();
    let mut bat: Vec<(String, String)> = Vec::new();
    let mut in_battery = false;
    let mut lds: Vec<(Props, Vec<String>)> = Vec::new();
    let mut pds: Vec<Props> = Vec::new();
    let flush = |sec: &Sec, cur: &mut Props, members: &mut Vec<String>, lds: &mut Vec<(Props, Vec<String>)>, pds: &mut Vec<Props>| {
        if cur.is_empty() {
            return;
        }
        match sec {
            Sec::Ld => lds.push((std::mem::take(cur), std::mem::take(members))),
            Sec::Pd => pds.push(std::mem::take(cur)),
            _ => cur.clear(),
        }
    };
    for line in text.lines() {
        let t = line.trim();
        let low = t.to_lowercase();
        if t.is_empty() || t.chars().all(|c| c == '-' || c == '=') {
            continue;
        }
        if low == "controller information" {
            sec = Sec::Ctl;
            continue;
        }
        if low.starts_with("logical device information") || low.starts_with("logical drive information") {
            flush(&sec, &mut cur, &mut members, &mut lds, &mut pds);
            sec = Sec::Ld;
            continue;
        }
        if low.starts_with("physical device information") {
            flush(&sec, &mut cur, &mut members, &mut lds, &mut pds);
            sec = Sec::Pd;
            continue;
        }
        if low.starts_with("array information") || low.starts_with("maxcache") || low.starts_with("connector information") || low.starts_with("enclosure information") {
            flush(&sec, &mut cur, &mut members, &mut lds, &mut pds);
            sec = Sec::Other;
            continue;
        }
        match sec {
            Sec::Ctl => {
                if low.contains("battery information") || low.contains("supercap information") || low.contains("backup unit") {
                    in_battery = true;
                } else if let Some(p) = kv(t) {
                    if in_battery { bat.push(p) } else { ctl_props.push(p) }
                }
            }
            Sec::Ld => {
                if low.starts_with("logical device number") || low.starts_with("logical drive number") {
                    flush(&sec, &mut cur, &mut members, &mut lds, &mut pds);
                    cur.push(("number".into(), t.split_whitespace().last().unwrap_or("").to_string()));
                } else if (low.starts_with("segment ") || low.starts_with("device ")) && low.contains(':') {
                    // "Segment 0 : Present (0,1) SN" / "Device 8 : Present (..., Enclosure:0, Slot:0) SN"
                    members.push(t.split_once(':').map(|x| x.1.trim().to_string()).unwrap_or_default());
                } else if let Some(p) = kv(t) {
                    cur.push(p);
                }
            }
            Sec::Pd => {
                if low.starts_with("device #") {
                    flush(&sec, &mut cur, &mut members, &mut lds, &mut pds);
                    cur.push(("number".into(), t.trim_start_matches(|c: char| !c.is_ascii_digit()).split_whitespace().next().unwrap_or("").to_string()));
                } else if low.starts_with("device is a") {
                    cur.push(("kind".into(), t.to_string()));
                } else if let Some(p) = kv(t) {
                    cur.push(p);
                }
            }
            Sec::Other => {}
        }
    }
    flush(&sec, &mut cur, &mut members, &mut lds, &mut pds);

    let status = pick(&ctl_props, &["Controller Status"]);
    c.model = pick(&ctl_props, &["Controller Model"]);
    c.serial = pick(&ctl_props, &["Controller Serial Number"]);
    c.firmware = pick(&ctl_props, &["Firmware"]);
    c.driver = pick(&ctl_props, &["Driver"]);
    c.mode = pick(&ctl_props, &["Controller Mode"]);
    c.pci = pick(&ctl_props, &["PCI Address"]);
    c.health = spec.health(&status);
    c.status = status;
    c.props = ctl_props;
    let bst = pick(&bat, &["Status"]);
    if !bst.is_empty() {
        c.battery = Some(Battery { kind: "Battery/Supercap".into(), health: spec.health(&bst), state: bst, props: bat });
    }
    for (p, m) in lds {
        let n = pick(&p, &["number"]);
        let state = pick(&p, &["Status of Logical Device", "Status of logical drive"]);
        c.vds.push(VirtualDrive {
            addr: vec![("ctl".into(), id.to_string()), ("vd".into(), n.clone())],
            id: n,
            name: pick(&p, &["Logical Device name", "Logical drive name"]),
            level: format!("RAID {}", pick(&p, &["RAID level"])),
            size: pick(&p, &["Size"]),
            health: spec.health(&state),
            progress: state.split_once('(').map(|x| x.1.trim_end_matches(')').to_string()).unwrap_or_default(),
            state: state.split('(').next().unwrap_or("").trim().to_string(),
            os_dev: pick(&p, &["Disk Name"]),
            members: m,
            props: p,
            ..Default::default()
        });
    }
    for p in pds {
        let kind = pick(&p, &["kind"]).to_lowercase();
        if !kind.is_empty() && !kind.contains("hard drive") && !kind.contains("ssd") {
            continue; // enclosure services devices etc.
        }
        let state = pick(&p, &["State"]);
        if state.is_empty() {
            continue;
        }
        // "0,1(1:0)" -> channel 0, device 1
        let chdev = pick(&p, &["Reported Channel,Device(T:L)", "Reported Channel,Device"]);
        let (ch, dev) = chdev.split('(').next().unwrap_or("").split_once(',').map(|(a, b)| (a.trim().to_string(), b.trim().to_string())).unwrap_or_default();
        let loc = pick(&p, &["Reported Location"]);
        let ls = state.to_lowercase();
        let role = if ls.contains("hot spare") {
            Role::HotSpare
        } else if ls.contains("raw") || ls.contains("pass") {
            Role::Jbod
        } else if ls.contains("ready") {
            Role::Unconfigured
        } else if ls.contains("fail") {
            Role::Failed
        } else if ls.contains("online") {
            Role::Member
        } else {
            Role::Other
        };
        c.pds.push(PhysicalDrive {
            addr: vec![("ctl".into(), id.to_string()), ("ch".into(), ch.clone()), ("dev".into(), dev.clone())],
            id: if loc.is_empty() { format!("{ch},{dev}") } else { loc.clone() },
            model: format!("{} {}", pick(&p, &["Vendor"]), pick(&p, &["Model"])).trim().to_string(),
            serial: pick(&p, &["Serial number"]),
            firmware: pick(&p, &["Firmware"]),
            size: pick(&p, &["Total Size", "Size"]),
            media: if pick(&p, &["SSD"]).eq_ignore_ascii_case("yes") { "SSD".into() } else { "HDD".into() },
            intf: pick(&p, &["Transfer Speed"]).split_whitespace().next().unwrap_or("").to_string(),
            health: spec.health(&state),
            state,
            role,
            temp_c: int(&pick(&p, &["Current Temperature"])).map(|t| t as i32),
            os_dev: pick(&p, &["Disk Name"]),
            token: format!("{ch} {dev}"),
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

    const LIST: &str = "Controllers found: 1\n----------------------------------------------------------------------\nController information\n----------------------------------------------------------------------\n   Controller ID             : Status, Slot, Mode, Name, SerialNumber, WWN\n----------------------------------------------------------------------\n   Controller 1:             : Optimal, Slot 1, RAID (Hide RAW), Adaptec SmartRAID 3154-8i, 7A4563D3E93, 50000D1E00123400\n";

    const CONFIG: &str = "Controllers found: 1
----------------------------------------------------------------------
Controller information
----------------------------------------------------------------------
   Controller Status                          : Optimal
   Controller Mode                            : RAID (Hide RAW)
   Controller Model                           : Adaptec SmartRAID 3154-8i
   Controller Serial Number                   : 7A4563D3E93
   Firmware                                   : 4.11[0]
   Driver                                     : Linux 2.1.16-030 (smartpqi)
   --------------------------------------------------------
   Controller Battery Information
   --------------------------------------------------------
   Status                                     : Optimal
----------------------------------------------------------------------
Logical device information
----------------------------------------------------------------------
Logical Device number 0
   Logical Device name                        : LogicalDrv 0
   Disk Name                                  : /dev/sda
   RAID level                                 : 1
   Status of Logical Device                   : Degraded(Rebuilding : 12 %)
   Size                                       : 457824 MB
   --------------------------------------------------------
   Array Physical Device Information
   --------------------------------------------------------
   Device 0                                   : Present (476940MB, SATA, SSD, Connector:CN0, Enclosure:0, Slot:0) S3F3
   Device 1                                   : Rebuilding (476940MB, SATA, SSD, Connector:CN0, Enclosure:0, Slot:1) S3F4
----------------------------------------------------------------------
Physical Device information
----------------------------------------------------------------------
      Device #0
         Device is a Hard drive
         State                                : Online
         Reported Channel,Device(T:L)         : 0,0(0:0)
         Reported Location                    : Enclosure 0, Slot 0
         Vendor                               : ATA
         Model                                : SAMSUNG MZ7LM480
         Serial number                        : S3F3
         Total Size                           : 476940 MB
         SSD                                  : Yes
         Current Temperature                  : 30 deg C
      Device #1
         Device is a Hard drive
         State                                : Ready
         Reported Channel,Device(T:L)         : 0,1(1:0)
         Reported Location                    : Enclosure 0, Slot 1
         Total Size                           : 476940 MB
      Device #2
         Device is an Enclosure Services Device
         Reported Channel,Device(T:L)         : 2,0(0:0)
";

    #[test]
    fn smartraid_3154() {
        assert_eq!(controller_ids(LIST), vec!["1"]);
        let c = parse_config(crate::spec("arcconf").unwrap(), "1", CONFIG);
        assert_eq!((c.model.as_str(), c.health, c.mode.as_str()), ("Adaptec SmartRAID 3154-8i", Health::Ok, "RAID (Hide RAW)"));
        assert_eq!(c.battery.as_ref().map(|b| b.health), Some(Health::Ok));
        assert_eq!(c.vds.len(), 1);
        let v = &c.vds[0];
        assert_eq!((v.state.as_str(), v.progress.as_str(), v.health, v.members.len()), ("Degraded", "Rebuilding : 12 %", Health::Warning, 2));
        assert_eq!(c.pds.len(), 2);
        assert_eq!((c.pds[0].role, c.pds[0].temp_c, c.pds[0].token.as_str()), (Role::Member, Some(30), "0 0"));
        assert_eq!(c.pds[1].role, Role::Unconfigured);
    }
}
