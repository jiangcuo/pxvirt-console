//! Tool independent data model. Every dialect parser fills these structs.

/// Health of an object, ordered by severity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    Ok,
    #[default]
    Unknown,
    Warning,
    Critical,
}

impl Health {
    pub fn label(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Unknown => "unknown",
            Health::Warning => "warning",
            Health::Critical => "critical",
        }
    }
}

/// Raw key/value details shown as-is.
pub type Props = Vec<(String, String)>;

/// Placeholder values for command templates (`{ctl}`, `{vd}`, `{enc}`...).
pub type Addr = Vec<(String, String)>;

/// Role of a physical drive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    /// member of a virtual drive / array
    Member,
    /// passed through to the OS (JBOD / HBA mode)
    Jbod,
    HotSpare,
    /// usable for a new virtual drive
    Unconfigured,
    Failed,
    #[default]
    Other,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Member => "member",
            Role::Jbod => "JBOD",
            Role::HotSpare => "hot spare",
            Role::Unconfigured => "unconfigured",
            Role::Failed => "failed",
            Role::Other => "-",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Battery {
    pub kind: String,
    pub state: String,
    pub health: Health,
    pub props: Props,
}

#[derive(Clone, Debug, Default)]
pub struct VirtualDrive {
    pub addr: Addr,
    /// id unique within the controller ("0/239", "1", "nvme0n1")
    pub id: String,
    pub name: String,
    pub level: String,
    pub size: String,
    pub state: String,
    pub health: Health,
    pub cache: String,
    /// block device in the OS (/dev/sdb)
    pub os_dev: String,
    /// ids of the member physical drives
    pub members: Vec<String>,
    /// running background operation ("Rebuild 45%")
    pub progress: String,
    pub props: Props,
}

#[derive(Clone, Debug, Default)]
pub struct PhysicalDrive {
    pub addr: Addr,
    /// id unique within the controller ("4:1", "1I:1:3")
    pub id: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub size: String,
    /// HDD / SSD / NVMe
    pub media: String,
    /// SAS / SATA / NVMe
    pub intf: String,
    pub state: String,
    pub health: Health,
    pub role: Role,
    /// virtual drive / array / drive group the drive belongs to
    pub group: String,
    pub temp_c: Option<i32>,
    pub os_dev: String,
    pub progress: String,
    /// how the drive is named in "create" commands (`4:1`, `1I:1:3`, `0 3`)
    pub token: String,
    /// locate LED (from the SES enclosure); None = not reported
    pub locate: Option<bool>,
    /// fault LED (from the SES enclosure)
    pub fault_led: Option<bool>,
    /// sysfs directory of the SES enclosure slot (LEDs can be switched there)
    pub ses: String,
    pub props: Props,
}

#[derive(Clone, Debug, Default)]
pub struct Controller {
    /// spec id of the tool that reported it
    pub tool: &'static str,
    pub addr: Addr,
    /// id as used by the tool ("0", "slot=0", "nvme0")
    pub id: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub driver: String,
    pub pci: String,
    /// RAID / JBOD / HBA personality
    pub mode: String,
    pub status: String,
    pub health: Health,
    pub battery: Option<Battery>,
    pub vds: Vec<VirtualDrive>,
    pub pds: Vec<PhysicalDrive>,
    /// background tasks (patrol read, consistency check...)
    pub tasks: Vec<String>,
    /// foreign configurations found on drives
    pub foreign: usize,
    pub enclosures: Vec<Enclosure>,
    pub props: Props,
}

/// An enclosure / backplane as the tool sees it.
#[derive(Clone, Debug, Default)]
pub struct Enclosure {
    /// id used in drive addresses (storcli EID)
    pub id: String,
    /// serial / SAS address (matches the SES enclosure id)
    pub serial: String,
    pub slots: u32,
}

/// Something an operator should look at.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub health: Health,
    pub text: String,
}

impl Controller {
    /// Problems of the controller, its battery, virtual and physical drives.
    pub fn problems(&self) -> Vec<Problem> {
        let mut v = Vec::new();
        let name = format!("{} {}", self.model, self.id);
        let mut push = |h: Health, t: String| {
            if h >= Health::Warning {
                v.push(Problem { health: h, text: t });
            }
        };
        push(self.health, format!("{name}: controller {}", self.status));
        if let Some(b) = &self.battery {
            push(b.health, format!("{name}: {} {}", b.kind, b.state));
        }
        for d in &self.vds {
            push(d.health, format!("{name}: virtual drive {} {} {}", d.id, d.level, d.state));
        }
        for d in &self.pds {
            push(d.health, format!("{name}: drive {} {} {}", d.id, d.model, d.state));
        }
        if self.foreign > 0 {
            v.push(Problem { health: Health::Warning, text: format!("{name}: {} foreign configuration(s)", self.foreign) });
        }
        v
    }

    /// Worst health of everything below the controller.
    pub fn overall(&self) -> Health {
        let mut h = self.health;
        for p in self.problems() {
            h = h.max(p.health);
        }
        h
    }
}

/// Default health classification of a state string. Tool specs can override it.
pub fn health_of(state: &str) -> Health {
    let s = state.trim().to_lowercase();
    if s.is_empty() || s == "-" || s == "n/a" {
        return Health::Unknown;
    }
    const CRIT: [&str; 12] = ["fail", "offline", "offln", "missing", "dead", "ubad", "error", "critical", "fld", "unusable", "not present", "replacement required"];
    const WARN: [&str; 16] = [
        "degrad", "dgrd", "dgd", "pdgd", "rebuild", "rbld", "recover", "predict", "copyback", "cpybck", "foreign", "inconsistent", "out of sync", "osy", "learn", "warning",
    ];
    const OK: [&str; 22] = [
        "optimal", "optl", "onln", "online", "ok", "okay", "oky", "opt", "normal", "ready", "good", "ugood", "jbod", "ghs", "dhs", "hot spare", "hotspare", "present",
        "active", "spare", "unconfigured", "raw",
    ];
    if CRIT.iter().any(|w| s.contains(w)) {
        Health::Critical
    } else if WARN.iter().any(|w| s.contains(w)) {
        Health::Warning
    } else if OK.iter().any(|w| s == *w || s.starts_with(w)) {
        Health::Ok
    } else {
        Health::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_words() {
        assert_eq!(health_of("Optl"), Health::Ok);
        assert_eq!(health_of("Onln"), Health::Ok);
        assert_eq!(health_of("Dgrd"), Health::Warning);
        assert_eq!(health_of("Rbld"), Health::Warning);
        assert_eq!(health_of("Offln"), Health::Critical);
        assert_eq!(health_of("Failed"), Health::Critical);
        assert_eq!(health_of("UBad"), Health::Critical);
        assert_eq!(health_of("Okay (OKY)"), Health::Ok);
        assert_eq!(health_of("Degraded (DGD)"), Health::Warning);
        assert_eq!(health_of("whatever"), Health::Unknown);
    }

    #[test]
    fn problems_collects_children() {
        let c = Controller {
            model: "X".into(),
            id: "0".into(),
            health: Health::Ok,
            status: "Optimal".into(),
            vds: vec![VirtualDrive { id: "0/0".into(), state: "Dgrd".into(), health: Health::Warning, ..Default::default() }],
            pds: vec![PhysicalDrive { id: "4:2".into(), state: "Offln".into(), health: Health::Critical, ..Default::default() }],
            ..Default::default()
        };
        let p = c.problems();
        assert_eq!(p.len(), 2);
        assert_eq!(c.overall(), Health::Critical);
    }
}
