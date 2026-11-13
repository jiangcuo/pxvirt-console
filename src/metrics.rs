//! Live metrics for the bottom status bar: sampled from /proc for the local node,
//! from the PVE API for other nodes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::api::Api;
use crate::util::{jarr, jf};

pub const HISTORY: usize = 120;

#[derive(Clone, Default)]
pub struct IfaceStat {
    pub name: String,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_pkts: u64,
    pub tx_pkts: u64,
    pub rx_errs: u64,
    pub tx_errs: u64,
    pub rx_drop: u64,
    pub tx_drop: u64,
    pub physical: bool,
    pub rx_hist: Vec<f64>,
    pub tx_hist: Vec<f64>,
}

#[derive(Clone, Default)]
pub struct MetricsData {
    pub source: String,
    pub ok: bool,
    pub cpu: f64,
    pub cpus: u32,
    pub iowait: f64,
    pub load: [f64; 3],
    pub mem_used: f64,
    pub mem_total: f64,
    pub swap_used: f64,
    pub swap_total: f64,
    pub arc_size: f64,
    pub net_rx: f64,
    pub net_tx: f64,
    pub disk_r: f64,
    pub disk_w: f64,
    pub disk_riops: f64,
    pub disk_wiops: f64,
    pub procs: u64,
    pub threads: u64,
    pub uptime: u64,
    pub cpu_hist: Vec<f64>,
    pub mem_hist: Vec<f64>,
    pub net_hist: Vec<f64>,
    pub disk_hist: Vec<f64>,
    pub ifaces: Vec<IfaceStat>,
    /// Network/disk numbers are coarse (API rrd minute averages).
    pub coarse: bool,
}

fn push(hist: &mut Vec<f64>, v: f64) {
    hist.push(v);
    if hist.len() > HISTORY {
        let n = hist.len() - HISTORY;
        hist.drain(..n);
    }
}

#[derive(Clone)]
pub enum Target {
    Local,
    /// /proc streamed over ssh (SSH mode)
    Exec(crate::exec::Exec),
    Api(Arc<Api>, String),
    None,
}

