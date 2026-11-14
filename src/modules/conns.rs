//! Network → Connections: TCP / UDP sockets of the node from `ss -tunapi` (process, ports, peer,
//! per-connection rates from the kernel byte counters, RTT, retransmissions). A connection can
//! be closed (`ss -K`, needs CONFIG_INET_DIAG_DESTROY) or its process terminated.

use std::collections::HashMap;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    Frame,
};

use super::{scroll, View};
use crate::ctx::Ctx;
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, status_style, style_dim, style_warn, Kv, ACCENT};
use crate::util::{fmt_bytes, fmt_rate};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sock {
    pub proto: String,
    pub state: String,
    pub recv_q: u64,
    pub send_q: u64,
    pub local: String,
    pub peer: String,
    /// (name, pid) of the owning processes
    pub procs: Vec<(String, u32)>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub bytes_retrans: u64,
    /// smoothed RTT (ms)
    pub rtt: f64,
    /// raw `ss -i` line
    pub info: String,
}

impl Sock {
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.proto, self.local, self.peer)
    }

    pub fn listening(&self) -> bool {
        self.state == "LISTEN" || self.state == "UNCONN"
    }

    pub fn process(&self) -> String {
        self.procs.first().map(|(n, p)| format!("{n} ({p})")).unwrap_or_else(|| "-".into())
    }
}

/// Split "addr:port" ("[::1]:22", "10.0.0.1%eth0:53", "*:*").
pub fn split_addr(a: &str) -> (String, String) {
    match a.rsplit_once(':') {
        Some((h, p)) => (h.trim_start_matches('[').trim_end_matches(']').split('%').next().unwrap_or("").to_string(), p.to_string()),
        None => (a.to_string(), String::new()),
    }
}

fn procs_of(s: &str) -> Vec<(String, u32)> {
    // users:(("sshd",pid=123,fd=4),("x",pid=5,fd=6))
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("(\"") {
        rest = &rest[i + 2..];
        let Some(j) = rest.find('"') else { break };
        let name = rest[..j].to_string();
        let pid = rest[j..].split("pid=").nth(1).and_then(|p| p.split([',', ')']).next()).and_then(|p| p.parse().ok()).unwrap_or(0);
        if !out.iter().any(|(_, p)| *p == pid) {
            out.push((name, pid));
        }
        rest = &rest[j..];
    }
    out
}

fn num_after(info: &str, key: &str) -> u64 {
    info.split_whitespace().find_map(|w| w.strip_prefix(key)).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Parse `ss -tunapiH` (socket line + optional tab-indented info line).
pub fn parse_ss(text: &str) -> Vec<Sock> {
    let mut out: Vec<Sock> = Vec::new();
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            if let Some(s) = out.last_mut() {
                let info = line.trim().to_string();
                s.bytes_sent = num_after(&info, "bytes_sent:");
                s.bytes_received = num_after(&info, "bytes_received:");
                s.bytes_retrans = num_after(&info, "bytes_retrans:");
                s.rtt = info.split_whitespace().find_map(|w| w.strip_prefix("rtt:")).and_then(|v| v.split('/').next()).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                s.info = info;
            }
            continue;
        }
        let w: Vec<&str> = line.split_whitespace().collect();
        if w.len() < 6 {
            continue;
        }
        out.push(Sock {
            proto: w[0].to_string(),
            state: w[1].to_string(),
            recv_q: w[2].parse().unwrap_or(0),
            send_q: w[3].parse().unwrap_or(0),
            local: w[4].to_string(),
            peer: w[5].to_string(),
            procs: procs_of(&w[6..].join(" ")),
            ..Default::default()
        });
    }
    out
}

const FILTERS: [&str; 3] = ["established", "listening", "all"];
const SORTS: [&str; 3] = ["traffic", "process", "peer"];

#[derive(Default)]
pub struct Conns {
    data: Loader<Vec<Sock>>,
    prev: HashMap<String, (Instant, u64, u64)>,
    /// key -> (recv/s, send/s)
    rates: HashMap<String, (f64, f64)>,
    table: Table,
    scroll: u16,
    filter: usize,
    sort: usize,
}

