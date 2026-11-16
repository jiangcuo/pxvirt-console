//! BMC (IPMI) data from ipmitool: controller info, LAN settings, chassis status, sensors and the
//! system event log. Shared by System > BMC and the health checks.

use std::collections::HashMap;

use chrono::NaiveDateTime;

use crate::health::Sev;

/// Everything shown on the BMC page, collected in one host call.
pub const SCRIPT: &str = r#"export LC_ALL=C
command -v ipmitool >/dev/null 2>&1 || echo '@@noipmitool'
ls /dev/ipmi* >/dev/null 2>&1 || echo '@@nodevice'
command -v ipmitool >/dev/null 2>&1 && ls /dev/ipmi* >/dev/null 2>&1 || exit 0
echo '@@mc'; timeout 20 ipmitool mc info 2>&1
for ch in 1 2 3 8; do o=$(timeout 10 ipmitool lan print $ch 2>/dev/null) && [ -n "$o" ] && { echo "@@lan $ch"; echo "$o"; }; done
echo '@@chassis'; timeout 20 ipmitool chassis status 2>&1
echo '@@selinfo'; timeout 20 ipmitool sel info 2>&1
echo '@@fru'; timeout 30 ipmitool fru print 0 2>/dev/null
echo '@@sdr'; timeout 60 ipmitool sdr elist 2>/dev/null
echo '@@sel'; timeout 120 ipmitool sel elist 2>/dev/null
true
"#;

/// "Key   : value" lines (mc info, lan print, chassis status, sel info, fru print).
pub fn kv(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(" : ").or_else(|| l.split_once(": "))?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

pub fn get<'a>(kv: &'a [(String, String)], key: &str) -> &'a str {
    kv.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or("")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sensor {
    pub name: String,
    /// ok, ns (no reading), nc / lnc / unc (non-critical), cr / lcr / ucr, nr (non-recoverable)
    pub status: String,
    pub entity: String,
    pub reading: String,
}

impl Sensor {
    pub fn sev(&self) -> Sev {
        match self.status.as_str() {
            "ok" | "ns" | "" => Sev::Ok,
            "nc" | "lnc" | "unc" => Sev::Warn,
            _ => Sev::Crit,
        }
    }
}

/// `ipmitool sdr elist`: "CPU1 Temp        | 30h | ok  |  3.1 | 45 degrees C"
pub fn parse_sdr(text: &str) -> Vec<Sensor> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('|').map(|x| x.trim()).collect();
            (f.len() >= 5).then(|| Sensor { name: f[0].into(), status: f[2].into(), entity: f[3].into(), reading: f[4].into() })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelEntry {
    /// record id (hex in `sel elist`)
    pub id: u64,
    pub date: String,
    pub time: String,
    pub at: Option<NaiveDateTime>,
    pub sensor: String,
    pub event: String,
    /// "Asserted" / "Deasserted" and extra data
    pub state: String,
}

impl SelEntry {
    pub fn when(&self) -> String {
        match self.at {
            Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
            None => format!("{} {}", self.date, self.time).trim().to_string(),
        }
    }

    /// How bad the event is. Deasserted events (a condition going away) are informational.
    pub fn sev(&self) -> Sev {
        let t = format!("{} {} {}", self.sensor, self.event, self.state).to_lowercase();
        let sev = if ["uncorrectable", "non-recoverable", "critical", "failure", "failed", "fault", "ac lost", "power off", "ierr", "machine check", "thermal trip", "bus fatal", "watchdog"].iter().any(|w| t.contains(w)) {
            Sev::Crit
        } else if ["correctable", "predictive", "degraded", "redundancy lost", "non-critical", "going high", "going low", "presence detected", "limit reached", "throttl"].iter().any(|w| t.contains(w)) {
            Sev::Warn
        } else {
            Sev::Info
        };
        if self.state.to_lowercase().starts_with("deasserted") {
            Sev::Info
        } else {
            sev
        }
    }
}

/// `ipmitool sel elist`: "   1 | 11/14/2026 | 21:00:00 | Memory #0x01 | Correctable ECC | Asserted"
/// (pre-init entries have no date; newer ipmitool appends a time zone to the time).
pub fn parse_sel(text: &str) -> Vec<SelEntry> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('|').map(|x| x.trim()).collect();
            if f.len() < 5 {
                return None;
            }
            let id = u64::from_str_radix(f[0], 16).ok()?;
            let time = f[2].split_whitespace().next().unwrap_or("");
            let at = NaiveDateTime::parse_from_str(&format!("{} {time}", f[1]), "%m/%d/%Y %H:%M:%S").ok();
            Some(SelEntry { id, date: f[1].into(), time: f[2].into(), at, sensor: f[3].into(), event: f[4].into(), state: f[5..].join(" | ") })
        })
        .collect()
}

/// Split "@@name args" sections.
pub fn sections(text: &str) -> HashMap<String, String> {
    let mut m: HashMap<String, String> = HashMap::new();
    let mut cur = String::new();
    for l in text.lines() {
        if let Some(h) = l.strip_prefix("@@") {
            cur = h.trim().to_string();
            m.entry(cur.clone()).or_default();
            continue;
        }
        let e = m.entry(cur.clone()).or_default();
        e.push_str(l);
        e.push('\n');
    }
    m
}

