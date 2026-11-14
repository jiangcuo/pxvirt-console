//! Tool specifications: everything that differs between vendor tools and can be written down
//! as data (binary names, install paths, matching kernel drivers, query commands, state words,
//! actions with their command templates). A new tool whose output looks like one of the
//! existing dialects only needs a new [`ToolSpec`] in `tools.rs`.

use anyhow::{bail, Result};

use crate::model::{health_of, Health, Role};

/// Output format family; each has a parser in `dialect/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// Broadcom StorCLI / StorCLI2 / Dell PERCCLI (JSON with `J`)
    Storcli,
    /// nvme-cli (JSON with `-o json`)
    Nvme,
    /// HPE Smart Storage Administrator CLI
    Ssacli,
    /// Microchip / Adaptec arcconf
    Arcconf,
    /// Broadcom sas2ircu / sas3ircu
    Ircu,
    /// Huawei hiraidadm
    Hiraidadm,
    /// Areca cli64
    Areca,
    /// HighPoint hptraidconf
    Hpt,
    /// Marvell mvcli / mnv_cli (Dell BOSS)
    Mvcli,
    /// 3ware tw_cli
    Twcli,
}

/// What an action works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Controller,
    Vd,
    Pd,
}

/// How careful the UI must be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// read only: run and show the output
    View,
    /// harmless (LED, start a check): confirm
    Low,
    /// changes the configuration: confirm and show the command
    Medium,
    /// can destroy data: type the object id to confirm
    High,
}

#[derive(Clone, Copy, Debug)]
pub enum ParamKind {
    /// fixed choices (value, label)
    Choice(&'static [(&'static str, &'static str)]),
    Text(&'static str),
    Number(&'static str),
    /// several unconfigured drives; the value is their tokens joined with the spec's `drive_sep`
    Drives,
}

#[derive(Clone, Copy, Debug)]
pub struct Param {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: ParamKind,
    pub required: bool,
}

pub const fn choice(key: &'static str, label: &'static str, opts: &'static [(&'static str, &'static str)]) -> Param {
    Param { key, label, kind: ParamKind::Choice(opts), required: true }
}

pub const fn text(key: &'static str, label: &'static str, default: &'static str, required: bool) -> Param {
    Param { key, label, kind: ParamKind::Text(default), required }
}

pub const fn drives(key: &'static str, label: &'static str) -> Param {
    Param { key, label, kind: ParamKind::Drives, required: true }
}

#[derive(Clone, Copy, Debug)]
pub struct ActionSpec {
    /// hotkey in the UI
    pub key: char,
    pub label: &'static str,
    pub target: Target,
    /// command templates (arguments of the tool), run one after another
    pub cmds: &'static [&'static str],
    pub risk: Risk,
    pub params: &'static [Param],
    /// physical drive roles the action applies to (empty = all)
    pub roles: &'static [Role],
    /// Some(true) = switches the locate LED on, Some(false) = off
    pub led: Option<bool>,
}

pub const fn act(key: char, label: &'static str, target: Target, risk: Risk, cmds: &'static [&'static str]) -> ActionSpec {
    ActionSpec { key, label, target, cmds, risk, params: &[], roles: &[], led: None }
}

impl ActionSpec {
    pub const fn with(mut self, params: &'static [Param]) -> Self {
        self.params = params;
        self
    }
    pub const fn led(mut self, on: bool) -> Self {
        self.led = Some(on);
        self
    }
    pub const fn roles(mut self, roles: &'static [Role]) -> Self {
        self.roles = roles;
        self
    }
    pub fn applies(&self, role: Role) -> bool {
        self.roles.is_empty() || self.roles.contains(&role)
    }
}

#[derive(Debug)]
pub struct ToolSpec {
    pub id: &'static str,
    pub name: &'static str,
    /// tools of one group manage the same controllers: only the first one found is used
    pub group: &'static str,
    pub bins: &'static [&'static str],
    /// install directories searched in addition to PATH
    pub dirs: &'static [&'static str],
    /// kernel drivers of the controllers this tool manages
    pub drivers: &'static [&'static str],
    /// PCI vendor or subsystem vendor ids ("0x103c"); empty = any
    pub pci_vendors: &'static [&'static str],
    pub dialect: Dialect,
    /// named query commands (arguments, may use `{ctl}`)
    pub queries: &'static [(&'static str, &'static str)],
    /// exact state words (lowercase) overriding the default health classification
    pub states: &'static [(&'static str, Health)],
    /// separator of drive tokens in "create" commands
    pub drive_sep: &'static str,
    pub actions: &'static [ActionSpec],
    /// where to get the tool
    pub install: &'static str,
    /// verified against real hardware
    pub tested: bool,
}

