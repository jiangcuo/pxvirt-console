//! Health checks of a node: hardware first (disks / SMART, kernel errors, PCIe AER and links,
//! memory ECC, temperatures, fans / power supplies / BMC events, NICs, RAID), then storage
//! (ZFS, LVM thin, Ceph, file systems, PVE storages), then system, cluster and guests.
//!
//! Collection is tiered: a cheap host script about every minute, an expensive one (SMART,
//! ipmitool, ethtool) every 10 minutes, the RAID scan every 5. Error counters (AER, CRC, ECC,
//! NIC errors, link flaps) only report when they grow; growth stays visible for a day.
//! Findings can be acknowledged; an acknowledged finding comes back when it gets worse.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::api::Api;
use crate::config::HealthCfg;
use crate::exec::Exec;
use crate::util::{jarr, jb, jf, js};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Sev {
    Ok,
    /// worth knowing, not an alert
    Info,
    Warn,
    Crit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cat {
    Disk,
    Kernel,
    Pcie,
    Memory,
    Temp,
    Power,
    Network,
    Raid,
    Zfs,
    Ceph,
    Lvm,
    Storage,
    System,
    Cluster,
    Guests,
}

impl Cat {
    pub const ALL: [Cat; 15] = [
        Cat::Disk,
        Cat::Kernel,
        Cat::Pcie,
        Cat::Memory,
        Cat::Temp,
        Cat::Power,
        Cat::Network,
        Cat::Raid,
        Cat::Zfs,
        Cat::Ceph,
        Cat::Lvm,
        Cat::Storage,
        Cat::System,
        Cat::Cluster,
        Cat::Guests,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Cat::Disk => "Disks",
            Cat::Kernel => "Kernel log",
            Cat::Pcie => "PCIe",
            Cat::Memory => "Memory",
            Cat::Temp => "Temperatures",
            Cat::Power => "Fans / power / BMC",
            Cat::Network => "Network",
            Cat::Raid => "RAID / HBA",
            Cat::Zfs => "ZFS",
            Cat::Ceph => "Ceph",
            Cat::Lvm => "LVM thin",
            Cat::Storage => "Storage",
            Cat::System => "System",
            Cat::Cluster => "Cluster",
            Cat::Guests => "Guests",
        }
    }

    /// Page with more information: (module index, view title).
    pub fn jump(self) -> (usize, &'static str) {
        match self {
            Cat::Disk => (2, "Physical"),
            Cat::Kernel => (3, "Journal"),
            Cat::Pcie => (0, "PCI"),
            Cat::Memory => (0, "Memory"),
            Cat::Temp => (0, "Hardware"),
            Cat::Power => (0, "BMC"),
            Cat::Network => (1, "Interfaces"),
            Cat::Raid => (2, "Hardware RAID"),
            Cat::Zfs => (2, "ZFS"),
            Cat::Ceph => (2, "Ceph"),
            Cat::Lvm => (2, "LVM"),
            Cat::Storage => (4, "Storage"),
            Cat::System => (3, "All Units"),
            Cat::Cluster => (4, "Cluster"),
            Cat::Guests => (4, "Guests"),
        }
    }
}

/// Translatable title: English template with `{}` placeholders and their values.
#[derive(Clone, Debug, PartialEq)]
pub struct Title {
    pub tpl: &'static str,
    pub args: Vec<String>,
}

impl Title {
    fn fill(tpl: &str, args: &[String]) -> String {
        let mut out = String::new();
        let mut it = args.iter();
        let mut rest = tpl;
        while let Some(i) = rest.find("{}") {
            out.push_str(&rest[..i]);
            out.push_str(it.next().map(|s| s.as_str()).unwrap_or(""));
            rest = &rest[i + 2..];
        }
        out.push_str(rest);
        out
    }
    /// In the interface language.
    pub fn text(&self) -> String {
        Self::fill(crate::i18n::tr(self.tpl), &self.args)
    }
    pub fn en(&self) -> String {
        Self::fill(self.tpl, &self.args)
    }
}

impl From<&'static str> for Title {
    fn from(tpl: &'static str) -> Self {
        Title { tpl, args: vec![] }
    }
}

macro_rules! t {
    ($tpl:literal $(, $a:expr)* $(,)?) => {
        Title { tpl: $tpl, args: vec![$($a.to_string()),*] }
    };
}

#[derive(Clone, Debug)]
pub struct Finding {
    /// stable id (used for acknowledgements)
    pub id: String,
    pub cat: Cat,
    pub sev: Sev,
    pub title: Title,
    pub value: String,
    pub detail: Vec<String>,
    pub advice: String,
    /// page to jump to instead of the category's default
    pub jump: Option<(usize, &'static str)>,
    pub acked: bool,
}

impl Finding {
    fn new(id: impl Into<String>, cat: Cat, sev: Sev, title: impl Into<Title>, value: impl Into<String>) -> Self {
        Self { id: id.into(), cat, sev, title: title.into(), value: value.into(), detail: vec![], advice: String::new(), jump: None, acked: false }
    }
    fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail.push(d.into());
        self
    }
    fn advice(mut self, a: &str) -> Self {
        self.advice = a.to_string();
        self
    }
    fn jump(mut self, m: usize, t: &'static str) -> Self {
        self.jump = Some((m, t));
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub at: Option<SystemTime>,
    pub findings: Vec<Finding>,
    /// checks that ran and found nothing: (category, what)
    pub passed: Vec<(Cat, Title)>,
    /// problems running the checks themselves
    pub errors: Vec<String>,
}

impl Report {
    /// Not acknowledged warnings and criticals (the alert counter).
    pub fn alerts(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.sev >= Sev::Warn && !f.acked)
    }

    pub fn worst(&self) -> Sev {
        self.alerts().map(|f| f.sev).max().unwrap_or(Sev::Ok)
    }

    pub fn count(&self, s: Sev) -> usize {
        self.findings.iter().filter(|f| f.sev == s && !f.acked).count()
    }
}

// ---------------------------------------------------------------------------
// State: counter growth, cached tiers, acknowledgements
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counter {
    last: f64,
    grew_at: Option<Instant>,
    growth: f64,
}

/// Result of one collection tier: (when, findings, passed checks, errors).
type Tier = Option<(Instant, Vec<Finding>, Vec<(Cat, Title)>, Vec<String>)>;

#[derive(Default)]
struct HostState {
    counters: HashMap<String, Counter>,
    cheap: Tier,
    slow: Tier,
}

impl HostState {
    /// Track a counter; returns the growth inside the report window (0 = not growing).
    fn grow(&mut self, key: &str, value: f64, window: Duration) -> f64 {
        let c = self.counters.entry(key.to_string()).or_insert_with(|| Counter { last: value, grew_at: None, growth: 0.0 });
        if value > c.last {
            c.growth += value - c.last;
            c.grew_at = Some(Instant::now());
        } else if value < c.last {
            // counter reset (reboot, driver reload)
            c.growth = 0.0;
        }
        c.last = value;
        match c.grew_at {
            Some(t) if t.elapsed() < window => c.growth,
            _ => {
                c.growth = 0.0;
                0.0
            }
        }
    }
}