impl Conns {
    fn list(&self) -> Vec<Sock> {
        let mut v: Vec<Sock> = self
            .data
            .data
            .iter()
            .flatten()
            .filter(|s| match self.filter {
                0 => !s.listening(),
                1 => s.listening(),
                _ => true,
            })
            .cloned()
            .collect();
        let rate = |s: &Sock| self.rates.get(&s.key()).map(|r| r.0 + r.1).unwrap_or(0.0);
        match self.sort {
            1 => v.sort_by(|a, b| a.process().cmp(&b.process()).then(a.local.cmp(&b.local))),
            2 => v.sort_by(|a, b| a.peer.cmp(&b.peer)),
            _ => v.sort_by(|a, b| rate(b).partial_cmp(&rate(a)).unwrap_or(std::cmp::Ordering::Equal).then((b.bytes_sent + b.bytes_received).cmp(&(a.bytes_sent + a.bytes_received)))),
        }
        v
    }

    fn sel(&self) -> Option<Sock> {
        let k = self.table.selected()?;
        self.data.data.as_ref()?.iter().find(|s| s.key() == k).cloned()
    }
}

impl View for Conns {
    fn title(&self) -> &'static str {
        "Connections"
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else {
            self.data.error = Some(tr("needs local or ssh access to the node").to_string());
            return;
        };
        self.data.load(move || Ok(parse_ss(&exec.output("ss", &["-tunapiH"])?)));
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        if !self.data.poll() {
            return false;
        }
        let now = Instant::now();
        let mut next = HashMap::new();
        self.rates.clear();
        for s in self.data.data.iter().flatten() {
            let k = s.key();
            if let Some((t, rx, tx)) = self.prev.get(&k) {
                let dt = now.duration_since(*t).as_secs_f64().max(0.1);
                self.rates.insert(k.clone(), (s.bytes_received.saturating_sub(*rx) as f64 / dt, s.bytes_sent.saturating_sub(*tx) as f64 / dt));
            }
            next.insert(k, (now, s.bytes_received, s.bytes_sent));
        }
        self.prev = next;
        true
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(2)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("f", "show"), ("o", "sort"), ("p", "this process")]
    }

    fn on_key(&mut self, key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        match key.code {
            KeyCode::Char('f') => self.filter = (self.filter + 1) % FILTERS.len(),
            KeyCode::Char('o') => self.sort = (self.sort + 1) % SORTS.len(),
            _ => return false,
        }
        true
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let list = self.list();
        let rows: Vec<Row> = list
            .iter()
            .map(|s| {
                let (rx, tx) = self.rates.get(&s.key()).copied().unwrap_or_default();
                let active = rx + tx > 0.0;
                Row::new(s.key())
                    .cs(s.proto.clone(), style_dim())
                    .cs(s.state.clone(), status_style(if s.state == "ESTAB" || s.listening() { "ok" } else { "warn" }))
                    .c(s.local.clone())
                    .c(s.peer.clone())
                    .cs(s.process(), Style::default().fg(ACCENT))
                    .c(if rx > 0.0 { fmt_rate(rx) } else { String::new() })
                    .c(if tx > 0.0 { fmt_rate(tx) } else { String::new() })
                    .cs(if s.rtt > 0.0 { super::diskio::ms(s.rtt) } else { String::new() }, super::diskio::lat_style(s.rtt))
                    .c(if s.bytes_sent + s.bytes_received > 0 { fmt_bytes((s.bytes_sent + s.bytes_received) as f64) } else { String::new() })
                    .dim(!active && !s.listening() && s.state != "ESTAB")
            })
            .collect();
        let cols = [
            col("Proto", Constraint::Length(5)),
            col("State", Constraint::Length(10)),
            col("Local", Constraint::Length(24)),
            col("Peer", Constraint::Length(24)),
            col("Process", Constraint::Min(16)),
            col("Recv", Constraint::Length(9)),
            col("Send", Constraint::Length(9)),
            col("RTT", Constraint::Length(7)),
            col("Total", Constraint::Length(8)),
        ];
        let title = format!("{} [{} · {} {}]", tr("Connections"), tr(FILTERS[self.filter]), tr("sort"), tr(SORTS[self.sort]));
        self.table.draw(f, area, &title, &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(s) = self.sel() {
            let (rx, tx) = self.rates.get(&s.key()).copied().unwrap_or_default();
            kv.add("Protocol", format!("{}  {}", s.proto, s.state));
            kv.add("Local", s.local.clone());
            kv.add("Peer", s.peer.clone());
            for (n, p) in &s.procs {
                kv.add("Process", format!("{n}  pid {p}"));
            }
            kv.add("Queues", format!("recv {}  send {}", s.recv_q, s.send_q));
            if !s.info.is_empty() {
                kv.section("Traffic");
                kv.add("Receive", format!("{}  ({} {})", fmt_rate(rx), tr("total"), fmt_bytes(s.bytes_received as f64)));
                kv.add("Send", format!("{}  ({} {})", fmt_rate(tx), tr("total"), fmt_bytes(s.bytes_sent as f64)));
                kv.add("RTT", super::diskio::ms(s.rtt));
                if s.bytes_retrans > 0 {
                    kv.styled("Retransmitted", fmt_bytes(s.bytes_retrans as f64), style_warn());
                }
                kv.section("TCP");
                for w in s.info.split_whitespace() {
                    if let Some((k, v)) = w.split_once(':') {
                        kv.add(k, v.to_string());
                    }
                }
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let Some(s) = self.sel() else { return vec![] };
        let pid = s.procs.first().map(|p| p.1).unwrap_or(0);
        let root = ctx.need_root();
        vec![
            item('p', "Only this process").disabled_if(s.procs.is_empty(), "no process"),
            item('x', "Disconnect").require(root.clone()).disabled_if(s.proto != "tcp" || s.listening(), "only established TCP connections"),
            item('K', "Terminate process (SIGTERM)").require(root).disabled_if(pid <= 1, "no process"),
        ]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(s) = self.sel() else { return };
        match key {
            'p' => {
                if let Some((n, _)) = s.procs.first() {
                    self.table.filter = n.to_lowercase();
                    self.filter = 2;
                }
            }
            'x' => ctx.popup(Popup::confirm(
                "Disconnect",
                &format!("kill:{}", s.key()),
                vec![format!("{}  {} → {}", s.process(), s.local, s.peer), tr("The program sees a reset connection.").to_string()],
                true,
            )),
            'K' => {
                if let Some((n, p)) = s.procs.first() {
                    ctx.popup(Popup::confirm_typed("Terminate process", &format!("term:{p}"), vec![format!("kill -TERM {p}  ({n})")], n));
                }
            }
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else { return };
        if let Some(k) = tag.strip_prefix("kill:") {
            let Some(s) = self.data.data.iter().flatten().find(|s| s.key() == k).cloned() else { return };
            let (lh, lp) = split_addr(&s.local);
            let (ph, pp) = split_addr(&s.peer);
            let args: Vec<String> = ["-K", "dst", &ph, "dport", "=", &pp, "src", &lh, "sport", "=", &lp].iter().map(|x| x.to_string()).collect();
            ctx.audit(&format!("ss {}", args.join(" ")));
            ctx.exec_call(format!("Disconnect {} → {}", s.local, s.peer), exec, "ss", args);
        } else if let Some(p) = tag.strip_prefix("term:") {
            ctx.audit(&format!("kill -TERM {p}"));
            ctx.exec_call(format!("kill -TERM {p}"), exec, "kill", vec!["-TERM".into(), p.to_string()]);
        }
    }

    fn reset(&mut self) {
        self.data.clear();
        self.prev.clear();
        self.rates.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SS: &str = "tcp ESTAB 0 0 192.168.88.11:45334 192.168.88.13:6816 users:((\"ceph-osd\",pid=4157,fd=104))\n\t cubic wscale:7,7 rto:211 rtt:10.424/15.844 ato:40 mss:1448 bytes_sent:3234302435689 bytes_retrans:3088070154 bytes_acked:3231214365536 bytes_received:3866600161 segs_out:2267957787\n\
tcp LISTEN 0 4096 [::]:22 [::]:* users:((\"sshd\",pid=900,fd=4),(\"systemd\",pid=1,fd=50))\n\
udp UNCONN 0 0 0.0.0.0:68 0.0.0.0:* users:((\"dhclient\",pid=6020,fd=7))\n";

    #[test]
    fn ss_lines() {
        let v = parse_ss(SS);
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].proto.as_str(), v[0].state.as_str(), v[0].peer.as_str()), ("tcp", "ESTAB", "192.168.88.13:6816"));
        assert_eq!(v[0].procs, vec![("ceph-osd".to_string(), 4157)]);
        assert_eq!((v[0].bytes_sent, v[0].bytes_received, v[0].rtt), (3234302435689, 3866600161, 10.424));
        assert_eq!(v[1].procs.len(), 2);
        assert!(v[1].listening() && v[2].listening());
        assert_eq!(split_addr("[::1]:22"), ("::1".to_string(), "22".to_string()));
        assert_eq!(split_addr("10.0.0.1%eth0:53"), ("10.0.0.1".to_string(), "53".to_string()));
    }
}
