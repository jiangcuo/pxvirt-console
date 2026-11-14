//! Hardware RAID / HBA / NVMe management through the vendor command line tools.
//!
//! - [`model`]: tool independent data (controllers, virtual and physical drives, health)
//! - [`spec`]: what differs between tools, written down as data ([`ToolSpec`])
//! - [`tools::TOOLS`]: the known tools
//! - one parser per output format (`dialect`)
//!
//! Commands run through a [`Runner`], so the caller decides where (local, ssh...).

pub mod model;
pub mod spec;
pub mod text;
pub mod tools;

mod dialect;

use anyhow::{bail, Result};

pub use model::{Battery, Controller, Health, PhysicalDrive, Problem, Role, VirtualDrive};
pub use spec::{ActionSpec, Detection, Dialect, Found, Param, ParamKind, PciDev, Risk, SesEnclosure, SesSlot, Target, ToolSpec};

/// Runs a tool. Implementations return stdout even when the exit code is not 0
/// (vendor tools report errors inside their output).
pub trait Runner {
    fn run(&self, prog: &str, args: &[String]) -> Result<String>;
}

/// Split a query / template string into arguments.
pub(crate) fn args(s: &str, ctl: &str) -> Vec<String> {
    spec::render(s, &[("ctl".to_string(), ctl.to_string())]).unwrap_or_else(|_| s.split_whitespace().map(String::from).collect())
}

/// Shell script for [`detect`]: run it with `sh -c` on the host.
pub fn detect_script() -> String {
    spec::detect_script(tools::TOOLS)
}

/// Controllers and tools from the output of [`detect_script`].
pub fn detect(out: &str) -> Detection {
    spec::parse_detect(tools::TOOLS, out)
}

/// Read everything a tool reports.
pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    let mut v = match f.spec.dialect {
        Dialect::Storcli => dialect::storcli::scan(r, f),
        Dialect::Nvme => dialect::nvme::scan(r, f),
        Dialect::Ssacli => dialect::ssacli::scan(r, f),
        Dialect::Arcconf => dialect::arcconf::scan(r, f),
        Dialect::Ircu => dialect::ircu::scan(r, f),
        Dialect::Hiraidadm => dialect::hiraidadm::scan(r, f),
        Dialect::Areca => dialect::areca::scan(r, f),
        Dialect::Hpt => dialect::hpt::scan(r, f),
        Dialect::Mvcli => dialect::mvcli::scan(r, f),
        Dialect::Twcli => dialect::twcli::scan(r, f),
    }?;
    for c in &mut v {
        c.tool = f.spec.id;
        if !c.addr.iter().any(|(k, _)| k == "ctl") {
            c.addr.insert(0, ("ctl".into(), c.id.clone()));
        }
    }
    Ok(v)
}

/// Command lines of an action: placeholders from the object's address plus the form values.
pub fn commands(a: &ActionSpec, addr: &[(String, String)], params: &[(String, String)]) -> Result<Vec<Vec<String>>> {
    let mut vars: Vec<(String, String)> = addr.to_vec();
    vars.extend(params.iter().cloned());
    for p in a.params {
        if p.required && !params.iter().any(|(k, v)| k == p.key && !v.trim().is_empty()) {
            bail!("{} is required", p.label);
        }
    }
    a.cmds.iter().map(|c| spec::render(c, &vars)).collect()
}

/// Attach the LED state of SES enclosure slots to the drives: by OS block device, else by
/// enclosure (tool serial = SES id, or the only enclosure with that many slots) and slot number.
pub fn attach_ses(ctls: &mut [Controller], ses: &[SesEnclosure]) {
    let norm = |s: &str| s.trim().trim_start_matches("0x").trim_start_matches("0X").to_lowercase();
    for c in ctls.iter_mut() {
        let encs = c.enclosures.clone();
        for pd in &mut c.pds {
            let get = |k: &str| pd.addr.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let dev = pd.os_dev.trim_start_matches("/dev/").to_string();
            let mut hit = if dev.is_empty() { None } else { ses.iter().flat_map(|e| &e.slots).find(|s| s.dev == dev) };
            if hit.is_none() {
                let (enc, slot) = (get("enc"), get("slot").parse::<u32>().ok());
                if let (Some(e), Some(slot)) = (encs.iter().find(|e| e.id == enc), slot) {
                    let se = ses.iter().find(|s| !e.serial.is_empty() && norm(&s.id) == norm(&e.serial)).or_else(|| {
                        let unique = encs.iter().filter(|x| x.slots == e.slots).count() == 1;
                        (ses.len() == 1 && unique && ses[0].slots.len() as u32 == e.slots).then(|| &ses[0])
                    });
                    hit = se.and_then(|s| s.slots.iter().find(|x| x.slot == slot));
                }
            }
            if let Some(h) = hit {
                pd.locate = Some(h.locate);
                pd.fault_led = Some(h.fault);
                pd.ses = h.path.clone();
            }
        }
    }
}