static STATE: Mutex<Option<HashMap<String, HostState>>> = Mutex::new(None);
static FORCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Next collection runs every tier now ("check now").
pub fn force() {
    FORCE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Acknowledged findings: "<node>|<id>" -> severity that was acknowledged.
fn ack_path() -> String {
    crate::config::state_path("health-ack.json")
}

fn load_acks() -> HashMap<String, String> {
    std::fs::read_to_string(ack_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn set_ack(node: &str, f: &Finding, on: bool) -> std::io::Result<()> {
    let mut a = load_acks();
    let k = format!("{node}|{}", f.id);
    if on {
        a.insert(k, format!("{:?}", f.sev));
    } else {
        a.remove(&k);
    }
    let p = ack_path();
    if let Some(d) = std::path::Path::new(&p).parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(&a).unwrap_or_default())
}

fn apply_acks(node: &str, fs: &mut [Finding]) {
    let a = load_acks();
    for f in fs {
        if let Some(s) = a.get(&format!("{node}|{}", f.id)) {
            // still acknowledged unless it got worse
            let acked_sev = match s.as_str() {
                "Crit" => Sev::Crit,
                "Warn" => Sev::Warn,
                _ => Sev::Info,
            };
            f.acked = f.sev <= acked_sev;
        }
    }
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// Cheap host data, about every minute.
const CHEAP: &str = r#"
echo '@@df'; df -PT -x tmpfs -x devtmpfs -x squashfs -x overlay -x fuse -x fuse.lxcfs 2>/dev/null
echo '@@dfi'; df -PiT -x tmpfs -x devtmpfs -x squashfs -x overlay -x fuse -x fuse.lxcfs 2>/dev/null
echo '@@meminfo'; grep -E '^(MemTotal|MemAvailable|SwapTotal|SwapFree):' /proc/meminfo
echo '@@psi'; cat /proc/pressure/memory 2>/dev/null
echo '@@aer'; for d in /sys/bus/pci/devices/*; do for k in correctable nonfatal fatal; do f=$d/aer_dev_$k; [ -f $f ] && echo "${d##*/} $k $(awk '/^TOTAL/{print $2}' $f)"; done; done
echo '@@link'; for d in /sys/bus/pci/devices/*; do [ -f $d/current_link_width ] || continue; echo "${d##*/}|$(cat $d/class)|$(cat $d/current_link_width)|$(cat $d/max_link_width)|$(cat $d/current_link_speed)|$(cat $d/max_link_speed)"; done
echo '@@lspci'; lspci -mm -D 2>/dev/null
echo '@@edac'; for m in /sys/devices/system/edac/mc/mc*; do [ -d $m ] || continue; echo "MC ${m##*/} $(cat $m/ce_count) $(cat $m/ue_count)"; for d in $m/dimm* $m/rank*; do [ -d $d ] || continue; echo "DIMM ${m##*/}/${d##*/} $(cat $d/dimm_ce_count 2>/dev/null) $(cat $d/dimm_ue_count 2>/dev/null) $(cat $d/dimm_label 2>/dev/null)"; done; done
echo '@@hwmon'; for h in /sys/class/hwmon/hwmon*; do n=$(cat $h/name 2>/dev/null); for t in $h/temp*_input; do [ -e "$t" ] || continue; b=${t%_input}; echo "$n|$(cat ${b}_label 2>/dev/null)|$(cat $t 2>/dev/null)|$(cat ${b}_max 2>/dev/null)|$(cat ${b}_crit 2>/dev/null)"; done; done
echo '@@net'; for i in /sys/class/net/*; do [ -e $i/device ] || continue; m=-; [ -e $i/master ] && m=$(basename $(readlink $i/master)); echo "${i##*/}|$(cat $i/operstate)|$(cat $i/speed 2>/dev/null)|$(cat $i/carrier_changes 2>/dev/null)|$(cat $i/statistics/rx_errors)|$(cat $i/statistics/tx_errors)|$m"; done
echo '@@kern'; journalctl -k -b -p warning -o short-unix --no-pager 2>/dev/null | grep -E 'I/O error|blk_update_request|Medium Error|critical medium error|blocked for more than|EXT4-fs error|XFS .*([Ee]rror|[Cc]orrupt)|BTRFS (error|critical)|Out of memory|oom-kill|soft lockup|hard LOCKUP|rcu_.*stall|AER:|Hardware Error|Machine check|EDAC .*(CE|UE)|not responding|temperature above threshold|clock throttled|segfault' | tail -n 3000
echo '@@kerr'; journalctl -k -b -p err -o short-unix --no-pager 2>/dev/null | tail -n 200
echo '@@failed'; systemctl --failed --no-legend --plain 2>/dev/null
echo '@@chrony'; chronyc -n tracking 2>/dev/null || timedatectl show 2>/dev/null
echo '@@kernel'; uname -r; ls /boot/vmlinuz-* 2>/dev/null | sed 's|/boot/vmlinuz-||' | sort -V | tail -n1; [ -e /var/run/reboot-required ] && echo REBOOT-REQUIRED
echo '@@certs'; for c in /etc/pve/local/pve-ssl.pem /etc/pve/local/pveproxy-ssl.pem /etc/pve/pve-root-ca.pem; do [ -f $c ] && echo "$c|$(openssl x509 -enddate -noout -in $c 2>/dev/null | cut -d= -f2)"; done
echo '@@zpool'; zpool list -Hp -o name,size,alloc,health,cap,frag 2>/dev/null
echo '@@zstatus'; zpool status -p 2>/dev/null
echo '@@lvs'; lvs --noheadings --separator '|' -o vg_name,lv_name,lv_attr,data_percent,metadata_percent 2>/dev/null
echo '@@ceph'; [ -e /etc/pve/ceph.conf ] && timeout 10 ceph health detail -f json 2>/dev/null
echo '@@corosync'; corosync-cfgtool -s 2>/dev/null
echo '@@ipmidev'; ls /dev/ipmi* 2>/dev/null; command -v ipmitool
true
"#;

/// Expensive host data, every 10 minutes.
const SLOW: &str = r#"
smartctl --scan-open 2>/dev/null | while read -r dev d typ rest; do [ "$d" = "-d" ] || continue; echo "@@smart $dev $typ"; smartctl -j -a "$dev" -d "$typ" 2>/dev/null; done
if command -v ipmitool >/dev/null 2>&1 && ls /dev/ipmi* >/dev/null 2>&1; then echo '@@sdr'; timeout 30 ipmitool sdr elist 2>/dev/null; echo '@@selinfo'; timeout 20 ipmitool sel info 2>/dev/null; echo '@@sel'; timeout 60 ipmitool sel elist 2>/dev/null | tail -n 500; fi
if command -v ethtool >/dev/null 2>&1; then for i in /sys/class/net/*; do [ -e $i/device ] || continue; echo "@@ethtool ${i##*/}"; ethtool ${i##*/} 2>/dev/null | sed -n '/Supported link modes/,/Supported pause/p'; done; fi
true
"#;

/// Split "@@name args" sections.
fn sections(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for l in text.lines() {
        if let Some(h) = l.strip_prefix("@@") {
            out.push((h.trim().to_string(), String::new()));
        } else if let Some(last) = out.last_mut() {
            last.1.push_str(l);
            last.1.push('\n');
        }
    }
    out
}

fn sec<'a>(s: &'a [(String, String)], name: &str) -> &'a str {
    s.iter().find(|(n, _)| n == name).map(|(_, b)| b.as_str()).unwrap_or("")
}

/// Run all due tiers and build the report.
pub fn collect(api: &Api, node: &str, exec: Option<Exec>, th: &HealthCfg) -> Report {
    let key = exec.as_ref().map(|e| e.label()).unwrap_or_else(|| format!("api:{node}"));
    let force = FORCE.swap(false, std::sync::atomic::Ordering::Relaxed);
    let mut rep = Report { at: Some(SystemTime::now()), ..Default::default() };
    let window = Duration::from_secs_f64(th.growth_hours * 3600.0);
    if let Some(e) = &exec {
        let due = |t: &Tier, every: u64| force || t.as_ref().map(|x| x.0.elapsed() >= Duration::from_secs(every)).unwrap_or(true);
        let (cheap_due, slow_due) = {
            let g = STATE.lock().unwrap();
            let st = g.as_ref().and_then(|m| m.get(&key));
            (st.map(|s| due(&s.cheap, 55)).unwrap_or(true), st.map(|s| due(&s.slow, 600)).unwrap_or(true))
        };
        // run the scripts without holding the lock
        let cheap_out = cheap_due.then(|| e.output_any("sh", &["-c", CHEAP]).map(|x| x.1));
        let slow_out = slow_due.then(|| e.output_any("sh", &["-c", SLOW]).map(|x| x.1));
        let mut g = STATE.lock().unwrap();
        let st = g.get_or_insert_with(HashMap::new).entry(key.clone()).or_default();
        if let Some(r) = cheap_out {
            let (mut f, mut p, mut errs) = (vec![], vec![], vec![]);
            match r {
                Ok(out) => host_checks(&sections(&out), th, st, window, &mut f, &mut p),
                Err(e) => errs.push(format!("host checks: {e:#}")),
            }
            st.cheap = Some((Instant::now(), std::mem::take(&mut f), std::mem::take(&mut p), std::mem::take(&mut errs)));
        }
        if let Some(r) = slow_out {
            let (mut f, mut p, mut errs) = (vec![], vec![], vec![]);
            match r {
                Ok(out) => slow_checks(&sections(&out), th, st, window, &mut f, &mut p),
                Err(e) => errs.push(format!("SMART / BMC checks: {e:#}")),
            }
            st.slow = Some((Instant::now(), f, p, errs));
        }
        for t in [&st.cheap, &st.slow].into_iter().flatten() {
            rep.findings.extend(t.1.iter().cloned());
            rep.passed.extend(t.2.iter().cloned());
            rep.errors.extend(t.3.iter().cloned());
        }
        drop(g);
        // RAID / HBA / NVMe tools (own 5 minute cache)
        let raid = crate::raidhost::problems_cached(e);
        if raid.is_empty() {
            rep.passed.push((Cat::Raid, "controllers and drives".into()));
        }
        for (i, (h, t)) in raid.into_iter().enumerate() {
            let sev = match h {
                pxvirt_raid::Health::Critical => Sev::Crit,
                pxvirt_raid::Health::Warning => Sev::Warn,
                _ => Sev::Info,
            };
            rep.findings.push(Finding::new(format!("raid:{i}:{t}"), Cat::Raid, sev, "RAID / HBA", t).advice("Open Disks → Hardware RAID for details and actions."));
        }
    }
    api_checks(api, node, th, &mut rep);
    rep.findings.sort_by(|a, b| b.sev.cmp(&a.sev).then(a.cat.cmp(&b.cat)).then(a.title.en().cmp(&b.title.en())));
    apply_acks(node, &mut rep.findings);
    rep
}