pub struct Metrics {
    pub data: Arc<Mutex<MetricsData>>,
    gen: Arc<AtomicU64>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self { data: Arc::new(Mutex::new(MetricsData::default())), gen: Arc::new(AtomicU64::new(0)) }
    }

    pub fn snapshot(&self) -> MetricsData {
        self.data.lock().unwrap().clone()
    }

    pub fn start(&self, target: Target) {
        let my_gen = self.gen.fetch_add(1, Ordering::SeqCst) + 1;
        *self.data.lock().unwrap() = MetricsData::default();
        let data = self.data.clone();
        let gen = self.gen.clone();
        match target {
            Target::None => {}
            Target::Local => {
                thread::spawn(move || {
                    let mut s = LocalSampler::default();
                    while gen.load(Ordering::SeqCst) == my_gen {
                        s.sample(&data);
                        thread::sleep(Duration::from_secs(1));
                    }
                });
            }
            Target::Exec(exec) => {
                thread::spawn(move || {
                    let Ok(stream) = exec.stream("sh", &["-c", REMOTE_SAMPLER]) else { return };
                    let mut s = LocalSampler::default();
                    let mut frame: HashMap<String, String> = HashMap::new();
                    let mut cur = String::new();
                    while gen.load(Ordering::SeqCst) == my_gen {
                        let Ok(line) = stream.rx.recv_timeout(Duration::from_secs(5)) else {
                            data.lock().unwrap().ok = false;
                            continue;
                        };
                        if let Some(name) = line.strip_prefix("@@F ") {
                            cur = name.to_string();
                            frame.insert(cur.clone(), String::new());
                        } else if line == "@@END" {
                            s.sample_from(&data, &Src::Frame(&frame));
                            data.lock().unwrap().source = "ssh".into();
                            frame.clear();
                        } else if let Some(v) = frame.get_mut(&cur) {
                            v.push_str(&line);
                            v.push('\n');
                        }
                    }
                });
            }
            Target::Api(api, node) => {
                thread::spawn(move || {
                    let mut last_rrd: Option<Instant> = None;
                    while gen.load(Ordering::SeqCst) == my_gen {
                        sample_api(&api, &node, &data, &mut last_rrd);
                        thread::sleep(Duration::from_secs(3));
                    }
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// local /proc sampling
// ---------------------------------------------------------------------------

#[derive(Default)]
struct LocalSampler {
    cpu_prev: Option<(u64, u64, u64)>, // total, idle, iowait
    net_prev: HashMap<String, (u64, u64)>,
    disk_prev: Option<(u64, u64, u64, u64)>,
    at: Option<Instant>,
}

/// Where the sampler gets its /proc data: this machine, or a frame streamed over ssh.
enum Src<'a> {
    Local,
    Frame(&'a HashMap<String, String>),
}

impl Src<'_> {
    fn read(&self, path: &str) -> String {
        match self {
            Src::Local => read(path),
            Src::Frame(m) => m.get(path).cloned().unwrap_or_default(),
        }
    }
    fn physical_net(&self, name: &str) -> bool {
        match self {
            Src::Local => std::path::Path::new(&format!("/sys/class/net/{name}/device")).exists(),
            Src::Frame(m) => m.get("netphys").map(|v| v.split_whitespace().any(|n| n == name)).unwrap_or(false),
        }
    }
    fn whole_disk(&self, name: &str) -> bool {
        match self {
            Src::Local => std::path::Path::new(&format!("/sys/block/{name}/device")).exists(),
            Src::Frame(m) => m.get("blockdev").map(|v| v.split_whitespace().any(|n| n == name)).unwrap_or(false),
        }
    }
    fn procs(&self) -> u64 {
        match self {
            Src::Local => std::fs::read_dir("/proc")
                .map(|d| d.filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())).count() as u64)
                .unwrap_or(0),
            Src::Frame(m) => m.get("procs").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
        }
    }
}

/// Script streaming one frame of /proc data per second (SSH mode).
const REMOTE_SAMPLER: &str = "P=$(ls -d /sys/class/net/*/device 2>/dev/null | cut -d/ -f5 | tr '\\n' ' '); \
B=$(ls -d /sys/block/*/device 2>/dev/null | cut -d/ -f4 | tr '\\n' ' '); \
while :; do echo '@@F netphys'; echo \"$P\"; echo '@@F blockdev'; echo \"$B\"; \
for f in /proc/stat /proc/loadavg /proc/meminfo /proc/net/dev /proc/diskstats /proc/uptime; do echo \"@@F $f\"; cat $f; done; \
echo '@@F /proc/spl/kstat/zfs/arcstats'; grep '^size ' /proc/spl/kstat/zfs/arcstats 2>/dev/null; \
echo '@@F procs'; ls /proc | grep -c '^[0-9]'; echo '@@END'; sleep 1; done";

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

impl LocalSampler {
    fn sample(&mut self, data: &Arc<Mutex<MetricsData>>) {
        self.sample_from(data, &Src::Local);
    }

    fn sample_from(&mut self, data: &Arc<Mutex<MetricsData>>, src: &Src) {
        let now = Instant::now();
        let dt = self.at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        self.at = Some(now);

        // CPU
        let stat = src.read("/proc/stat");
        let mut cpu = 0.0;
        let mut iowait = 0.0;
        let mut cpus = 0;
        for line in stat.lines() {
            if line.starts_with("cpu ") {
                let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
                if v.len() >= 5 {
                    let total: u64 = v.iter().take(8).sum();
                    let idle = v[3] + v[4];
                    if let Some((pt, pi, pw)) = self.cpu_prev {
                        let dtot = total.saturating_sub(pt) as f64;
                        if dtot > 0.0 {
                            cpu = 1.0 - idle.saturating_sub(pi) as f64 / dtot;
                            iowait = v[4].saturating_sub(pw) as f64 / dtot;
                        }
                    }
                    self.cpu_prev = Some((total, idle, v[4]));
                }
            } else if line.starts_with("cpu") {
                cpus += 1;
            }
        }

        // load, threads
        let la = src.read("/proc/loadavg");
        let parts: Vec<&str> = la.split_whitespace().collect();
        let mut load = [0.0; 3];
        for (i, l) in load.iter_mut().enumerate() {
            *l = parts.get(i).and_then(|x| x.parse().ok()).unwrap_or(0.0);
        }
        let threads = parts
            .get(3)
            .and_then(|x| x.split('/').nth(1))
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        let procs = src.procs();

        // memory
        let mut mi: HashMap<&str, f64> = HashMap::new();
        let meminfo = src.read("/proc/meminfo");
        for line in meminfo.lines() {
            if let Some((k, v)) = line.split_once(':') {
                let n: f64 = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
                mi.insert(k, n * 1024.0);
            }
        }
        let mem_total = *mi.get("MemTotal").unwrap_or(&0.0);
        let mem_used = mem_total - *mi.get("MemAvailable").unwrap_or(&0.0);
        let swap_total = *mi.get("SwapTotal").unwrap_or(&0.0);
        let swap_used = swap_total - *mi.get("SwapFree").unwrap_or(&0.0);
        let arc_size = src.read("/proc/spl/kstat/zfs/arcstats")
            .lines()
            .find(|l| l.starts_with("size "))
            .and_then(|l| l.split_whitespace().nth(2))
            .and_then(|x| x.parse().ok())
            .unwrap_or(0.0);

        // network
        let mut ifaces = Vec::new();
        let (mut rx_sum, mut tx_sum) = (0.0, 0.0);
        let old_ifaces: HashMap<String, IfaceStat> = data
            .lock()
            .unwrap()
            .ifaces
            .iter()
            .map(|i| (i.name.clone(), i.clone()))
            .collect();
        for line in src.read("/proc/net/dev").lines().skip(2) {
            let Some((name, rest)) = line.split_once(':') else { continue };
            let name = name.trim().to_string();
            let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() < 16 {
                continue;
            }
            let physical = src.physical_net(&name);
            let (rx_bps, tx_bps) = match self.net_prev.get(&name) {
                Some((prx, ptx)) if dt > 0.0 => (
                    v[0].saturating_sub(*prx) as f64 / dt,
                    v[8].saturating_sub(*ptx) as f64 / dt,
                ),
                _ => (0.0, 0.0),
            };
            self.net_prev.insert(name.clone(), (v[0], v[8]));
            if physical {
                rx_sum += rx_bps;
                tx_sum += tx_bps;
            }
            let mut st = IfaceStat {
                name: name.clone(),
                rx_bps,
                tx_bps,
                rx_bytes: v[0],
                tx_bytes: v[8],
                rx_pkts: v[1],
                tx_pkts: v[9],
                rx_errs: v[2],
                tx_errs: v[10],
                rx_drop: v[3],
                tx_drop: v[11],
                physical,
                ..Default::default()
            };
            if let Some(o) = old_ifaces.get(&name) {
                st.rx_hist = o.rx_hist.clone();
                st.tx_hist = o.tx_hist.clone();
            }
            push(&mut st.rx_hist, rx_bps);
            push(&mut st.tx_hist, tx_bps);
            ifaces.push(st);
        }
        ifaces.sort_by(|a, b| b.physical.cmp(&a.physical).then(a.name.cmp(&b.name)));

        // disks (whole devices only)
        let (mut rs, mut ws, mut ri, mut wi) = (0u64, 0u64, 0u64, 0u64);
        for line in src.read("/proc/diskstats").lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 14 {
                continue;
            }
            let name = f[2];
            if !src.whole_disk(name) {
                continue;
            }
            let p = |i: usize| f[i].parse::<u64>().unwrap_or(0);
            ri += p(3);
            rs += p(5);
            wi += p(7);
            ws += p(9);
        }
        let (mut disk_r, mut disk_w, mut riops, mut wiops) = (0.0, 0.0, 0.0, 0.0);
        if let Some((prs, pws, pri, pwi)) = self.disk_prev {
            if dt > 0.0 {
                disk_r = rs.saturating_sub(prs) as f64 * 512.0 / dt;
                disk_w = ws.saturating_sub(pws) as f64 * 512.0 / dt;
                riops = ri.saturating_sub(pri) as f64 / dt;
                wiops = wi.saturating_sub(pwi) as f64 / dt;
            }
        }
        self.disk_prev = Some((rs, ws, ri, wi));

        let uptime = src.read("/proc/uptime")
            .split_whitespace()
            .next()
            .and_then(|x| x.parse::<f64>().ok())
            .unwrap_or(0.0) as u64;

        let mut d = data.lock().unwrap();
        d.source = "local".into();
        d.ok = true;
        d.cpu = cpu;
        d.cpus = cpus;
        d.iowait = iowait;
        d.load = load;
        d.mem_used = mem_used;
        d.mem_total = mem_total;
        d.swap_used = swap_used;
        d.swap_total = swap_total;
        d.arc_size = arc_size;
        d.net_rx = rx_sum;
        d.net_tx = tx_sum;
        d.disk_r = disk_r;
        d.disk_w = disk_w;
        d.disk_riops = riops;
        d.disk_wiops = wiops;
        d.procs = procs;
        d.threads = threads;
        d.uptime = uptime;
        d.ifaces = ifaces;
        d.coarse = false;
        push(&mut d.cpu_hist, cpu);
        push(&mut d.mem_hist, if mem_total > 0.0 { mem_used / mem_total } else { 0.0 });
        push(&mut d.net_hist, rx_sum + tx_sum);
        push(&mut d.disk_hist, disk_r + disk_w);
    }
}

// ---------------------------------------------------------------------------
// API sampling (other nodes, remote mode)
// ---------------------------------------------------------------------------

fn sample_api(api: &Api, node: &str, data: &Arc<Mutex<MetricsData>>, last_rrd: &mut Option<Instant>) {
    let status = api.get(&format!("/nodes/{node}/status"));
    let rrd = if last_rrd.map(|t| t.elapsed() > Duration::from_secs(30)).unwrap_or(true) {
        *last_rrd = Some(Instant::now());
        api.get(&format!("/nodes/{node}/rrddata?timeframe=hour&cf=AVERAGE")).ok()
    } else {
        None
    };
    let mut d = data.lock().unwrap();
    d.source = format!("api:{node}");
    d.coarse = true;
    match status {
        Ok(s) => {
            d.ok = true;
            d.cpu = jf(&s, "cpu");
            d.iowait = jf(&s, "wait");
            d.cpus = s.get("cpuinfo").map(|c| jf(c, "cpus") as u32).unwrap_or(0);
            let la = s.get("loadavg").map(jarr).unwrap_or_default();
            for i in 0..3 {
                d.load[i] = la.get(i).and_then(|v| v.as_str()).and_then(|x| x.parse().ok()).unwrap_or(0.0);
            }
            if let Some(m) = s.get("memory") {
                d.mem_used = jf(m, "used");
                d.mem_total = jf(m, "total");
            }
            if let Some(m) = s.get("swap") {
                d.swap_used = jf(m, "used");
                d.swap_total = jf(m, "total");
            }
            d.uptime = jf(&s, "uptime") as u64;
            let cpu = d.cpu;
            push(&mut d.cpu_hist, cpu);
            let mf = if d.mem_total > 0.0 { d.mem_used / d.mem_total } else { 0.0 };
            push(&mut d.mem_hist, mf);
        }
        Err(_) => d.ok = false,
    }
    if let Some(r) = rrd {
        let rows = jarr(&r);
        if let Some(last) = rows.iter().rev().find(|e| e.get("netin").is_some()) {
            d.net_rx = jf(last, "netin");
            d.net_tx = jf(last, "netout");
            d.disk_r = jf(last, "diskread");
            d.disk_w = jf(last, "diskwrite");
        }
        d.net_hist = rows.iter().map(|e| jf(e, "netin") + jf(e, "netout")).collect();
        d.disk_hist = rows.iter().map(|e| jf(e, "diskread") + jf(e, "diskwrite")).collect();
    }
}
