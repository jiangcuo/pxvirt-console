//! Broadcom StorCLI / StorCLI2 / Dell PERCCLI: JSON output (`J`).
//!
//! Queries: `all` (controllers, VD/PD lists, CacheVault/BBU), `vds` (VD details, members,
//! OS device), `pds` (drive serials, temperatures, error counters), `rebuild`, `cc`, `init`,
//! `pr` (patrol read), `foreign`. Only `all` is required, the rest enrich the result.

use anyhow::Result;
use serde_json::Value;

use super::{json_of, props_of, query, s};
use crate::model::{Battery, Controller, Health, PhysicalDrive, Role, VirtualDrive};
use crate::spec::{Found, ToolSpec};
use crate::text::int;
use crate::Runner;

pub fn scan(r: &dyn Runner, f: &Found) -> Result<Vec<Controller>> {
    let all = json_of(&query(r, f, "all", "all")?)?;
    let mut ctls = parse_all(f.spec, &all);
    if ctls.is_empty() {
        return Ok(ctls);
    }
    let opt = |k: &str| query(r, f, k, "all").ok().and_then(|o| json_of(&o).ok());
    if let Some(v) = opt("vds") {
        apply_vds(&mut ctls, &v);
    }
    if let Some(v) = opt("pds") {
        apply_pds(f.spec, &mut ctls, &v);
    }
    // drives without an enclosure are only reachable as /cx/sx
    if ctls.iter().any(|c| c.pds.iter().any(|p| enc(p).is_empty())) {
        if let Some(v) = opt("pds_noenc") {
            apply_pds(f.spec, &mut ctls, &v);
        }
    }
    if let Some(v) = opt("rebuild") {
        apply_rebuild(&mut ctls, &v);
    }
    for (k, op) in [("cc", "Consistency check"), ("init", "Initialization")] {
        if let Some(v) = opt(k) {
            apply_vd_op(&mut ctls, &v, op);
        }
    }
    if let Some(v) = opt("pr") {
        for (c, rd) in responses(&v) {
            if let Some(ctl) = ctls.iter_mut().find(|x| x.id == c) {
                let st = prop_table(rd, "PR Current State");
                if !st.is_empty() && !st.eq_ignore_ascii_case("stopped") {
                    ctl.tasks.push(format!("Patrol read: {st}"));
                }
                let mode = prop_table(rd, "PR Mode");
                if !mode.is_empty() {
                    ctl.props.push(("Patrol read mode".into(), mode));
                }
            }
        }
    }
    if let Some(v) = opt("encl") {
        for (c, rd) in responses(&v) {
            let Some(ctl) = ctls.iter_mut().find(|x| x.id == c) else { continue };
            let Value::Object(m) = rd else { continue };
            for (k, e) in m {
                let Some(eid) = k.strip_prefix("Enclosure ").and_then(|p| p.rsplit_once("/e")).map(|x| x.1.trim().to_string()) else { continue };
                let serial = e.get("Information").map(|i| s(i, "Enclosure Serial Number")).unwrap_or_default();
                if let Some(en) = ctl.enclosures.iter_mut().find(|x| x.id == eid) {
                    en.serial = serial;
                } else {
                    ctl.enclosures.push(crate::model::Enclosure { id: eid, serial, slots: 0 });
                }
            }
        }
    }
    if let Some(v) = opt("foreign") {
        for (c, rd) in responses(&v) {
            if let Some(ctl) = ctls.iter_mut().find(|x| x.id == c) {
                ctl.foreign = rd.get("Total Foreign PDs").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            }
        }
    }
    Ok(ctls)
}