// ---------------------------------------------------------------------------
// Host checks (cheap tier)
// ---------------------------------------------------------------------------

fn host_checks(s: &[(String, String)], th: &HealthCfg, st: &mut HostState, window: Duration, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    fs_checks(sec(s, "df"), sec(s, "dfi"), th, out, passed);
    mem_checks(sec(s, "meminfo"), sec(s, "psi"), th, out, passed);
    let names = lspci_names(sec(s, "lspci"));
    pcie_checks(sec(s, "aer"), sec(s, "link"), &names, st, window, out, passed);
    edac_checks(sec(s, "edac"), st, window, out, passed);
    hwmon_checks(sec(s, "hwmon"), th, out, passed);
    net_checks(sec(s, "net"), st, window, out, passed);
    kernel_checks(sec(s, "kern"), sec(s, "kerr"), out, passed);
    failed_units(sec(s, "failed"), out, passed);
    time_checks(sec(s, "chrony"), th, out, passed);
    reboot_check(sec(s, "kernel"), out);
    cert_checks(sec(s, "certs"), th, out, passed);
    zfs_checks(sec(s, "zpool"), sec(s, "zstatus"), th, out, passed);
    lvm_checks(sec(s, "lvs"), th, out, passed);
    ceph_checks(sec(s, "ceph"), out, passed);
    corosync_checks(sec(s, "corosync"), out);
    let ipmi = sec(s, "ipmidev");
    if ipmi.contains("/dev/ipmi") && !ipmi.contains("ipmitool") {
        out.push(
            Finding::new("bmc:no-ipmitool", Cat::Power, Sev::Info, "BMC present, ipmitool missing", "no sensors / event log")
                .advice("Install ipmitool (apt install ipmitool, or I on System → BMC) to check fans, power supplies, voltages and the BMC event log.")
                .jump(0, "BMC"),
        );
    }
}

fn pct(v: &str) -> f64 {
    v.trim().trim_end_matches('%').parse().unwrap_or(0.0)
}

pub fn fs_checks(df: &str, dfi: &str, th: &HealthCfg, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    const MOUNTS: [&str; 7] = ["/", "/var", "/var/log", "/tmp", "/boot", "/boot/efi", "/var/lib/vz"];
    let mut ok = true;
    for (text, what) in [(df, "space"), (dfi, "inodes")] {
        for l in text.lines().skip(1) {
            let w: Vec<&str> = l.split_whitespace().collect();
            if w.len() < 7 || !MOUNTS.contains(&w[6]) {
                continue;
            }
            let p = pct(w[5]);
            let sev = if p >= th.fs_crit { Sev::Crit } else if p >= th.fs_warn { Sev::Warn } else { continue };
            ok = false;
            out.push(
                Finding::new(format!("fs:{what}:{}", w[6]), Cat::System, sev, t!("File system {} {}", w[6], what), format!("{p:.0}% used"))
                    .detail(format!("{} ({})", w[0], w[1]))
                    .advice(if what == "space" { "Free space: old logs (journalctl --vacuum-size), apt cache (apt clean), old kernels, backups or ISOs on local storage." } else { "Many small files: look for runaway caches, mail queues or session files." })
                    .jump(2, "Partitions"),
            );
        }
    }
    if ok {
        passed.push((Cat::System, "file systems".into()));
    }
}

fn mem_checks(meminfo: &str, psi: &str, th: &HealthCfg, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let get = |k: &str| meminfo.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let (st, sf) = (get("SwapTotal:"), get("SwapFree:"));
    let mut ok = true;
    if st > 0.0 {
        let used = (st - sf) / st * 100.0;
        if used >= th.swap_warn {
            ok = false;
            out.push(Finding::new("mem:swap", Cat::Memory, Sev::Warn, "Swap usage", format!("{used:.0}% used")).advice("The node is short of memory: check ballooning, guest memory and ZFS ARC limit."));
        }
    }
    // some avg60: share of time tasks waited for memory
    if let Some(v) = psi.lines().find(|l| l.starts_with("some")).and_then(|l| l.split_whitespace().find_map(|w| w.strip_prefix("avg60="))).and_then(|v| v.parse::<f64>().ok()) {
        if v >= th.mem_pressure_warn {
            ok = false;
            out.push(Finding::new("mem:psi", Cat::Memory, Sev::Warn, "Memory pressure", format!("{v:.1}% of time stalled (1 min)")).advice("Processes wait for memory: reduce guest memory or the ZFS ARC limit."));
        }
    }
    if ok {
        passed.push((Cat::Memory, "swap and memory pressure".into()));
    }
}