/// Tool by spec id.
pub fn spec(id: &str) -> Option<&'static ToolSpec> {
    tools::TOOLS.iter().find(|t| t.id == id)
}

#[cfg(test)]
mod live {
    use super::*;

    struct Ssh(String);

    impl Runner for Ssh {
        fn run(&self, prog: &str, args: &[String]) -> Result<String> {
            let mut cmd = vec![prog.to_string()];
            cmd.extend(args.iter().map(|a| format!("'{}'", a.replace('\'', "'\\''"))));
            let out = std::process::Command::new("ssh").args(["-o", "BatchMode=yes", &self.0, &cmd.join(" ")]).output()?;
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        }
    }

    /// Read-only: `RAID_SSH=root@host cargo test -p pxvirt-raid live_host -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_host() {
        let Ok(host) = std::env::var("RAID_SSH") else { return };
        let r = Ssh(host);
        let det = detect(&r.run("sh", &["-c".into(), detect_script()]).unwrap());
        for d in &det.pci {
            println!("pci {} {} {}:{} sub {} {}", d.addr, d.class, d.vendor, d.device, d.subvendor, d.driver);
        }
        for (d, s) in &det.missing {
            println!("missing tool for {} ({}): {}", d.addr, d.driver, s.install);
        }
        assert!(!det.tools.is_empty());
        for f in &det.tools {
            let ctls = scan(&r, f).unwrap_or_else(|e| panic!("{}: {e:#}", f.spec.id));
            for c in &ctls {
                println!("[{}] {} ctl {} {} fw {} mode {} status {} ({:?}) overall {:?}", f.spec.id, f.path, c.id, c.model, c.firmware, c.mode, c.status, c.health, c.overall());
                if let Some(b) = &c.battery {
                    println!("    battery {} {} {:?}", b.kind, b.state, b.health);
                }
                for t in &c.tasks {
                    println!("    task {t}");
                }
                for v in &c.vds {
                    println!("    vd {} {} {} {} {:?} {} members {:?} {}", v.id, v.level, v.size, v.state, v.health, v.os_dev, v.members, v.progress);
                }
                for p in &c.pds {
                    println!("    pd {:<6} {:<12} {:<26} {:>10} {:<5} {:<14} {:?} temp {:?} {} {}", p.id, p.role.label(), p.model, p.size, p.media, p.state, p.health, p.temp_c, p.serial, p.group);
                }
                for p in c.problems() {
                    println!("    PROBLEM {:?} {}", p.health, p.text);
                }
                // read-only actions of the first controller, virtual and physical drive
                let objs = [Some(c.addr.clone()), c.vds.first().map(|v| v.addr.clone()), c.pds.first().map(|p| p.addr.clone())];
                for (t, addr) in [Target::Controller, Target::Vd, Target::Pd].into_iter().zip(objs) {
                    let Some(addr) = addr else { continue };
                    for a in f.spec.actions_for(t).filter(|a| a.risk == Risk::View) {
                        for cmd in commands(a, &addr, &[]).unwrap() {
                            let out = r.run(&f.path, &cmd).unwrap();
                            let bad = out.contains("Status = Failure") || out.contains("\"Status\" : \"Failure\"") || out.trim().is_empty() || out.contains("Invalid");
                            println!("    {} {:<28} {} -> {} bytes{}", if bad { "FAIL" } else { "ok  " }, a.label, cmd.join(" "), out.len(), if bad { format!(": {}", out.lines().take(12).collect::<Vec<_>>().join(" | ")) } else { String::new() });
                        }
                    }
                }
            }
        }
    }
}