/// (controller id, Response Data) of every controller in a reply.
fn responses(v: &Value) -> Vec<(String, &Value)> {
    v.get("Controllers")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let id = c.get("Command Status").map(|cs| s(cs, "Controller")).unwrap_or_default();
                    Some((id, c.get("Response Data")?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Value of a `[{"Ctrl_Prop": .., "Value": ..}]` property table.
fn prop_table(rd: &Value, name: &str) -> String {
    rd.get("Controller Properties")
        .and_then(|a| a.as_array())
        .and_then(|a| a.iter().find(|p| s(p, "Ctrl_Prop") == name))
        .map(|p| s(p, "Value"))
        .unwrap_or_default()
}

/// Recursively find a scalar by key.
fn find(v: &Value, key: &str) -> String {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k.trim() == key && !x.is_object() && !x.is_array() {
                    return s(v, k);
                }
            }
            m.values().map(|x| find(x, key)).find(|x| !x.is_empty()).unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn enc(p: &PhysicalDrive) -> String {
    p.addr.iter().find(|(k, _)| k == "enc").map(|(_, v)| v.clone()).unwrap_or_default()
}

fn role_of(state: &str, dg: &str, ty: &str) -> Role {
    match state.trim().to_lowercase().as_str() {
        "jbod" => Role::Jbod,
        "ugood" | "ugunsp" | "ugshld" | "cfshld" => Role::Unconfigured,
        "ghs" | "dhs" => Role::HotSpare,
        "ubad" | "offln" | "failed" | "ubunsp" => Role::Failed,
        _ if ty.eq_ignore_ascii_case("jbod") => Role::Jbod,
        _ if dg.parse::<u32>().is_ok() => Role::Member,
        _ => Role::Other,
    }
}

pub fn parse_all(spec: &ToolSpec, v: &Value) -> Vec<Controller> {
    let mut out = Vec::new();
    for (id, rd) in responses(v) {
        let basics = rd.get("Basics").cloned().unwrap_or(Value::Null);
        let version = rd.get("Version").cloned().unwrap_or(Value::Null);
        let status_obj = rd.get("Status").cloned().unwrap_or(Value::Null);
        let id = if id.is_empty() { s(&basics, "Controller") } else { id };
        let status = s(&status_obj, "Controller Status");
        let mut c = Controller {
            id: id.clone(),
            addr: vec![("ctl".into(), id.clone())],
            model: s(&basics, "Model"),
            serial: s(&basics, "Serial Number"),
            pci: s(&basics, "PCI Address"),
            firmware: s(&version, "Firmware Version"),
            driver: format!("{} {}", s(&version, "Driver Name"), s(&version, "Driver Version")).trim().to_string(),
            mode: find(rd, "Current Personality"),
            health: spec.health(&status),
            status,
            ..Default::default()
        };
        for sec in ["Basics", "Version", "Bus", "Status"] {
            if let Some(x) = rd.get(sec) {
                c.props.extend(props_of(x));
            }
        }
        for key in ["Cachevault_Info", "BBU_Info"] {
            if let Some(b) = rd.get(key).and_then(|a| a.as_array()).and_then(|a| a.first()) {
                let state = s(b, "State");
                c.battery = Some(Battery {
                    kind: format!("{} {}", if key.starts_with("Cache") { "CacheVault" } else { "BBU" }, s(b, "Model")).trim().to_string(),
                    health: spec.health(&state),
                    state,
                    props: props_of(b),
                });
            }
        }
        for e in rd.get("Enclosure LIST").and_then(|a| a.as_array()).into_iter().flatten() {
            c.enclosures.push(crate::model::Enclosure { id: s(e, "EID"), serial: String::new(), slots: int(&s(e, "Slots")).unwrap_or(0) as u32 });
        }
        for vd in rd.get("VD LIST").and_then(|a| a.as_array()).into_iter().flatten() {
            let dgvd = s(vd, "DG/VD");
            let (dg, vdn) = dgvd.split_once('/').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or_default();
            let state = s(vd, "State");
            c.vds.push(VirtualDrive {
                addr: vec![("ctl".into(), id.clone()), ("vd".into(), vdn), ("dg".into(), dg)],
                id: dgvd,
                name: s(vd, "Name"),
                level: s(vd, "TYPE"),
                size: s(vd, "Size"),
                health: spec.health(&state),
                state,
                cache: s(vd, "Cache"),
                props: props_of(vd),
                ..Default::default()
            });
        }
        for pd in rd.get("PD LIST").and_then(|a| a.as_array()).into_iter().flatten() {
            let es = s(pd, "EID:Slt");
            let (e, slot) = es.split_once(':').map(|(a, b)| (a.trim().to_string(), b.trim().to_string())).unwrap_or_default();
            let path = if e.is_empty() { format!("/s{slot}") } else { format!("/e{e}/s{slot}") };
            let state = s(pd, "State");
            let dg = s(pd, "DG");
            let role = role_of(&state, &dg, &s(pd, "Type"));
            c.pds.push(PhysicalDrive {
                addr: vec![("ctl".into(), id.clone()), ("enc".into(), e.clone()), ("slot".into(), slot.clone()), ("pd".into(), path), ("dg".into(), dg.clone()), ("did".into(), s(pd, "DID"))],
                id: if e.is_empty() { format!(":{slot}") } else { es.clone() },
                model: s(pd, "Model"),
                size: s(pd, "Size"),
                media: s(pd, "Med"),
                intf: s(pd, "Intf"),
                health: spec.health(&state),
                state: if role == Role::Jbod && !state.eq_ignore_ascii_case("jbod") { format!("{state} (JBOD)") } else { state },
                role,
                group: if dg.parse::<u32>().is_ok() { format!("DG {dg}") } else { String::new() },
                token: es,
                props: props_of(pd),
                ..Default::default()
            });
        }
        out.push(c);
    }
    out
}

fn apply_vds(ctls: &mut [Controller], v: &Value) {
    for (cid, rd) in responses(v) {
        let Some(c) = ctls.iter_mut().find(|x| x.id == cid) else { continue };
        let Value::Object(m) = rd else { continue };
        for vd in &mut c.vds {
            let n = vd.addr.iter().find(|(k, _)| k == "vd").map(|(_, v)| v.clone()).unwrap_or_default();
            if let Some(members) = m.get(&format!("PDs for VD {n}")).and_then(|a| a.as_array()) {
                vd.members = members.iter().map(|p| s(p, "EID:Slt")).collect();
            }
            if let Some(p) = m.get(&format!("VD{n} Properties")) {
                vd.os_dev = s(p, "OS Drive Name");
                let ops = s(p, "Active Operations");
                if !ops.is_empty() && !ops.eq_ignore_ascii_case("none") {
                    vd.progress = ops;
                }
                vd.props.extend(props_of(p));
            }
        }
        // member drives: name the virtual drive they belong to
        let names: Vec<(String, Vec<String>)> = c.vds.iter().map(|v| (v.id.clone(), v.members.clone())).collect();
        for pd in &mut c.pds {
            if let Some((vd, _)) = names.iter().find(|(_, m)| m.contains(&pd.token)) {
                pd.group = format!("VD {vd}");
            }
        }
    }
}

fn apply_pds(spec: &ToolSpec, ctls: &mut [Controller], v: &Value) {
    for (cid, rd) in responses(v) {
        let Some(c) = ctls.iter_mut().find(|x| x.id == cid) else { continue };
        let Value::Object(m) = rd else { continue };
        for (k, det) in m {
            let Some(path) = k.strip_prefix("Drive ").and_then(|k| k.strip_suffix(" - Detailed Information")) else { continue };
            // /c0/e4/s1 or /c0/s1
            let tail = path.splitn(3, '/').nth(2).map(|t| format!("/{t}")).unwrap_or_default();
            let Some(pd) = c.pds.iter_mut().find(|p| p.addr.iter().any(|(a, b)| a == "pd" && *b == tail)) else { continue };
            let state = det.get(format!("Drive {path} State")).cloned().unwrap_or(Value::Null);
            let attr = det.get(format!("Drive {path} Device attributes")).cloned().unwrap_or(Value::Null);
            pd.serial = s(&attr, "SN");
            pd.firmware = s(&attr, "Firmware Revision");
            let model = s(&attr, "Model Number");
            if !model.is_empty() {
                pd.model = model;
            }
            pd.temp_c = int(&s(&state, "Drive Temperature")).filter(|t| *t > 0).map(|t| t as i32);
            let media = int(&s(&state, "Media Error Count")).unwrap_or(0);
            let other = int(&s(&state, "Other Error Count")).unwrap_or(0);
            let pred = int(&s(&state, "Predictive Failure Count")).unwrap_or(0);
            let smart = s(&state, "S.M.A.R.T alert flagged by drive");
            if pred > 0 || smart.eq_ignore_ascii_case("yes") {
                pd.health = pd.health.max(Health::Warning);
                pd.state = format!("{} (predictive failure)", pd.state);
            }
            if media > 0 || other > 0 {
                pd.props.push(("Errors".into(), format!("media {media}, other {other}")));
            }
            pd.props.extend(props_of(&state));
            pd.props.extend(props_of(&attr));
            if let Some(pol) = det.get(format!("Drive {path} Policies/Settings")) {
                pd.props.extend(props_of(pol));
            }
            let _ = spec;
        }
    }
}

fn apply_rebuild(ctls: &mut [Controller], v: &Value) {
    for (cid, rd) in responses(v) {
        let Some(c) = ctls.iter_mut().find(|x| x.id == cid) else { continue };
        for r in rd.as_array().into_iter().flatten() {
            let st = s(r, "Status");
            if st.is_empty() || st.eq_ignore_ascii_case("not in progress") {
                continue;
            }
            let id = s(r, "Drive-ID");
            let tail = id.splitn(3, '/').nth(2).map(|t| format!("/{t}")).unwrap_or_default();
            if let Some(pd) = c.pds.iter_mut().find(|p| p.addr.iter().any(|(a, b)| a == "pd" && *b == tail)) {
                pd.progress = format!("Rebuild {}% ({} left)", s(r, "Progress%"), s(r, "Estimated Time Left"));
            }
        }
    }
}

fn apply_vd_op(ctls: &mut [Controller], v: &Value, op: &str) {
    for (cid, rd) in responses(v) {
        let Some(c) = ctls.iter_mut().find(|x| x.id == cid) else { continue };
        for r in rd.get("VD Operation Status").and_then(|a| a.as_array()).into_iter().flatten() {
            let st = s(r, "Status");
            if st.is_empty() || st.eq_ignore_ascii_case("not in progress") {
                continue;
            }
            let n = s(r, "VD");
            if let Some(vd) = c.vds.iter_mut().find(|x| x.addr.iter().any(|(a, b)| a == "vd" && *b == n)) {
                vd.progress = format!("{op} {}% ({} left)", s(r, "Progress%"), s(r, "Estimated Time Left"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::testutil::{fixture, found, Fake};

    fn huawei() -> Fake {
        let d = "storcli/huawei-3108-jbod";
        Fake::new(&[
            ("/call show all J", &fixture(&format!("{d}/call_show_all.json"))),
            ("/call/vall show all J", &fixture(&format!("{d}/call_vall_show_all.json"))),
            ("/call/eall/sall show all J", &fixture(&format!("{d}/call_eall_sall_show_all.json"))),
            ("/call/eall/sall show rebuild J", &fixture(&format!("{d}/call_eall_sall_show_rebuild.json"))),
            ("/call/vall show cc J", &fixture(&format!("{d}/call_vall_show_cc.json"))),
            ("/call/fall show J", &fixture(&format!("{d}/call_fall_show.json"))),
            ("/call/eall show all J", &fixture(&format!("{d}/call_eall_show_all.json"))),
        ])
    }

    #[test]
    fn huawei_3108_jbod_mode() {
        let r = huawei();
        let c = crate::scan(&r, &found("storcli")).unwrap();
        assert_eq!(c.len(), 1);
        let c = &c[0];
        assert_eq!(c.id, "0");
        assert_eq!(c.status, "Optimal");
        assert_eq!(c.health, Health::Ok);
        assert_eq!(c.driver, "megaraid_sas 07.725.01.00-rc1");
        assert!(c.mode.starts_with("JBOD"), "{}", c.mode);
        assert_eq!(c.battery.as_ref().map(|b| (b.kind.as_str(), b.state.as_str())), Some(("CacheVault CVPM02", "Optimal")));
        assert_eq!(c.vds.len(), 1);
        let vd = &c.vds[0];
        assert_eq!((vd.id.as_str(), vd.level.as_str(), vd.state.as_str(), vd.os_dev.as_str()), ("0/239", "RAID1", "Optl", "/dev/sdi"));
        assert_eq!(vd.members, vec!["4:1", "4:2"]);
        assert_eq!(c.pds.len(), 10);
        let p1 = c.pds.iter().find(|p| p.id == "4:1").unwrap();
        assert_eq!((p1.role, p1.group.as_str()), (Role::Member, "VD 0/239"));
        assert!(!p1.serial.is_empty());
        let p3 = c.pds.iter().find(|p| p.id == "4:3").unwrap();
        assert_eq!(p3.role, Role::Jbod);
        assert_eq!(p3.temp_c, Some(37));
        assert_eq!(p3.addr.iter().find(|(k, _)| k == "pd").map(|x| x.1.as_str()), Some("/e4/s3"));
        assert_eq!(c.foreign, 0);
        let e4 = c.enclosures.iter().find(|e| e.id == "4").unwrap();
        assert_eq!((e4.serial.as_str(), e4.slots), ("500e004aaaaaaa1e", 16));
        // locate LEDs from the SES enclosure of the expander
        let det = crate::detect(&fixture("storcli/huawei-3108-jbod/ses.txt"));
        assert_eq!(det.ses.len(), 1);
        assert_eq!(det.ses[0].slots.len(), 16);
        let mut ctls = vec![c.clone()];
        let mut ses = det.ses.clone();
        ses[0].slots[3].locate = true;
        crate::attach_ses(&mut ctls, &ses);
        let p = |id: &str| ctls[0].pds.iter().find(|p| p.id == id).unwrap().clone();
        assert_eq!(p("4:3").locate, Some(true));
        assert_eq!(p("4:4").locate, Some(false));
        assert!(p("4:11").ses.ends_with("Array Device 11 "));
        assert!(c.problems().is_empty(), "{:?}", c.problems());
        // optional queries that are not answered (init, pr) are skipped
        assert!(r.calls.borrow().iter().any(|a| a == "/call show patrolread J"));
    }

    #[test]
    fn degraded_and_rebuild_are_reported() {
        let mut v: Value = serde_json::from_str(&fixture("storcli/huawei-3108-jbod/call_show_all.json")).unwrap();
        let rd = &mut v["Controllers"][0]["Response Data"];
        rd["VD LIST"][0]["State"] = "Dgrd".into();
        rd["PD LIST"][1]["State"] = "Rbld".into();
        rd["PD LIST"][2]["State"] = "UBad".into();
        let c = &parse_all(crate::spec("storcli").unwrap(), &v)[0];
        assert_eq!(c.vds[0].health, Health::Warning);
        assert_eq!(c.pds[1].health, Health::Warning);
        assert_eq!((c.pds[2].health, c.pds[2].role), (Health::Critical, Role::Failed));
        assert_eq!(c.overall(), Health::Critical);
        assert_eq!(c.problems().len(), 3);
    }
}