fn lspci_names(text: &str) -> HashMap<String, String> {
    // 0000:01:00.0 "Ethernet controller" "Intel Corporation" "Ethernet Controller X710" ...
    text.lines()
        .filter_map(|l| {
            let (addr, rest) = l.split_once(' ')?;
            let q: Vec<&str> = rest.split('"').filter(|x| !x.trim().is_empty()).collect();
            Some((addr.to_string(), format!("{} {}", q.get(1).unwrap_or(&""), q.get(2).unwrap_or(&"")).trim().to_string()))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn pcie_checks(aer: &str, link: &str, names: &HashMap<String, String>, st: &mut HostState, window: Duration, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let mut ok = true;
    let name = |a: &str| names.get(a).cloned().unwrap_or_default();
    for l in aer.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        if w.len() < 3 {
            continue;
        }
        let v: f64 = w[2].parse().unwrap_or(0.0);
        let id = format!("aer:{}:{}", w[0], w[1]);
        match w[1] {
            "correctable" => {
                let g = st.grow(&id, v, window);
                if g > 0.0 {
                    ok = false;
                    out.push(
                        Finding::new(id, Cat::Pcie, Sev::Warn, t!("PCIe corrected errors {}", w[0]), format!("+{g:.0} (total {v:.0})"))
                            .detail(name(w[0]))
                            .advice("Corrected errors growing: bad riser / slot / cable or signal problems; reseat the card, check firmware."),
                    );
                }
            }
            _ if v > 0.0 => {
                ok = false;
                out.push(
                    Finding::new(id, Cat::Pcie, Sev::Crit, t!("PCIe {} errors {}", w[1], w[0]), format!("{v:.0} since boot"))
                        .detail(name(w[0]))
                        .advice("Uncorrectable PCIe errors: data may have been lost. Check the device, slot and firmware; plan a replacement."),
                );
            }
            _ => {}
        }
    }
    for l in link.lines() {
        let f: Vec<&str> = l.split('|').collect();
        if f.len() < 6 || f[1].starts_with("0x0604") {
            continue; // bridges
        }
        let (cw, mw): (u32, u32) = (f[2].parse().unwrap_or(0), f[3].parse().unwrap_or(0));
        // width 0 / 255 = no external link (integrated devices)
        if cw == 0 || mw == 0 || mw == 255 || cw >= mw {
            continue;
        }
        ok = false;
        out.push(
            Finding::new(format!("link:{}", f[0]), Cat::Pcie, Sev::Warn, t!("PCIe link narrower than possible {}", f[0]), format!("x{cw} of x{mw}"))
                .detail(name(f[0]))
                .detail(format!("speed {} (max {})", f[4], f[5]))
                .advice("The card runs with fewer lanes than it supports: slot wired narrower, riser, or a bad contact. Throughput is limited."),
        );
    }
    if ok {
        passed.push((Cat::Pcie, "AER errors and link widths".into()));
    }
}

fn edac_checks(text: &str, st: &mut HostState, window: Duration, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let mut ok = true;
    let mut any = false;
    for l in text.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        if w.len() < 4 || w[0] != "MC" && w[0] != "DIMM" {
            continue;
        }
        any = true;
        let (ce, ue): (f64, f64) = (w[2].parse().unwrap_or(0.0), w[3].parse().unwrap_or(0.0));
        let label = if w[0] == "DIMM" { w.get(4..).map(|x| x.join(" ")).unwrap_or_default() } else { String::new() };
        let what = if label.is_empty() { w[1].to_string() } else { format!("{} ({label})", w[1]) };
        if ue > 0.0 {
            ok = false;
            out.push(Finding::new(format!("ecc:ue:{}", w[1]), Cat::Memory, Sev::Crit, "Uncorrectable memory errors", format!("{ue:.0} on {what}")).advice("Replace the memory module; uncorrectable errors can crash the node or corrupt data."));
        }
        if w[0] == "MC" {
            let g = st.grow(&format!("ecc:ce:{}", w[1]), ce, window);
            if g > 0.0 {
                ok = false;
                out.push(Finding::new(format!("ecc:ce:{}", w[1]), Cat::Memory, Sev::Warn, "Corrected memory errors growing", format!("+{g:.0} on {what} (total {ce:.0})")).advice("A memory module is degrading: check the BMC log for the DIMM slot and plan a replacement."));
            }
        }
    }
    if any && ok {
        passed.push((Cat::Memory, "ECC errors".into()));
    }
}

fn hwmon_checks(text: &str, th: &HealthCfg, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let mut ok = true;
    let mut n = 0;
    for l in text.lines() {
        let f: Vec<&str> = l.split('|').collect();
        if f.len() < 5 {
            continue;
        }
        let chip = f[0];
        // disks and NVMe are checked through SMART
        if chip == "nvme" || chip == "drivetemp" {
            continue;
        }
        let Ok(t) = f[2].trim().parse::<f64>() else { continue };
        let t = t / 1000.0;
        let max = f[3].trim().parse::<f64>().ok().map(|v| v / 1000.0).filter(|v| *v > 0.0);
        let crit = f[4].trim().parse::<f64>().ok().map(|v| v / 1000.0).filter(|v| *v > 0.0);
        let cpu = ["coretemp", "k10temp", "zenpower", "cpu_thermal"].contains(&chip);
        let (warn, critl) = match (max, crit, cpu) {
            (Some(m), Some(c), _) => (m, c),
            (None, Some(c), _) => (c - 10.0, c),
            (Some(m), None, _) => (m, m + 10.0),
            (None, None, true) => (th.cpu_temp_warn, th.cpu_temp_crit),
            _ => continue,
        };
        n += 1;
        let sev = if t >= critl { Sev::Crit } else if t >= warn { Sev::Warn } else { continue };
        ok = false;
        let label = if f[1].trim().is_empty() { chip.to_string() } else { format!("{chip} {}", f[1].trim()) };
        out.push(Finding::new(format!("temp:{label}"), Cat::Temp, sev, t!("Temperature {}", label), format!("{t:.0} °C (warn {warn:.0}, crit {critl:.0})")).advice("Check fans, air flow, dust and the room temperature."));
    }
    if n > 0 && ok {
        passed.push((Cat::Temp, t!("{} sensors", n)));
    }
}

fn net_checks(text: &str, st: &mut HostState, window: Duration, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let mut ok = true;
    for l in text.lines() {
        let f: Vec<&str> = l.split('|').collect();
        if f.len() < 7 {
            continue;
        }
        let (name, oper, master) = (f[0], f[1], f[6]);
        let used = master != "-";
        if used && oper != "up" {
            ok = false;
            out.push(Finding::new(format!("nic:down:{name}"), Cat::Network, Sev::Crit, t!("NIC {} is down", name), format!("{oper}, member of {master}")).advice("Check the cable, the switch port and the transceiver."));
            continue;
        }
        // speed below what NIC supports (ethtool, slow tier)
        let speed: f64 = f[2].trim().parse().unwrap_or(0.0);
        let max = st.counters.get(&format!("nicmax:{name}")).map(|c| c.last).unwrap_or(0.0);
        if used && speed > 0.0 && max > speed {
            out.push(Finding::new(format!("nic:speed:{name}"), Cat::Network, Sev::Info, t!("NIC {} below its maximum speed", name), format!("{speed:.0} Mb/s of {max:.0} Mb/s")).advice("Negotiated a lower speed: check the cable category, transceiver and switch port settings."));
        }
        let flaps = st.grow(&format!("nic:flap:{name}"), f[3].parse().unwrap_or(0.0), window);
        if flaps > 0.0 && used {
            ok = false;
            out.push(Finding::new(format!("nic:flap:{name}"), Cat::Network, Sev::Warn, t!("NIC {} link flaps", name), format!("{:.0} link changes", flaps / 2.0)).advice("The link goes down and up: cable, transceiver or switch port."));
        }
        let errs: f64 = f[4].parse::<f64>().unwrap_or(0.0) + f[5].parse::<f64>().unwrap_or(0.0);
        let g = st.grow(&format!("nic:err:{name}"), errs, window);
        if g > 0.0 {
            ok = false;
            out.push(Finding::new(format!("nic:err:{name}"), Cat::Network, Sev::Warn, t!("NIC {} errors growing", name), format!("+{g:.0} rx/tx errors")).advice("Errors on the wire: cable, transceiver, duplex mismatch or a bad switch port."));
        }
    }
    if ok {
        passed.push((Cat::Network, "NIC links and errors".into()));
    }
}

/// Kernel log patterns: (id, category, severity, title, regex-like needles, advice).
type KernPattern = (&'static str, Cat, Sev, &'static str, &'static [&'static str], &'static str);

const KERN: &[KernPattern] = &[
    ("io", Cat::Disk, Sev::Crit, "Disk I/O errors", &["I/O error", "blk_update_request", "Medium Error", "critical medium error"], "A disk or its path returns errors: check SMART, cables and the controller; replace the disk."),
    ("hung", Cat::Kernel, Sev::Warn, "Tasks blocked for minutes (hung tasks)", &["blocked for more than"], "Processes waited minutes for IO: storage was stalled (slow / failing disk, NFS / Ceph problem)."),
    ("fs", Cat::Kernel, Sev::Crit, "File system errors", &["EXT4-fs error", "BTRFS error", "BTRFS critical", "XFS "], "File system corruption: run a check in maintenance, restore from backup if needed."),
    ("oom", Cat::Memory, Sev::Warn, "Out of memory: processes killed", &["Out of memory", "oom-kill"], "The kernel killed processes (maybe a VM): add memory or reduce guest memory / ZFS ARC."),
    ("lockup", Cat::Kernel, Sev::Crit, "CPU lockups / RCU stalls", &["soft lockup", "hard LOCKUP", "stall"], "CPUs stopped responding: firmware / microcode / hardware or driver problem."),
    ("aer", Cat::Pcie, Sev::Warn, "PCIe AER messages", &["AER:"], "PCIe errors reported by the kernel, see the PCIe section."),
    ("mce", Cat::Memory, Sev::Crit, "Machine check / hardware errors", &["Hardware Error", "Machine check", "EDAC"], "The CPU or memory reported hardware errors: check the BMC log, memory and CPU."),
    ("nfs", Cat::Storage, Sev::Warn, "NFS server not responding", &["not responding"], "An NFS server did not answer: check the server and the network."),
    ("thermal", Cat::Temp, Sev::Warn, "Thermal throttling", &["temperature above threshold", "clock throttled"], "CPUs were throttled because of heat: check cooling."),
    ("segv", Cat::Kernel, Sev::Info, "Programs crashed (segfault)", &["segfault"], "A program crashed; repeated crashes of PVE daemons need attention."),
];

fn kernel_checks(kern: &str, kerr: &str, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let mut hits: HashMap<&str, (usize, String, String)> = HashMap::new();
    for l in kern.lines() {
        if let Some(k) = KERN.iter().find(|k| k.4.iter().any(|n| l.contains(n))) {
            let e = hits.entry(k.0).or_insert((0, String::new(), String::new()));
            e.0 += 1;
            if e.1.is_empty() {
                e.1 = l.to_string();
            }
            e.2 = l.to_string();
        }
    }
    let when = |l: &str| l.split_whitespace().next().and_then(|t| t.split('.').next()).and_then(|t| t.parse::<i64>().ok()).map(crate::util::fmt_time).unwrap_or_default();
    let msg = |l: &str| l.split_once(": ").map(|x| x.1.to_string()).unwrap_or_else(|| l.to_string());
    for k in KERN {
        if let Some((n, first, last)) = hits.get(k.0) {
            let mut f = Finding::new(format!("kern:{}", k.0), k.1, k.2, k.3, format!("{n} since boot, last {}", when(last))).advice(k.5).jump(3, "Journal");
            f = f.detail(format!("first {}: {}", when(first), msg(first)));
            if n > &1 {
                f = f.detail(format!("last  {}: {}", when(last), msg(last)));
            }
            out.push(f);
        }
    }
    // other kernel errors: informational
    let other: Vec<&str> = kerr.lines().filter(|l| !l.trim().is_empty() && !KERN.iter().any(|k| k.4.iter().any(|n| l.contains(n)))).collect();
    if !other.is_empty() {
        let mut f = Finding::new("kern:other", Cat::Kernel, Sev::Info, "Other kernel errors", format!("{} since boot", other.len())).jump(3, "Journal");
        for l in other.iter().rev().take(5) {
            f = f.detail(format!("{}: {}", when(l), msg(l)));
        }
        out.push(f);
    }
    if hits.is_empty() {
        passed.push((Cat::Kernel, "no disk, file system, memory or lockup errors since boot".into()));
    }
}

const PVE_CORE: [&str; 6] = ["pveproxy", "pvedaemon", "pve-cluster", "corosync", "pvestatd", "pve-ha-lrm"];

fn failed_units(text: &str, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let units: Vec<&str> = text.lines().filter_map(|l| l.split_whitespace().next()).filter(|u| u.contains('.')).collect();
    for u in &units {
        let core = PVE_CORE.iter().any(|c| u.starts_with(c));
        out.push(Finding::new(format!("unit:{u}"), Cat::System, if core { Sev::Crit } else { Sev::Warn }, t!("Service {} failed", u), "").advice("Open Services & Logs → All Units: view the log, restart or reset the failed state.").jump(3, "All Units"));
    }
    if units.is_empty() {
        passed.push((Cat::System, "services".into()));
    }
}

fn time_checks(text: &str, th: &HealthCfg, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    if text.trim().is_empty() {
        return;
    }
    // chronyc: "System time     : 0.000117757 seconds slow of NTP time", "Leap status     : Normal"
    let line = |k: &str| text.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_once(':')).map(|x| x.1.trim().to_string());
    if let Some(v) = line("System time") {
        let off: f64 = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
        let leap = line("Leap status").unwrap_or_default();
        let sev = if off >= th.time_offset_crit { Sev::Crit } else if off >= th.time_offset_warn || leap.contains("Not synchronised") { Sev::Warn } else { Sev::Ok };
        if sev > Sev::Ok {
            out.push(Finding::new("time:offset", Cat::System, sev, "Clock not in sync", format!("offset {off:.3} s, {leap}")).advice("Check the NTP servers on System → Settings and the network to them; the cluster and certificates need correct time.").jump(0, "Settings"));
        } else {
            passed.push((Cat::System, t!("time in sync ({} s)", format!("{off:.3}"))));
        }
    } else if text.contains("NTPSynchronized=no") {
        out.push(Finding::new("time:offset", Cat::System, Sev::Warn, "Clock not in sync", "NTP not synchronized").advice("Enable and check NTP (chrony).").jump(0, "Settings"));
    }
}