#[derive(Clone, Debug, Default)]
pub struct Bmc {
    pub no_tool: bool,
    pub no_device: bool,
    pub mc: Vec<(String, String)>,
    /// (channel, settings)
    pub lan: Vec<(u8, Vec<(String, String)>)>,
    pub chassis: Vec<(String, String)>,
    pub sel_info: Vec<(String, String)>,
    pub fru: Vec<(String, String)>,
    pub sensors: Vec<Sensor>,
    /// newest first
    pub sel: Vec<SelEntry>,
}

impl Bmc {
    pub fn parse(out: &str) -> Self {
        let s = sections(out);
        let sec = |n: &str| s.get(n).map(|x| x.as_str()).unwrap_or("");
        let mut lan: Vec<(u8, Vec<(String, String)>)> = s.iter().filter_map(|(k, v)| Some((k.strip_prefix("lan ")?.parse().ok()?, kv(v)))).collect();
        lan.sort_by_key(|x| x.0);
        // channels without an address are not interesting
        lan.retain(|(_, l)| !get(l, "IP Address").is_empty());
        let mut sel = parse_sel(sec("sel"));
        sel.sort_by_key(|e| std::cmp::Reverse(e.id));
        Bmc {
            no_tool: s.contains_key("noipmitool"),
            no_device: s.contains_key("nodevice"),
            mc: kv(sec("mc")),
            lan,
            chassis: kv(sec("chassis")),
            sel_info: kv(sec("selinfo")),
            fru: kv(sec("fru")),
            sensors: parse_sdr(sec("sdr")),
            sel,
        }
    }

    /// Percent of the event log used, if the BMC reports it.
    pub fn sel_used(&self) -> Option<f64> {
        sel_used(&self.sel_info)
    }
}

pub fn sel_used(info: &[(String, String)]) -> Option<f64> {
    get(info, "Percent Used").trim_end_matches('%').trim().parse().ok()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const OUT: &str = "@@mc
Device ID                 : 32
Firmware Revision         : 3.25
IPMI Version              : 2.0
Manufacturer Name         : Huawei Technologies Co., Ltd.
Additional Device Support :
    Sensor Device
@@lan 1
Set in Progress         : Set Complete
IP Address Source       : Static Address
IP Address              : 192.168.18.111
Subnet Mask             : 255.255.255.0
MAC Address             : 88:bc:c1:00:11:22
Default Gateway IP      : 192.168.18.1
802.1q VLAN ID          : Disabled
@@lan 2
IP Address              :
@@chassis
System Power         : on
Power Restore Policy : always-off
Chassis Intrusion    : inactive
Cooling/Fan Fault    : false
@@selinfo
SEL Information
Version          : 1.5 (v1.5, v2 compliant)
Entries          : 3
Percent Used     : 93%
Overflow         : false
@@sdr
Inlet Temp       | 01h | ok  | 64.1 | 23 degrees C
FAN1 Speed       | 30h | cr  | 29.1 | 0 RPM
PS2 Status       | 61h | ns  | 10.2 | No Reading
@@sel
   1 | Pre-Init  |0000000005| System Event #0x01 | Timestamp Clock Sync | Asserted
   a | 11/14/2026 | 21:00:00 CET | Memory #0x01 | Correctable ECC | Asserted
   b | 11/14/2026 | 21:05:00 | Power Supply #0x02 | Power Supply AC lost | Asserted
   c | 11/14/2026 | 21:06:00 | Power Supply #0x02 | Power Supply AC lost | Deasserted
";

    #[test]
    fn parses_ipmitool_output() {
        let b = Bmc::parse(OUT);
        assert!(!b.no_tool && !b.no_device);
        assert_eq!(get(&b.mc, "Firmware Revision"), "3.25");
        assert_eq!(b.lan.len(), 1, "channels without address are dropped");
        assert_eq!(get(&b.lan[0].1, "IP Address"), "192.168.18.111");
        assert_eq!(get(&b.chassis, "Power Restore Policy"), "always-off");
        assert_eq!(b.sel_used(), Some(93.0));
        assert_eq!(b.sensors.iter().map(|s| s.sev()).collect::<Vec<_>>(), vec![Sev::Ok, Sev::Crit, Sev::Ok]);
        let ids: Vec<u64> = b.sel.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![12, 11, 10, 1], "newest first, hex ids");
        assert_eq!(b.sel[2].when(), "2026-11-14 21:00:00", "time zone suffix ignored");
        assert_eq!(b.sel[3].at, None);
        assert_eq!(b.sel.iter().map(|e| e.sev()).collect::<Vec<_>>(), vec![Sev::Info, Sev::Crit, Sev::Warn, Sev::Info]);
    }

    #[test]
    fn missing_tool() {
        let b = Bmc::parse("@@noipmitool\n");
        assert!(b.no_tool && !b.no_device && b.sel.is_empty());
    }
}