impl ToolSpec {
    pub fn query(&self, key: &str) -> Option<&'static str> {
        self.queries.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    pub fn health(&self, state: &str) -> Health {
        let s = state.trim().to_lowercase();
        self.states.iter().find(|(k, _)| *k == s).map(|(_, h)| *h).unwrap_or_else(|| health_of(state))
    }

    pub fn actions_for(&self, t: Target) -> impl Iterator<Item = &ActionSpec> {
        self.actions.iter().filter(move |a| a.target == t)
    }

    /// Does a PCI controller belong to this tool?
    pub fn matches(&self, d: &PciDev) -> bool {
        self.drivers.contains(&d.driver.as_str())
            && (self.pci_vendors.is_empty() || self.pci_vendors.iter().any(|v| v.eq_ignore_ascii_case(&d.vendor) || v.eq_ignore_ascii_case(&d.subvendor)))
    }
}

/// Render a command template into arguments.
///
/// `{name}` is replaced by a value; `[token]` is dropped when one of its placeholders is empty;
/// `{name*}` splits the value into several arguments (drive lists like `0 1 0 2`).
pub fn render(tpl: &str, vars: &[(String, String)]) -> Result<Vec<String>> {
    let get = |k: &str| vars.iter().rev().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
    let mut out = Vec::new();
    for tok in tpl.split_whitespace() {
        let (optional, t) = match tok.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            Some(t) => (true, t),
            None => (false, tok),
        };
        let mut s = String::new();
        let mut split = false;
        let mut missing = false;
        let mut rest = t;
        while let Some(i) = rest.find('{') {
            s.push_str(&rest[..i]);
            let Some(j) = rest[i..].find('}') else { bail!("bad template: {tpl}") };
            let mut name = &rest[i + 1..i + j];
            if let Some(n) = name.strip_suffix('*') {
                name = n;
                split = true;
            }
            match get(name) {
                Some(v) if !v.is_empty() => s.push_str(v),
                _ if optional => missing = true,
                Some(_) => bail!("{name} is empty"),
                None => bail!("missing value {name}"),
            }
            rest = &rest[i + j + 1..];
        }
        s.push_str(rest);
        if optional && missing {
            continue;
        }
        if split {
            out.extend(s.split_whitespace().map(String::from));
        } else {
            out.push(s);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// A PCI mass storage controller.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PciDev {
    pub addr: String,
    pub class: String,
    pub vendor: String,
    pub device: String,
    pub subvendor: String,
    pub subdevice: String,
    pub driver: String,
}

/// A tool binary that was found and will be used.
#[derive(Clone, Debug)]
pub struct Found {
    pub spec: &'static ToolSpec,
    pub path: String,
}

#[derive(Clone, Debug, Default)]
pub struct Detection {
    /// storage controllers some tool knows about
    pub pci: Vec<PciDev>,
    pub tools: Vec<Found>,
    /// controllers without a usable tool: (controller, what to install)
    pub missing: Vec<(PciDev, &'static ToolSpec)>,
    /// SES enclosures of the kernel (/sys/class/enclosure)
    pub ses: Vec<SesEnclosure>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SesEnclosure {
    pub path: String,
    /// logical id / SAS address ("0x500e004aaaaaaa1e")
    pub id: String,
    pub slots: Vec<SesSlot>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SesSlot {
    pub slot: u32,
    pub path: String,
    pub locate: bool,
    pub fault: bool,
    /// block device of the disk in the slot, when the OS sees it (sdc)
    pub dev: String,
    pub status: String,
}

/// Shell script listing PCI mass storage controllers and installed tool binaries.
pub fn detect_script(specs: &[ToolSpec]) -> String {
    let mut s = String::from(
        "for d in /sys/bus/pci/devices/*; do c=$(cat $d/class 2>/dev/null); case $c in 0x01*) \
         drv=-; [ -e $d/driver ] && drv=$(basename $(readlink $d/driver)); \
         echo PCI $(basename $d) $c $(cat $d/vendor) $(cat $d/device) $(cat $d/subsystem_vendor) $(cat $d/subsystem_device) $drv;; esac; done\n",
    );
    let mut paths: Vec<String> = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    for t in specs {
        for b in t.bins {
            for d in t.dirs {
                paths.push(format!("{d}/{b}"));
            }
            if !names.contains(b) {
                names.push(b);
            }
        }
    }
    s.push_str(&format!("for p in {}; do [ -x \"$p\" ] && echo BIN $p; done\n", paths.join(" ")));
    s.push_str(&format!("for b in {}; do p=$(command -v $b 2>/dev/null) && echo BIN $p; done\n", names.join(" ")));
    // SES enclosures: slot LEDs (paths contain spaces, so fields are separated by |)
    s.push_str(
        "for e in /sys/class/enclosure/*; do [ -d \"$e\" ] || continue; echo \"SES|$e|$(cat \"$e/id\" 2>/dev/null)\"; \
         for d in \"$e\"/*/; do [ -f \"$d/slot\" ] || continue; b=$(ls \"$d/device/block\" 2>/dev/null | head -n1); \
         echo \"SLOT|$e|${d%/}|$(cat \"$d/slot\")|$(cat \"$d/locate\" 2>/dev/null)|$(cat \"$d/fault\" 2>/dev/null)|${b:--}|$(cat \"$d/status\" 2>/dev/null)\"; done; done\ntrue\n",
    );
    s
}

/// Parse the output of [`detect_script`].
pub fn parse_detect(specs: &'static [ToolSpec], out: &str) -> Detection {
    let mut pci = Vec::new();
    let mut bins: Vec<String> = Vec::new();
    let mut ses: Vec<SesEnclosure> = Vec::new();
    for l in out.lines() {
        let p: Vec<&str> = l.split('|').collect();
        match p.first() {
            Some(&"SES") if p.len() >= 3 => {
                ses.push(SesEnclosure { path: p[1].into(), id: p[2].trim().into(), slots: vec![] });
                continue;
            }
            Some(&"SLOT") if p.len() >= 8 => {
                if let Some(e) = ses.iter_mut().find(|e| e.path == p[1]) {
                    if let Ok(slot) = p[3].trim().parse() {
                        e.slots.push(SesSlot {
                            slot,
                            path: p[2].into(),
                            locate: p[4].trim() == "1",
                            fault: p[5].trim() == "1",
                            dev: if p[6] == "-" { String::new() } else { p[6].trim().into() },
                            status: p[7..].join("|").trim().into(),
                        });
                    }
                }
                continue;
            }
            _ => {}
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        match f.first() {
            Some(&"PCI") if f.len() >= 8 => pci.push(PciDev {
                addr: f[1].into(),
                class: f[2].into(),
                vendor: f[3].into(),
                device: f[4].into(),
                subvendor: f[5].into(),
                subdevice: f[6].into(),
                driver: f[7].into(),
            }),
            Some(&"BIN") if f.len() >= 2
                && !bins.contains(&f[1].to_string()) => {
                    bins.push(f[1].to_string());
                }
            _ => {}
        }
    }
    let pci: Vec<PciDev> = pci.into_iter().filter(|d| specs.iter().any(|s| s.matches(d))).collect();
    let no_pci_info = !out.contains("PCI ");
    let mut tools: Vec<Found> = Vec::new();
    for spec in specs {
        if tools.iter().any(|t| t.spec.group == spec.group) {
            continue;
        }
        let used = no_pci_info || spec.drivers.is_empty() || pci.iter().any(|d| spec.matches(d));
        if !used {
            continue;
        }
        // preferred binary name first
        let path = spec.bins.iter().find_map(|b| bins.iter().find(|p| p.rsplit('/').next() == Some(*b)));
        if let Some(p) = path {
            tools.push(Found { spec, path: p.clone() });
        }
    }
    let mut missing = Vec::new();
    for d in &pci {
        if !tools.iter().any(|t| t.spec.matches(d)) {
            if let Some(s) = specs.iter().find(|s| s.matches(d)) {
                missing.push((d.clone(), s));
            }
        }
    }
    Detection { pci, tools, missing, ses }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn render_templates() {
        let vars = v(&[("ctl", "0"), ("pd", "/e4/s1"), ("name", ""), ("drives", "0 1 0 2")]);
        assert_eq!(render("/c{ctl}{pd} start locate", &vars).unwrap(), vec!["/c0/e4/s1", "start", "locate"]);
        assert_eq!(render("add vd r1 [name={name}] J", &vars).unwrap(), vec!["add", "vd", "r1", "J"]);
        assert_eq!(render("CREATE {ctl} LOGICALDRIVE MAX 1 {drives*}", &vars).unwrap(), vec!["CREATE", "0", "LOGICALDRIVE", "MAX", "1", "0", "1", "0", "2"]);
        assert!(render("{nope}", &vars).is_err());
        assert!(render("{name}", &vars).is_err());
    }
}