fn reboot_check(text: &str, out: &mut Vec<Finding>) {
    let l: Vec<&str> = text.lines().collect();
    if l.len() >= 2 && !l[1].is_empty() && l[0] != l[1] || text.contains("REBOOT-REQUIRED") {
        out.push(Finding::new("system:reboot", Cat::System, Sev::Info, "Reboot required", format!("running {}, installed {}", l.first().unwrap_or(&""), l.get(1).unwrap_or(&""))).advice("A newer kernel is installed; reboot (migrate guests first) to use it."));
    }
}

fn cert_checks(text: &str, th: &HealthCfg, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let mut ok = true;
    for l in text.lines() {
        let Some((path, date)) = l.split_once('|') else { continue };
        // "Nov 14 10:00:00 2027 GMT"
        let Ok(t) = chrono::NaiveDateTime::parse_from_str(date.trim().trim_end_matches(" GMT"), "%b %e %H:%M:%S %Y") else { continue };
        let days = (t.and_utc().timestamp() - now) as f64 / 86400.0;
        let sev = if days <= th.cert_days_crit { Sev::Crit } else if days <= th.cert_days_warn { Sev::Warn } else { continue };
        ok = false;
        out.push(Finding::new(format!("cert:{path}"), Cat::System, sev, t!("Certificate {}", path), if days < 0.0 { "expired".to_string() } else { format!("expires in {days:.0} days") }).advice("Renew it: System → Certificates (or ACME).").jump(0, "Certificates"));
    }
    if ok && !text.trim().is_empty() {
        passed.push((Cat::System, "certificates".into()));
    }
}

fn zfs_checks(list: &str, status: &str, th: &HealthCfg, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    if list.trim().is_empty() {
        return;
    }
    let mut ok = true;
    let now = chrono::Local::now().naive_local();
    // per pool status block
    let mut blocks: HashMap<String, String> = HashMap::new();
    let mut cur = String::new();
    for l in status.lines() {
        if let Some(p) = l.trim().strip_prefix("pool: ") {
            cur = p.trim().to_string();
        }
        if !cur.is_empty() {
            blocks.entry(cur.clone()).or_default().push_str(&format!("{l}\n"));
        }
    }
    for l in list.lines() {
        let w: Vec<&str> = l.split('\t').collect();
        if w.len() < 5 {
            continue;
        }
        let (name, health, cap) = (w[0], w[3], pct(w[4]));
        let b = blocks.get(name).cloned().unwrap_or_default();
        if health != "ONLINE" {
            ok = false;
            out.push(Finding::new(format!("zfs:health:{name}"), Cat::Zfs, if health == "DEGRADED" { Sev::Warn } else { Sev::Crit }, t!("ZFS pool {}", name), health).advice("A device of the pool failed: replace it (zpool replace) and let it resilver."));
        }
        // vdev lines with READ WRITE CKSUM counters
        let mut errs = vec![];
        for vl in b.lines() {
            let v: Vec<&str> = vl.split_whitespace().collect();
            if v.len() >= 5 && v[2..5].iter().all(|x| x.parse::<u64>().is_ok()) && v[2..5].iter().any(|x| *x != "0") {
                errs.push(format!("{} read {} write {} cksum {}", v[0], v[2], v[3], v[4]));
            }
        }
        if !errs.is_empty() {
            ok = false;
            let mut f = Finding::new(format!("zfs:errors:{name}"), Cat::Zfs, Sev::Crit, t!("ZFS pool {} has device errors", name), format!("{} device(s)", errs.len())).advice("Check the disks (SMART, cables); after fixing, clear the counters with zpool clear and scrub.");
            for e in errs {
                f = f.detail(e);
            }
            out.push(f);
        }
        if cap >= th.zfs_cap_warn {
            ok = false;
            out.push(Finding::new(format!("zfs:cap:{name}"), Cat::Zfs, if cap >= th.zfs_cap_crit { Sev::Crit } else { Sev::Warn }, t!("ZFS pool {} usage", name), format!("{cap:.0}% used")).advice("ZFS slows down when it gets full: free space or add disks."));
        }
        // "scan: scrub repaired 0B in 00:10:12 with 0 errors on Sun Nov  9 00:34:13 2026"
        if let Some(sl) = b.lines().find(|l| l.trim_start().starts_with("scan:")) {
            if let Some(d) = sl.split(" on ").last() {
                if let Ok(t) = chrono::NaiveDateTime::parse_from_str(&d.split_whitespace().collect::<Vec<_>>().join(" "), "%a %b %d %H:%M:%S %Y") {
                    let days = (now - t).num_days() as f64;
                    if days > th.zfs_scrub_days && sl.contains("scrub") {
                        ok = false;
                        out.push(Finding::new(format!("zfs:scrub:{name}"), Cat::Zfs, Sev::Info, t!("ZFS pool {} not scrubbed", name), format!("last scrub {days:.0} days ago")).advice("Run a scrub (Disks → ZFS, s) to find silent errors."));
                    }
                }
            }
        } else if !b.is_empty() {
            out.push(Finding::new(format!("zfs:scrub:{name}"), Cat::Zfs, Sev::Info, t!("ZFS pool {} never scrubbed", name), "no scrub").advice("Run a scrub (Disks → ZFS, s)."));
        }
    }
    if ok {
        passed.push((Cat::Zfs, "pools".into()));
    }
}

