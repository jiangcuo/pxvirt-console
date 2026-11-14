//! Disks → Network storage: NFS mounts (from /proc/self/mountstats: rates, IOPS, RTT,
//! timeouts), iSCSI sessions, Fibre Channel HBAs and remote ports, NVMe over Fabrics
//! controllers and multipath, read from sysfs of the node.

use std::collections::HashMap;
use std::time::Instant;

use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    Frame,
};

use super::{scroll, View};
use crate::ctx::Ctx;
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, status_style, style_bad, style_dim, style_warn, Kv, ACCENT};
use crate::util::{fmt_bytes, fmt_rate, jarr, jf, js};

const SCRIPT: &str = r#"
echo '@@mountstats'; cat /proc/self/mountstats 2>/dev/null
echo '@@iscsi'
for s in /sys/class/iscsi_session/session*; do [ -d "$s" ] || continue; n=${s##*/}
  echo "S|$n|$(cat $s/targetname 2>/dev/null)|$(cat $s/state 2>/dev/null)|$(cat $s/recovery_tmo 2>/dev/null)|$(cat $s/initiatorname 2>/dev/null)"
  for c in /sys/class/iscsi_connection/connection${n#session}:*; do [ -d "$c" ] && echo "C|$n|$(cat $c/persistent_address 2>/dev/null):$(cat $c/persistent_port 2>/dev/null)|$(cat $c/state 2>/dev/null)"; done
  for b in $s/device/target*/*/block/*; do [ -e "$b" ] && echo "D|$n|${b##*/}"; done
done
echo '@@fc'
for h in /sys/class/fc_host/host*; do [ -d "$h" ] || continue; n=${h##*/}
  echo "H|$n|$(cat $h/port_name)|$(cat $h/node_name)|$(cat $h/port_state)|$(cat $h/speed)|$(cat $h/port_type)|$(cat $h/fabric_name 2>/dev/null)|$(cat $h/supported_speeds 2>/dev/null)"
  for st in tx_words rx_words tx_frames rx_frames link_failure_count loss_of_sync_count loss_of_signal_count error_frames invalid_crc_count; do echo "T|$n|$st|$(cat $h/statistics/$st 2>/dev/null)"; done
done
for r in /sys/class/fc_remote_ports/rport-*; do [ -d "$r" ] || continue; echo "R|${r##*/}|$(cat $r/port_name)|$(cat $r/port_state)|$(cat $r/roles)"; done
echo '@@nvmeof'
for c in /sys/class/nvme/nvme*; do [ -d "$c" ] || continue; t=$(cat $c/transport 2>/dev/null); [ "$t" = pcie ] && continue
  ns=$(ls -d $c/nvme*n* 2>/dev/null | sed 's|.*/||' | tr '\n' ' ')
  echo "N|${c##*/}|$t|$(cat $c/subsysnqn 2>/dev/null)|$(cat $c/address 2>/dev/null | tr ',' ' ')|$(cat $c/state 2>/dev/null)|$(cat $c/model 2>/dev/null)|$ns"
done
echo '@@multipath'; command -v multipath >/dev/null 2>&1 && multipath -ll 2>/dev/null | head -n 200
true
"#;

#[derive(Clone, Debug, Default)]
pub struct Nfs {
    pub device: String,
    pub mount: String,
    pub fstype: String,
    pub opts: HashMap<String, String>,
    /// normal + direct read / write bytes (application side)
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// per op: ops, transmissions, timeouts, rtt ms, execute ms
    pub ops: HashMap<String, [u64; 5]>,
    pub xprt: String,
}

#[derive(Clone, Debug, Default)]
pub struct Iscsi {
    pub session: String,
    pub target: String,
    pub state: String,
    pub recovery_tmo: String,
    pub initiator: String,
    pub portals: Vec<(String, String)>,
    pub disks: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct FcHost {
    pub host: String,
    pub wwpn: String,
    pub wwnn: String,
    pub state: String,
    pub speed: String,
    pub port_type: String,
    pub fabric: String,
    pub supported: String,
    pub stats: HashMap<String, u64>,
}

#[derive(Clone, Debug, Default)]
pub struct NvmeOf {
    pub ctrl: String,
    pub transport: String,
    pub nqn: String,
    pub address: String,
    pub state: String,
    pub model: String,
    pub namespaces: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub nfs: Vec<Nfs>,
    pub iscsi: Vec<Iscsi>,
    pub fc: Vec<FcHost>,
    /// (rport, wwpn, state, roles)
    pub rports: Vec<(String, String, String, String)>,
    pub nvmeof: Vec<NvmeOf>,
    pub multipath: String,
    /// PVE storages: mount point -> (storage id, active)
    pub pve: HashMap<String, (String, bool)>,
}

fn hexnum(s: &str) -> u64 {
    let t = s.trim();
    t.strip_prefix("0x").map(|h| u64::from_str_radix(h, 16).unwrap_or(0)).unwrap_or_else(|| t.parse().unwrap_or(0))
}

/// NFS mounts of /proc/self/mountstats.
pub fn parse_mountstats(text: &str) -> Vec<Nfs> {
    let mut out: Vec<Nfs> = Vec::new();
    let mut cur: Option<Nfs> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("device ") {
            if let Some(n) = cur.take() {
                out.push(n);
            }
            // device srv:/export mounted on /mnt with fstype nfs4 statvers=1.1
            let w: Vec<&str> = rest.split_whitespace().collect();
            let fstype = w.iter().position(|x| *x == "fstype").and_then(|i| w.get(i + 1)).unwrap_or(&"").to_string();
            if fstype.starts_with("nfs") {
                let on = w.iter().position(|x| *x == "on").map(|i| i + 1).unwrap_or(0);
                let fi = w.iter().position(|x| *x == "with").unwrap_or(w.len());
                cur = Some(Nfs { device: w.first().unwrap_or(&"").to_string(), mount: w[on.min(fi)..fi].join(" "), fstype, ..Default::default() });
            }
            continue;
        }
        let Some(n) = cur.as_mut() else { continue };
        let t = line.trim();
        if let Some(o) = t.strip_prefix("opts:") {
            for kv in o.trim().split(',') {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                n.opts.insert(k.to_string(), v.to_string());
            }
        } else if let Some(b) = t.strip_prefix("bytes:") {
            let v: Vec<u64> = b.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() >= 4 {
                n.read_bytes = v[0] + v[2];
                n.write_bytes = v[1] + v[3];
            }
        } else if let Some(x) = t.strip_prefix("xprt:") {
            n.xprt = x.trim().to_string();
        } else if let Some((op, rest)) = t.split_once(':') {
            if op.chars().all(|c| c.is_ascii_uppercase() || c == '_') && !op.is_empty() {
                let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if v.len() >= 8 {
                    n.ops.insert(op.to_string(), [v[0], v[1], v[2], v[6], v[7]]);
                }
            }
        }
    }
    if let Some(n) = cur {
        out.push(n);
    }
    out
}

pub fn parse(text: &str) -> Snapshot {
    let mut snap = Snapshot::default();
    let mut sec = "";
    let mut ms = String::new();
    let mut mp = String::new();
    for line in text.lines() {
        if let Some(s) = line.strip_prefix("@@") {
            sec = match s.trim() {
                "mountstats" => "mountstats",
                "iscsi" => "iscsi",
                "fc" => "fc",
                "nvmeof" => "nvmeof",
                "multipath" => "multipath",
                _ => "",
            };
            continue;
        }
        let p: Vec<&str> = line.split('|').collect();
        match sec {
            "mountstats" => {
                ms.push_str(line);
                ms.push('\n');
            }
            "multipath" => {
                mp.push_str(line);
                mp.push('\n');
            }
            "iscsi" => match p.first() {
                Some(&"S") if p.len() >= 6 => snap.iscsi.push(Iscsi {
                    session: p[1].into(),
                    target: p[2].into(),
                    state: p[3].into(),
                    recovery_tmo: p[4].into(),
                    initiator: p[5].into(),
                    ..Default::default()
                }),
                Some(&"C") if p.len() >= 4 => {
                    if let Some(s) = snap.iscsi.iter_mut().find(|s| s.session == p[1]) {
                        s.portals.push((p[2].into(), p[3].into()));
                    }
                }
                Some(&"D") if p.len() >= 3 => {
                    if let Some(s) = snap.iscsi.iter_mut().find(|s| s.session == p[1]) {
                        s.disks.push(p[2].into());
                    }
                }
                _ => {}
            },
            "fc" => match p.first() {
                Some(&"H") if p.len() >= 9 => snap.fc.push(FcHost {
                    host: p[1].into(),
                    wwpn: p[2].into(),
                    wwnn: p[3].into(),
                    state: p[4].into(),
                    speed: p[5].into(),
                    port_type: p[6].into(),
                    fabric: p[7].into(),
                    supported: p[8].into(),
                    stats: HashMap::new(),
                }),
                Some(&"T") if p.len() >= 4 => {
                    if let Some(h) = snap.fc.iter_mut().find(|h| h.host == p[1]) {
                        h.stats.insert(p[2].into(), hexnum(p[3]));
                    }
                }
                Some(&"R") if p.len() >= 5 => snap.rports.push((p[1].into(), p[2].into(), p[3].into(), p[4].into())),
                _ => {}
            },
            "nvmeof" => {
                if p.first() == Some(&"N") && p.len() >= 8 {
                    snap.nvmeof.push(NvmeOf {
                        ctrl: p[1].into(),
                        transport: p[2].into(),
                        nqn: p[3].into(),
                        address: p[4].into(),
                        state: p[5].into(),
                        model: p[6].trim().into(),
                        namespaces: p[7].split_whitespace().map(String::from).collect(),
                    });
                }
            }
            _ => {}
        }
    }
    snap.nfs = parse_mountstats(&ms);
    snap.multipath = mp.trim().to_string();
    snap
}

/// Rates of an NFS mount between two snapshots.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NfsRate {
    pub read_bps: f64,
    pub write_bps: f64,
    pub read_iops: f64,
    pub write_iops: f64,
    /// average round trip / total execution time of READ and WRITE (ms)
    pub rtt: f64,
    pub exec: f64,
    pub timeouts: u64,
    pub retrans: u64,
}

pub fn nfs_rate(prev: &Nfs, cur: &Nfs, dt: f64) -> NfsRate {
    let d = |a: u64, b: u64| b.saturating_sub(a) as f64;
    let op = |k: &str, i: usize| (prev.ops.get(k).map(|v| v[i]).unwrap_or(0), cur.ops.get(k).map(|v| v[i]).unwrap_or(0));
    let mut r = NfsRate::default();
    if dt > 0.0 {
        r.read_bps = d(prev.read_bytes, cur.read_bytes) / dt;
        r.write_bps = d(prev.write_bytes, cur.write_bytes) / dt;
        r.read_iops = { let (a, b) = op("READ", 0); d(a, b) / dt };
        r.write_iops = { let (a, b) = op("WRITE", 0); d(a, b) / dt };
    }
    let mut ops = 0.0;
    let (mut rtt, mut exec) = (0.0, 0.0);
    for k in ["READ", "WRITE"] {
        ops += { let (a, b) = op(k, 0); d(a, b) };
        rtt += { let (a, b) = op(k, 3); d(a, b) };
        exec += { let (a, b) = op(k, 4); d(a, b) };
    }
    if ops > 0.0 {
        r.rtt = rtt / ops;
        r.exec = exec / ops;
    }
    // totals since mount
    for v in cur.ops.values() {
        r.timeouts += v[2];
        r.retrans += v[1].saturating_sub(v[0]);
    }
    r
}

#[derive(Default)]
pub struct NetStorage {
    data: Loader<Snapshot>,
    prev: Option<(Instant, Snapshot)>,
    cur_at: Option<Instant>,
    rates: HashMap<String, NfsRate>,
    fc_rates: HashMap<String, (f64, f64)>,
    table: Table,
    scroll: u16,
}

impl NetStorage {
    fn snap(&self) -> Option<&Snapshot> {
        self.data.data.as_ref()
    }
}

impl View for NetStorage {
    fn title(&self) -> &'static str {
        "Network storage"
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else {
            self.data.error = Some(tr("needs local or ssh access to the node").to_string());
            return;
        };
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        self.data.load(move || {
            let mut s = parse(&exec.output_any("sh", &["-c", SCRIPT])?.1);
            if let Ok(v) = api.get(&format!("/nodes/{node}/storage")) {
                for st in jarr(&v) {
                    let id = js(&st, "storage");
                    s.pve.insert(format!("/mnt/pve/{id}"), (id, jf(&st, "active") != 0.0));
                }
            }
            Ok(s)
        });
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        if !self.data.poll() {
            return false;
        }
        let now = Instant::now();
        if let (Some(cur), Some((t, prev))) = (self.data.data.as_ref(), self.prev.as_ref()) {
            let dt = now.duration_since(*t).as_secs_f64();
            self.rates = cur.nfs.iter().filter_map(|n| prev.nfs.iter().find(|p| p.mount == n.mount).map(|p| (n.mount.clone(), nfs_rate(p, n, dt)))).collect();
            self.fc_rates = cur
                .fc
                .iter()
                .filter_map(|h| {
                    let p = prev.fc.iter().find(|p| p.host == h.host)?;
                    let d = |k: &str| h.stats.get(k).unwrap_or(&0).saturating_sub(*p.stats.get(k).unwrap_or(&0)) as f64 * 4.0 / dt.max(0.1);
                    Some((h.host.clone(), (d("rx_words"), d("tx_words"))))
                })
                .collect();
        }
        self.cur_at = Some(now);
        if let Some(cur) = self.data.data.clone() {
            self.prev = Some((now, cur));
        }
        true
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(3)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let m = ctx.metrics.lock().unwrap().clone();
        let head = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
        let mut rows = Vec::new();
        let section = |rows: &mut Vec<Row>, key: &str, title: &str, n: usize| {
            rows.push(Row::new(format!("h|{key}")).cs(format!("{} · {n}", tr(title)), head).c("").c("").c("").c("").c("").c("").c(""));
        };
        if let Some(s) = self.snap() {
            section(&mut rows, "nfs", "NFS", s.nfs.len());
            for (i, n) in s.nfs.iter().enumerate() {
                let (pve, active) = s.pve.get(&n.mount).cloned().map(|(id, a)| (id, Some(a))).unwrap_or((String::new(), None));
                let r = self.rates.get(&n.mount).copied().unwrap_or_default();
                let state = match active {
                    Some(true) => "active",
                    Some(false) => "inactive",
                    None => "mounted",
                };
                rows.push(
                    Row::new(format!("n|{i}"))
                        .c(format!("  {}", if pve.is_empty() { n.mount.clone() } else { pve }))
                        .c(format!("{} v{}", n.fstype, n.opts.get("vers").cloned().unwrap_or_default()))
                        .c(n.device.clone())
                        .cs(state, status_style(state))
                        .c(fmt_rate(r.read_bps))
                        .c(fmt_rate(r.write_bps))
                        .c(format!("{:.0}", r.read_iops + r.write_iops))
                        .cs(super::diskio::ms(r.rtt), super::diskio::lat_style(r.rtt)),
                );
            }
            section(&mut rows, "iscsi", "iSCSI", s.iscsi.len());
            for (i, x) in s.iscsi.iter().enumerate() {
                let (rd, wr, lat) = x.disks.iter().filter_map(|d| m.disk(d)).fold((0.0, 0.0, 0.0f64), |a, d| (a.0 + d.r_bps, a.1 + d.w_bps, a.2.max(d.r_await.max(d.w_await))));
                rows.push(
                    Row::new(format!("i|{i}"))
                        .c(format!("  {}", x.target))
                        .c(x.session.clone())
                        .c(x.portals.iter().map(|p| p.0.clone()).collect::<Vec<_>>().join(" "))
                        .cs(x.state.clone(), status_style(if x.state == "LOGGED_IN" { "ok" } else { "error" }))
                        .c(fmt_rate(rd))
                        .c(fmt_rate(wr))
                        .c(x.disks.join(" "))
                        .cs(super::diskio::ms(lat), super::diskio::lat_style(lat)),
                );
            }
            section(&mut rows, "fc", "Fibre Channel", s.fc.len());
            for (i, h) in s.fc.iter().enumerate() {
                let (rx, tx) = self.fc_rates.get(&h.host).copied().unwrap_or_default();
                let errs: u64 = ["link_failure_count", "loss_of_sync_count", "loss_of_signal_count", "error_frames", "invalid_crc_count"].iter().map(|k| *h.stats.get(*k).unwrap_or(&0)).sum();
                rows.push(
                    Row::new(format!("f|{i}"))
                        .c(format!("  {} {}", h.host, h.wwpn))
                        .c(h.speed.clone())
                        .c(h.fabric.clone())
                        .cs(h.state.clone(), status_style(if h.state == "Online" { "ok" } else { "error" }))
                        .c(fmt_rate(rx))
                        .c(fmt_rate(tx))
                        .c(String::new())
                        .cs(if errs > 0 { format!("{errs} err") } else { String::new() }, style_warn()),
                );
            }
            section(&mut rows, "nvmeof", "NVMe over Fabrics", s.nvmeof.len());
            for (i, c) in s.nvmeof.iter().enumerate() {
                let (rd, wr, lat) = c.namespaces.iter().filter_map(|d| m.disk(d)).fold((0.0, 0.0, 0.0f64), |a, d| (a.0 + d.r_bps, a.1 + d.w_bps, a.2.max(d.r_await.max(d.w_await))));
                rows.push(
                    Row::new(format!("v|{i}"))
                        .c(format!("  {} {}", c.ctrl, c.nqn))
                        .c(c.transport.clone())
                        .c(c.address.clone())
                        .cs(c.state.clone(), status_style(if c.state == "live" { "ok" } else { "error" }))
                        .c(fmt_rate(rd))
                        .c(fmt_rate(wr))
                        .c(c.namespaces.join(" "))
                        .cs(super::diskio::ms(lat), super::diskio::lat_style(lat)),
                );
            }
            if !s.multipath.is_empty() {
                section(&mut rows, "mp", "Multipath", s.multipath.lines().filter(|l| l.contains(" dm-")).count());
            }
        }
        let cols = [
            col("Name", Constraint::Min(24)),
            col("Type", Constraint::Length(10)),
            col("Address", Constraint::Length(22)),
            col("State", Constraint::Length(10)),
            col("Read", Constraint::Length(9)),
            col("Write", Constraint::Length(9)),
            col("IOPS / disks", Constraint::Length(12)),
            col("Latency", Constraint::Length(8)),
        ];
        self.table.draw(f, area, "Network storage", &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        let key = self.table.selected().unwrap_or("").to_string();
        let (kind, idx) = key.split_once('|').map(|(a, b)| (a.to_string(), b.parse::<usize>().ok())).unwrap_or_default();
        if let Some(s) = self.snap() {
            match (kind.as_str(), idx) {
                ("n", Some(i)) => {
                    if let Some(n) = s.nfs.get(i) {
                        let r = self.rates.get(&n.mount).copied().unwrap_or_default();
                        kv.section("NFS");
                        kv.add("Export", n.device.clone());
                        kv.add("Mount point", n.mount.clone());
                        if let Some((id, active)) = s.pve.get(&n.mount) {
                            kv.styled("PVE storage", format!("{id} ({})", tr(if *active { "active" } else { "inactive" })), if *active { Style::default() } else { style_bad() });
                        }
                        for k in ["vers", "proto", "addr", "rsize", "wsize", "timeo", "retrans", "sec"] {
                            if let Some(v) = n.opts.get(k) {
                                kv.add(k, v.clone());
                            }
                        }
                        kv.add("mode", if n.opts.contains_key("soft") { "soft" } else { "hard" });
                        kv.section("IO");
                        kv.add("Read", format!("{}  {:.0} IOPS", fmt_rate(r.read_bps), r.read_iops));
                        kv.add("Write", format!("{}  {:.0} IOPS", fmt_rate(r.write_bps), r.write_iops));
                        kv.styled("RTT", super::diskio::ms(r.rtt), super::diskio::lat_style(r.rtt));
                        kv.styled("Execution time", super::diskio::ms(r.exec), super::diskio::lat_style(r.exec));
                        kv.add("Since mount", format!("{} {} / {} {}", tr("read"), fmt_bytes(n.read_bytes as f64), tr("written"), fmt_bytes(n.write_bytes as f64)));
                        kv.styled("Timeouts / retransmissions", format!("{} / {}", r.timeouts, r.retrans), if r.timeouts > 0 { style_warn() } else { Style::default() });
                        kv.add("Transport", n.xprt.clone());
                    }
                }
                ("i", Some(i)) => {
                    if let Some(x) = s.iscsi.get(i) {
                        kv.section("iSCSI");
                        kv.add("Target", x.target.clone());
                        kv.add("Session", x.session.clone());
                        kv.add("Initiator", x.initiator.clone());
                        kv.styled("State", x.state.clone(), status_style(if x.state == "LOGGED_IN" { "ok" } else { "error" }));
                        kv.add("Recovery timeout", format!("{} s", x.recovery_tmo));
                        for (p, st) in &x.portals {
                            kv.add("Portal", format!("{p}  {st}"));
                        }
                        let m = ctx.metrics.lock().unwrap().clone();
                        for d in &x.disks {
                            if let Some(st) = m.disk(d) {
                                kv.add(&format!("/dev/{d}"), format!("R {} W {}  {}", fmt_rate(st.r_bps), fmt_rate(st.w_bps), super::diskio::ms(st.r_await.max(st.w_await))));
                            } else {
                                kv.add(&format!("/dev/{d}"), "");
                            }
                        }
                    }
                }
                ("f", Some(i)) => {
                    if let Some(h) = s.fc.get(i) {
                        kv.section("Fibre Channel");
                        kv.add("Host", h.host.clone());
                        kv.add("WWPN", h.wwpn.clone());
                        kv.add("WWNN", h.wwnn.clone());
                        kv.styled("Port state", h.state.clone(), status_style(if h.state == "Online" { "ok" } else { "error" }));
                        kv.add("Speed", format!("{}  ({} {})", h.speed, tr("supported"), h.supported));
                        kv.add("Port type", h.port_type.clone());
                        kv.add("Fabric", h.fabric.clone());
                        kv.section("Counters");
                        let mut st: Vec<(&String, &u64)> = h.stats.iter().collect();
                        st.sort();
                        for (k, v) in st {
                            kv.styled(k, v.to_string(), if k.contains("count") || k.contains("error") || k.contains("crc") { if *v > 0 { style_warn() } else { Style::default() } } else { Style::default() });
                        }
                        let rp: Vec<_> = s.rports.iter().collect();
                        if !rp.is_empty() {
                            kv.section("Remote ports");
                            for (r, wwpn, state, roles) in rp {
                                kv.add(r, format!("{wwpn}  {state}  {roles}"));
                            }
                        }
                    }
                }
                ("v", Some(i)) => {
                    if let Some(c) = s.nvmeof.get(i) {
                        kv.section("NVMe over Fabrics");
                        kv.add("Controller", c.ctrl.clone());
                        kv.add("Transport", c.transport.clone());
                        kv.add("Address", c.address.clone());
                        kv.add("NQN", c.nqn.clone());
                        kv.add("Model", c.model.clone());
                        kv.styled("State", c.state.clone(), status_style(if c.state == "live" { "ok" } else { "error" }));
                        kv.add("Namespaces", c.namespaces.join(" "));
                    }
                }
                ("h", _) if idx.is_none() && key == "h|mp" => {
                    kv.section("Multipath");
                    for l in s.multipath.lines() {
                        kv.add("", l.to_string());
                    }
                }
                _ => {
                    kv.styled("", tr("NFS, iSCSI, Fibre Channel and NVMe-oF of this node; rates are measured between refreshes (3 s)."), style_dim());
                }
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn reset(&mut self) {
        self.data.clear();
        self.prev = None;
        self.rates.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTSTATS: &str = "device proc mounted on /proc with fstype proc
device 10.0.0.5:/export/iso mounted on /mnt/pve/nfs-iso with fstype nfs4 statvers=1.1
\topts:\trw,vers=4.2,rsize=1048576,wsize=1048576,namlen=255,acregmin=3,hard,proto=tcp,timeo=600,retrans=2,sec=sys,clientaddr=10.0.0.2,local_lock=none,addr=10.0.0.5
\tage:\t3600
\tbytes:\t1000 2000 300 400 1300 2400 0 0
\txprt:\ttcp 0 1 1 0 1 120 120 0 120 0 2 0 0
\tper-op statistics
\t        NULL: 1 1 0 44 24 0 0 0 0
\t        READ: 10 11 1 1600 1100000 2 50 60 0
\t       WRITE: 20 20 0 2400000 3200 4 80 100 0
";

    #[test]
    fn mountstats_and_rates() {
        let n = parse_mountstats(MOUNTSTATS);
        assert_eq!(n.len(), 1);
        let a = &n[0];
        assert_eq!((a.device.as_str(), a.mount.as_str(), a.fstype.as_str()), ("10.0.0.5:/export/iso", "/mnt/pve/nfs-iso", "nfs4"));
        assert_eq!(a.opts.get("vers").map(|s| s.as_str()), Some("4.2"));
        assert_eq!((a.read_bytes, a.write_bytes), (1300, 2400));
        assert_eq!(a.ops["READ"], [10, 11, 1, 50, 60]);
        let mut b = a.clone();
        b.read_bytes += 2_000_000;
        b.write_bytes += 1_000_000;
        b.ops.insert("READ".into(), [30, 31, 1, 250, 300]);
        b.ops.insert("WRITE".into(), [40, 40, 0, 280, 400]);
        let r = nfs_rate(a, &b, 2.0);
        assert_eq!((r.read_bps, r.write_bps), (1_000_000.0, 500_000.0));
        assert_eq!((r.read_iops, r.write_iops), (10.0, 10.0));
        assert_eq!(r.rtt, 10.0); // (200 + 200) ms / 40 ops
        assert_eq!((r.timeouts, r.retrans), (1, 1));
    }

    #[test]
    fn sysfs_sections() {
        let t = "@@mountstats\n@@iscsi\nS|session1|iqn.2003-01.org.linux-iscsi:target1|LOGGED_IN|120|iqn.1993-08.org.debian:01:abc\nC|session1|10.0.0.9:3260|up\nD|session1|sdc\n\
                 @@fc\nH|host5|0x21000024ff3dbe10|0x20000024ff3dbe10|Online|16 Gbit|NPort (fabric via point-to-point)|0x100000051e8a1f00|4 Gbit, 8 Gbit, 16 Gbit\nT|host5|rx_words|0x100\nT|host5|link_failure_count|0x2\nR|rport-5:0-0|0x500601693ee0a1b2|Online|FCP Target\n\
                 @@nvmeof\nN|nvme3|tcp|nqn.2014-08.org.nvmexpress:uuid:1|traddr=10.0.0.7 trsvcid=4420|live|Linux|nvme3n1 \n@@multipath\nmpatha (360014) dm-3 LIO-ORG,disk\n";
        let s = parse(t);
        assert_eq!(s.iscsi.len(), 1);
        assert_eq!((s.iscsi[0].portals.len(), s.iscsi[0].disks.clone()), (1, vec!["sdc".to_string()]));
        assert_eq!(s.fc.len(), 1);
        assert_eq!((s.fc[0].stats["rx_words"], s.fc[0].stats["link_failure_count"]), (256, 2));
        assert_eq!(s.rports.len(), 1);
        assert_eq!((s.nvmeof[0].transport.as_str(), s.nvmeof[0].namespaces.clone()), ("tcp", vec!["nvme3n1".to_string()]));
        assert!(s.multipath.contains("dm-3"));
    }
}
