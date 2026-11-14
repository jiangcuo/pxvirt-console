//! 3ware / LSI tw_cli (legacy 9000 series): `show` lists controllers, `/c<n> show` prints a
//! unit table and a port table.

use anyhow::Result;

use super::query;
use crate::model::{Controller, PhysicalDrive, Role, VirtualDrive};
use crate::spec::{Found, ToolSpec};
use crate::text::table;
use crate::Runner;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    let list = query(r, f, "list", "")?;
    let mut out = Vec::new();
    for row in table(&list, |l| l.trim_start().starts_with("Ctl ")) {
        let ctl = row.first().map(|x| x.1.trim_start_matches('c').to_string()).unwrap_or_default();
        if ctl.is_empty() {
            continue;
        }
        let model = row.get(1).map(|x| x.1.clone()).unwrap_or_default();
        if let Ok(t) = query(r, f, "show", &ctl) {
            out.push(parse(f.spec, &ctl, &model, &t));
        }
    }
    Ok(out)
}

pub fn parse(spec: &ToolSpec, ctl: &str, model: &str, text: &str) -> Controller {
    let mut c = Controller { id: ctl.to_string(), addr: vec![("ctl".into(), ctl.to_string())], model: model.to_string(), status: "-".into(), health: spec.health("ok"), ..Default::default() };
    // Unit  UnitType  Status  %RCmpl  %V/I/M  Stripe  Size(GB)  Cache  AVrfy
    for w in text.lines().map(|l| l.split_whitespace().collect::<Vec<_>>()).filter(|w| w.first().map(|x| x.starts_with('u') && x[1..].parse::<u32>().is_ok()).unwrap_or(false)) {
        if w.len() < 7 {
            continue;
        }
        let state = w[2].to_string();
        c.vds.push(VirtualDrive {
            addr: vec![("ctl".into(), ctl.to_string()), ("vd".into(), w[0][1..].to_string())],
            id: w[0].to_string(),
            level: w[1].to_string(),
            size: format!("{} GB", w[6]),
            health: spec.health(&state),
            progress: if w[3] != "-" { format!("rebuild {}", w[3]) } else if w[4] != "-" { format!("verify/init {}", w[4]) } else { String::new() },
            state,
            cache: w.get(7).map(|x| x.to_string()).unwrap_or_default(),
            ..Default::default()
        });
    }
    // VPort Status Unit Size Type Phy Encl-Slot Model   (older: Port Status Unit Size Blocks Serial)
    for w in text.lines().map(|l| l.split_whitespace().collect::<Vec<_>>()).filter(|w| w.first().map(|x| x.starts_with('p') && x[1..].parse::<u32>().is_ok()).unwrap_or(false)) {
        if w.len() < 4 {
            continue;
        }
        let state = w[1].to_string();
        let unit = w[2].to_string();
        let role = match state.to_uppercase().as_str() {
            "NOT-PRESENT" => continue,
            s if s.contains("ECC") || s.contains("DEGRADED") || s.contains("FAIL") => Role::Failed,
            _ if unit.starts_with('u') => Role::Member,
            _ => Role::Unconfigured,
        };
        c.pds.push(PhysicalDrive {
            addr: vec![("ctl".into(), ctl.to_string()), ("pd".into(), w[0][1..].to_string())],
            id: w[0].to_string(),
            size: format!("{} {}", w[3], w.get(4).filter(|x| x.len() <= 2).map(|x| x.to_string()).unwrap_or_default()).trim().to_string(),
            model: w.get(7..).map(|x| x.join(" ")).unwrap_or_default(),
            health: spec.health(&state),
            state,
            role,
            group: if unit.starts_with('u') { unit } else { String::new() },
            token: w[0][1..].to_string(),
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
    fn c9650() {
        let t = "Unit  UnitType  Status         %RCmpl  %V/I/M  Stripe  Size(GB)  Cache  AVrfy\n------------------------------------------------------------------------------\nu0    RAID-1    DEGRADED       -       -       -       931.312   RiW    ON\n\nVPort Status         Unit Size      Type  Phy Encl-Slot    Model\n------------------------------------------------------------------------------\np0    OK             u0   931.51 GB SATA  0   -            WDC WD1003FBYX-01Y7\np1    NOT-PRESENT    -    -         -     -   -            -\np2    OK             -    931.51 GB SATA  2   -            WDC WD1003FBYX-01Y7\n";
        let c = parse(crate::spec("tw_cli").unwrap(), "0", "9650SE-4LPML", t);
        assert_eq!((c.vds.len(), c.vds[0].health), (1, Health::Warning));
        assert_eq!(c.pds.len(), 2);
        assert_eq!((c.pds[0].role, c.pds[1].role), (Role::Member, Role::Unconfigured));
    }
}