fn lvm_checks(text: &str, th: &HealthCfg, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let mut ok = true;
    let mut any = false;
    for l in text.lines() {
        let f: Vec<&str> = l.split('|').map(|x| x.trim()).collect();
        if f.len() < 5 || !f[2].starts_with('t') {
            continue; // thin pools only
        }
        any = true;
        for (what, v) in [("data", pct(f[3])), ("metadata", pct(f[4]))] {
            let sev = if v >= th.lvm_thin_crit { Sev::Crit } else if v >= th.lvm_thin_warn { Sev::Warn } else { continue };
            ok = false;
            out.push(
                Finding::new(format!("lvm:{}:{}:{what}", f[0], f[1]), Cat::Lvm, sev, t!("LVM thin pool {}/{} {}", f[0], f[1], what), format!("{v:.0}% used"))
                    .advice(if what == "metadata" { "A full metadata area stops every guest on the pool: extend it (lvextend --poolmetadatasize)." } else { "Extend the pool or free space; a full thin pool stops writing guests." }),
            );
        }
    }
    if any && ok {
        passed.push((Cat::Lvm, "thin pools".into()));
    }
}

fn ceph_checks(text: &str, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let Some(v) = text.find('{').and_then(|i| serde_json::from_str::<Value>(&text[i..]).ok()) else { return };
    let status = js(&v, "status");
    let checks = v.get("checks").and_then(|c| c.as_object()).cloned().unwrap_or_default();
    for (name, c) in &checks {
        let sev = if js(c, "severity") == "HEALTH_ERR" { Sev::Crit } else { Sev::Warn };
        let summary = c.get("summary").map(|s| js(s, "message")).unwrap_or_default();
        let mut f = Finding::new(format!("ceph:{name}"), Cat::Ceph, sev, t!("Ceph {}", name), summary).advice("See ceph health detail; Disks → Ceph shows the OSDs.");
        for d in c.get("detail").map(jarr).unwrap_or_default().iter().take(8) {
            f = f.detail(js(d, "message"));
        }
        out.push(f);
    }
    if checks.is_empty() && status == "HEALTH_OK" {
        passed.push((Cat::Ceph, "HEALTH_OK".into()));
    }
}

fn corosync_checks(text: &str, out: &mut Vec<Finding>) {
    let bad: Vec<&str> = text.lines().filter(|l| l.contains("disconnected") || l.contains("FAULTY")).map(|l| l.trim()).collect();
    if !bad.is_empty() {
        let mut f = Finding::new("corosync:links", Cat::Cluster, Sev::Warn, "Corosync links down", format!("{} link(s)", bad.len())).advice("A cluster network link is down: check that network; with all links down the node loses quorum.");
        for b in bad.iter().take(8) {
            f = f.detail(*b);
        }
        out.push(f);
    }
}

// ---------------------------------------------------------------------------
// Slow tier: SMART, BMC, ethtool
// ---------------------------------------------------------------------------

fn slow_checks(s: &[(String, String)], th: &HealthCfg, st: &mut HostState, window: Duration, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let mut disks = 0;
    let mut disk_ok = true;
    let mut seen: Vec<String> = vec![];
    for (head, body) in s {
        if let Some(rest) = head.strip_prefix("smart ") {
            let Some(v) = body.find('{').and_then(|i| serde_json::from_str::<Value>(&body[i..]).ok()) else { continue };
            // the same disk can show up twice (JBOD as /dev/sdX and behind the controller)
            let serial = js(&v, "serial_number");
            if !serial.is_empty() {
                if seen.contains(&serial) {
                    continue;
                }
                seen.push(serial.clone());
            }
            disks += 1;
            disk_ok &= smart_checks(rest, &v, th, st, window, out);
        }
    }
    if disks > 0 && disk_ok {
        passed.push((Cat::Disk, t!("SMART of {} disk(s)", disks)));
    }
    let sdr = sec(s, "sdr");
    if !sdr.is_empty() {
        sdr_checks(sdr, out, passed);
    }
    let sel = sec(s, "sel");
    if !sel.is_empty() {
        sel_checks(sel, sec(s, "selinfo"), window, out, passed);
    }
    // NIC slower than it can do
    for (head, body) in s {
        if let Some(nic) = head.strip_prefix("ethtool ") {
            let max = body.split(|c: char| !c.is_ascii_alphanumeric()).filter_map(|w| w.find("base").and_then(|i| w[..i].parse::<u64>().ok())).max().unwrap_or(0);
            st.counters.entry(format!("nicmax:{nic}")).or_default().last = max as f64;
        }
    }
}

/// SMART of one device; returns true when nothing was found.
fn smart_checks(dev: &str, v: &Value, th: &HealthCfg, st: &mut HostState, window: Duration, out: &mut Vec<Finding>) -> bool {
    let model = js(v, "model_name");
    let serial = js(v, "serial_number");
    let name = format!("{} {}", dev.split_whitespace().next().unwrap_or(dev), model).trim().to_string();
    let id = if serial.is_empty() { dev.to_string() } else { serial.clone() };
    let mut ok = true;
    let mut push = |f: Finding| {
        ok = false;
        out.push(f.detail(format!("{model}  SN {serial}")).jump(2, "Physical"));
    };
    if let Some(p) = v.get("smart_status").and_then(|s| s.get("passed")).and_then(|p| p.as_bool()) {
        if !p {
            push(Finding::new(format!("smart:fail:{id}"), Cat::Disk, Sev::Crit, t!("SMART failed: {}", name), "the drive predicts its failure").advice("Replace the drive now; copy data away first."));
        }
    }
    // ATA attributes
    let attr = |want: u64| -> Option<f64> {
        v.get("ata_smart_attributes").and_then(|a| a.get("table")).map(jarr).unwrap_or_default().into_iter().find(|a| a.get("id").and_then(|x| x.as_u64()) == Some(want)).and_then(|a| a.get("raw").and_then(|r| r.get("value")).and_then(|x| x.as_f64()))
    };
    if let Some(r) = attr(5).filter(|r| *r > 0.0) {
        push(Finding::new(format!("smart:realloc:{id}"), Cat::Disk, Sev::Warn, t!("Reallocated sectors: {}", name), format!("{r:.0}")).advice("The drive remapped bad sectors; watch whether the number grows and plan a replacement."));
    }
    if let Some(r) = attr(197).filter(|r| *r > 0.0) {
        push(Finding::new(format!("smart:pending:{id}"), Cat::Disk, Sev::Warn, t!("Pending sectors: {}", name), format!("{r:.0}")).advice("Unreadable sectors waiting for a rewrite: run a scrub / long test, replace the drive if they stay."));
    }
    if let Some(r) = attr(198).filter(|r| *r > 0.0) {
        push(Finding::new(format!("smart:uncorr:{id}"), Cat::Disk, Sev::Crit, t!("Uncorrectable sectors: {}", name), format!("{r:.0}")).advice("Data on these sectors is lost; replace the drive."));
    }
    if let Some(r) = attr(199) {
        let g = st.grow(&format!("smart:crc:{id}"), r, window);
        if g > 0.0 {
            push(Finding::new(format!("smart:crc:{id}"), Cat::Disk, Sev::Warn, t!("Interface CRC errors growing: {}", name), format!("+{g:.0} (total {r:.0})")).advice("Bad cable, backplane slot or connector: reseat or replace the cable."));
        }
    }
    // SCSI / SAS
    if let Some(g) = v.get("scsi_grown_defect_list").and_then(|x| x.as_f64()).filter(|x| *x > 0.0) {
        push(Finding::new(format!("smart:grown:{id}"), Cat::Disk, Sev::Warn, t!("Grown defects: {}", name), format!("{g:.0}")).advice("The drive remapped defective blocks; plan a replacement if the number grows."));
    }
    if let Some(log) = v.get("scsi_error_counter_log") {
        let unc: f64 = ["read", "write", "verify"].iter().map(|k| log.get(*k).and_then(|x| x.get("total_uncorrected_errors")).and_then(|x| x.as_f64()).unwrap_or(0.0)).sum();
        if unc > 0.0 {
            push(Finding::new(format!("smart:scsiunc:{id}"), Cat::Disk, Sev::Warn, t!("Uncorrected errors: {}", name), format!("{unc:.0}")).advice("The drive could not correct some reads / writes: check it and plan a replacement."));
        }
    }
    // NVMe
    if let Some(n) = v.get("nvme_smart_health_information_log") {
        let cw = jf(n, "critical_warning");
        if cw > 0.0 {
            push(Finding::new(format!("nvme:cw:{id}"), Cat::Disk, Sev::Crit, t!("NVMe critical warning: {}", name), format!("0x{:x}", cw as u64)).advice("Spare low, temperature, reliability or read-only: replace the drive."));
        }
        let media = jf(n, "media_errors");
        if media > 0.0 {
            push(Finding::new(format!("nvme:media:{id}"), Cat::Disk, Sev::Warn, t!("NVMe media errors: {}", name), format!("{media:.0}")).advice("The flash had unrecoverable errors; plan a replacement."));
        }
        let used = jf(n, "percentage_used");
        if used >= th.nvme_wear_warn {
            push(Finding::new(format!("nvme:wear:{id}"), Cat::Disk, if used >= 100.0 { Sev::Crit } else { Sev::Warn }, t!("NVMe worn out: {}", name), format!("{used:.0}% of rated endurance")).advice("The drive reached its rated write endurance; replace it."));
        }
    }
    // temperature
    if let Some(t) = v.get("temperature").and_then(|t| t.get("current")).and_then(|t| t.as_f64()) {
        let nvme = v.get("nvme_smart_health_information_log").is_some();
        let (w, c) = if nvme { (th.nvme_temp_warn, th.nvme_temp_crit) } else { (th.disk_temp_warn, th.disk_temp_crit) };
        if t >= w {
            push(Finding::new(format!("smart:temp:{id}"), Cat::Temp, if t >= c { Sev::Crit } else { Sev::Warn }, t!("Disk temperature: {}", name), format!("{t:.0} °C")).advice("Check the fans and air flow of the drive bays."));
        }
    }
    ok
}

fn sdr_checks(text: &str, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let sensors = crate::bmc::parse_sdr(text);
    let mut ok = true;
    for x in &sensors {
        let sev = x.sev();
        if sev == Sev::Ok {
            continue;
        }
        ok = false;
        let cat = if x.name.to_lowercase().contains("temp") { Cat::Temp } else { Cat::Power };
        out.push(Finding::new(format!("bmc:sdr:{}", x.name), cat, sev, t!("BMC sensor {}", x.name), format!("{} ({})", x.reading, x.status)).advice("The BMC reports this sensor outside its normal range: check fans, power supplies and cooling.").jump(0, "BMC"));
    }
    if !sensors.is_empty() && ok {
        passed.push((Cat::Power, t!("{} BMC sensors", sensors.len())));
    }
}

fn sel_checks(text: &str, info: &str, window: Duration, out: &mut Vec<Finding>, passed: &mut Vec<(Cat, Title)>) {
    let entries = crate::bmc::parse_sel(text);
    // events inside the report window (by the BMC clock)
    let since = chrono::Local::now().naive_local() - chrono::Duration::seconds(window.as_secs() as i64);
    let mut new: Vec<&crate::bmc::SelEntry> = entries.iter().filter(|e| e.at.map(|t| t > since).unwrap_or(false) && e.sev() >= Sev::Warn).collect();
    new.sort_by_key(|e| std::cmp::Reverse(e.id));
    let mut ok = true;
    if let Some(worst) = new.iter().map(|e| e.sev()).max() {
        ok = false;
        let mut f = Finding::new("bmc:sel", Cat::Power, worst, "BMC event log", format!("{} event(s)", new.len())).advice("System → BMC shows the full event log.").jump(0, "BMC");
        for e in new.iter().take(10) {
            f = f.detail(format!("{}  {} | {} | {}", e.when(), e.sensor, e.event, e.state));
        }
        out.push(f);
    }
    let info = crate::bmc::kv(info);
    if let Some(used) = crate::bmc::sel_used(&info).filter(|u| *u >= 90.0) {
        ok = false;
        out.push(
            Finding::new("bmc:sel:full", Cat::Power, Sev::Warn, "BMC event log almost full", format!("{used:.0}% used"))
                .advice("When the log is full the BMC stops recording (or overwrites) events. Export and clear it on System → BMC.")
                .jump(0, "BMC"),
        );
    }
    if ok {
        passed.push((Cat::Power, t!("BMC event log ({} entries)", entries.len())));
    }
}

// ---------------------------------------------------------------------------
// PVE API checks
// ---------------------------------------------------------------------------

fn api_checks(api: &Api, node: &str, th: &HealthCfg, rep: &mut Report) {
    let f = &mut rep.findings;
    // cluster
    if let Ok(v) = api.get("/cluster/status") {
        let list = jarr(&v);
        let mut ok = true;
        for e in &list {
            match js(e, "type").as_str() {
                "cluster" if jf(e, "quorate") == 0.0 => {
                    ok = false;
                    f.push(Finding::new("cluster:quorum", Cat::Cluster, Sev::Crit, t!("Cluster {} has no quorum", js(e, "name")), "not quorate").advice("/etc/pve is read-only. Bring the other nodes / links back; only in an emergency set pvecm expected to the number of online nodes."));
                }
                "node" if jf(e, "online") == 0.0 => {
                    ok = false;
                    f.push(Finding::new(format!("cluster:node:{}", js(e, "name")), Cat::Cluster, Sev::Warn, t!("Node {} offline", js(e, "name")), "offline"));
                }
                _ => {}
            }
        }
        if ok && list.len() > 1 {
            rep.passed.push((Cat::Cluster, "quorum and nodes".into()));
        }
    }
    // storages of this node
    if let Ok(v) = api.get(&format!("/nodes/{node}/storage")) {
        let mut ok = true;
        for s in jarr(&v) {
            let id = js(&s, "storage");
            if jb(&s, "enabled") && !jb(&s, "active") {
                ok = false;
                f.push(Finding::new(format!("storage:inactive:{id}"), Cat::Storage, Sev::Crit, t!("Storage {} not available", id), "inactive").advice("The storage cannot be reached (NFS / iSCSI / Ceph / mount); guests on it cannot start.").jump(4, "Storage"));
            }
            let frac = jf(&s, "used_fraction") * 100.0;
            if jb(&s, "active") && frac >= th.storage_warn {
                ok = false;
                f.push(Finding::new(format!("storage:full:{id}"), Cat::Storage, if frac >= th.storage_crit { Sev::Crit } else { Sev::Warn }, t!("Storage {} usage", id), format!("{frac:.0}% used")).advice("Free space or extend the storage.").jump(4, "Storage"));
            }
        }
        if ok {
            rep.passed.push((Cat::Storage, "PVE storages".into()));
        }
    }
    // HA errors
    if let Ok(v) = api.get("/cluster/ha/status/current") {
        for e in jarr(&v) {
            let st = js(&e, "state");
            if js(&e, "type") == "service" && (st == "error" || st == "fence" || st == "recovery") {
                f.push(Finding::new(format!("ha:{}", js(&e, "sid")), Cat::Guests, Sev::Warn, t!("HA resource {}", js(&e, "sid")), st).jump(4, "HA"));
            }
        }
    }
    // failed backups in the last day
    let since = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() - 86400).unwrap_or(0);
    if let Ok(v) = api.get(&format!("/nodes/{node}/tasks?typefilter=vzdump&errors=1&since={since}&limit=50")) {
        let failed = jarr(&v);
        if !failed.is_empty() {
            let mut x = Finding::new("backup:failed", Cat::Guests, Sev::Warn, "Backups failed (24 h)", format!("{} task(s)", failed.len())).advice("Open the task log (F8) for the reason.").jump(4, "Backup");
            for t in failed.iter().take(6) {
                x = x.detail(format!("{} {}: {}", crate::util::fmt_time(js(t, "starttime").parse().unwrap_or(0)), js(t, "id"), js(t, "status")));
            }
            f.push(x);
        } else {
            rep.passed.push((Cat::Guests, "backups (24 h)".into()));
        }
    }
    // locked guests on this node
    if let Ok(v) = api.get("/cluster/resources?type=vm") {
        for g in jarr(&v).into_iter().filter(|g| js(g, "node") == node && !js(g, "lock").is_empty()) {
            f.push(Finding::new(format!("guest:lock:{}", js(&g, "id")), Cat::Guests, Sev::Info, t!("Guest {} locked", js(&g, "vmid")), js(&g, "lock")).advice("A lock left behind by an interrupted task blocks actions; unlock it in PVE → Guests (U) if no task is running.").jump(4, "Guests"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn th() -> HealthCfg {
        HealthCfg::default()
    }

    #[test]
    fn counters_report_only_growth() {
        let mut st = HostState::default();
        let w = Duration::from_secs(3600);
        assert_eq!(st.grow("x", 5.0, w), 0.0, "first sample is the baseline");
        assert_eq!(st.grow("x", 5.0, w), 0.0);
        assert_eq!(st.grow("x", 8.0, w), 3.0);
        assert_eq!(st.grow("x", 8.0, w), 3.0, "growth stays visible inside the window");
        assert_eq!(st.grow("x", 1.0, w), 0.0, "reset after reboot");
    }

    #[test]
    fn filesystems_and_lvm() {
        let df = "Filesystem Type 1024-blocks Used Available Capacity Mounted on\n/dev/mapper/pve-root ext4 100 92 8 92% /\n/dev/sdb1 ext4 100 99 1 99% /mnt/data\n";
        let (mut f, mut p) = (vec![], vec![]);
        fs_checks(df, "", &th(), &mut f, &mut p);
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].sev, f[0].value.as_str()), (Sev::Warn, "92% used"));
        let (mut f, mut p) = (vec![], vec![]);
        lvm_checks("  pve|data|twi-aotz--|50.00|96.10\n  pve|root|-wi-ao----||\n", &th(), &mut f, &mut p);
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].sev, f[0].title.en().as_str()), (Sev::Crit, "LVM thin pool pve/data metadata"));
    }

    #[test]
    fn pcie_links_skip_integrated_devices() {
        let mut st = HostState::default();
        let w = Duration::from_secs(3600);
        let link = "0000:00:04.0|0x088000|0|255|Unknown|Unknown\n0000:3b:00.0|0x020000|8|16|8.0 GT/s PCIe|8.0 GT/s PCIe\n0000:00:01.0|0x060400|4|16|x|x\n";
        let aer = "0000:3b:00.0 correctable 3\n0000:3b:00.0 nonfatal 0\n0000:3b:00.0 fatal 1\n";
        let (mut f, mut p) = (vec![], vec![]);
        pcie_checks(aer, link, &HashMap::new(), &mut st, w, &mut f, &mut p);
        let titles: Vec<String> = f.iter().map(|x| x.title.en()).collect();
        assert_eq!(titles, vec!["PCIe fatal errors 0000:3b:00.0", "PCIe link narrower than possible 0000:3b:00.0"]);
        // corrected errors only once they grow
        let (mut f, mut p) = (vec![], vec![]);
        pcie_checks("0000:3b:00.0 correctable 9\n", "", &HashMap::new(), &mut st, w, &mut f, &mut p);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].value, "+6 (total 9)");
    }

    #[test]
    fn kernel_log_patterns() {
        let k = "1763200000.123 node1 kernel: INFO: task jbd2/sda1-8:433 blocked for more than 120 seconds.\n1763200100.000 node1 kernel: blk_update_request: I/O error, dev sdc, sector 123\n1763200200.000 node1 kernel: I/O error, dev sdc, sector 456\n";
        let (mut f, mut p) = (vec![], vec![]);
        kernel_checks(k, "1763100000.0 node1 kernel: Error: Driver 'pcspkr' is already registered, aborting...\n", &mut f, &mut p);
        let io = f.iter().find(|x| x.id == "kern:io").unwrap();
        assert_eq!((io.sev, io.value.starts_with("2 since boot")), (Sev::Crit, true));
        assert!(f.iter().any(|x| x.id == "kern:hung" && x.sev == Sev::Warn));
        assert!(f.iter().any(|x| x.id == "kern:other" && x.sev == Sev::Info));
    }

    #[test]
    fn ceph_zfs_time_and_smart() {
        let ceph = r#"{"status":"HEALTH_WARN","checks":{"BLUESTORE_SLOW_OP_ALERT":{"severity":"HEALTH_WARN","summary":{"message":"3 OSD(s) experiencing slow operations in BlueStore"},"detail":[{"message":"osd.4 observed slow operation indications in BlueStore"}]}}}"#;
        let (mut f, mut p) = (vec![], vec![]);
        ceph_checks(ceph, &mut f, &mut p);
        assert_eq!((f.len(), f[0].sev, f[0].detail.len()), (1, Sev::Warn, 1));
        let zl = "rpool\t100\t50\tDEGRADED\t85\t10\n";
        let zs = "  pool: rpool\n state: DEGRADED\n  scan: scrub repaired 0B in 00:10:12 with 0 errors on Sun Jan  4 00:34:13 2026\nconfig:\n\tNAME        STATE     READ WRITE CKSUM\n\trpool       DEGRADED     0     0     0\n\t  mirror-0  DEGRADED     0     0     0\n\t    sda2    ONLINE       0     0     3\n";
        let (mut f, mut p) = (vec![], vec![]);
        zfs_checks(zl, zs, &th(), &mut f, &mut p);
        let ids: Vec<&str> = f.iter().map(|x| x.id.as_str()).collect();
        assert!(ids.contains(&"zfs:health:rpool") && ids.contains(&"zfs:errors:rpool") && ids.contains(&"zfs:cap:rpool") && ids.contains(&"zfs:scrub:rpool"), "{ids:?}");
        let (mut f, mut p) = (vec![], vec![]);
        time_checks("System time     : 2.500000 seconds fast of NTP time\nLeap status     : Normal\n", &th(), &mut f, &mut p);
        assert_eq!(f[0].sev, Sev::Warn);
        let smart: Value = serde_json::from_str(r#"{"model_name":"ST4000","serial_number":"Z1","smart_status":{"passed":true},"ata_smart_attributes":{"table":[{"id":5,"raw":{"value":8}},{"id":199,"raw":{"value":2}}]},"temperature":{"current":58}}"#).unwrap();
        let mut st = HostState::default();
        let mut f = vec![];
        smart_checks("/dev/sda sat", &smart, &th(), &mut st, Duration::from_secs(60), &mut f);
        let ids: Vec<&str> = f.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["smart:realloc:Z1", "smart:temp:Z1"], "CRC counter starts as baseline");
    }

    #[test]
    fn bmc_sensors_and_events() {
        let sdr = "CPU1 Temp        | 30h | ok  |  3.1 | 45 degrees C\nFAN1             | 41h | cr  | 29.1 | 0 RPM\nPS2 Status       | 60h | nc  | 10.2 | Power Supply AC lost\n";
        let (mut f, mut p) = (vec![], vec![]);
        sdr_checks(sdr, &mut f, &mut p);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].sev, Sev::Crit);
        let now = chrono::Local::now().format("%m/%d/%Y | %H:%M:%S").to_string();
        let sel = format!("   1 | 01/02/2020 | 10:00:00 | Memory #0x01 | Uncorrectable ECC | Asserted\n   2 | {now} | Memory #0x01 | Correctable ECC | Asserted\n   3 | {now} | Power Supply #0x02 | Power Supply AC lost | Asserted\n");
        let day = Duration::from_secs(86400);
        let (mut f, mut p) = (vec![], vec![]);
        sel_checks(&sel, "Percent Used : 95%\n", day, &mut f, &mut p);
        assert_eq!((f[0].sev, f[0].detail.len()), (Sev::Crit, 2), "old events are not reported");
        assert_eq!(f[1].id, "bmc:sel:full");
    }
}
