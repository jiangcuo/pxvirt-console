//! System > Overview: one screen with what matters about the node — software versions,
//! system and board, CPU, memory, storage, guests, management URLs and the busiest
//! processes. The node power actions (reboot, shutdown, start/stop all guests) live here.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    Frame,
};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

use super::system::Power;
use super::{scroll, View};
use crate::ctx::Ctx;
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::popup::MenuItem;
use crate::ui::{draw_lines, loading_or_error, style_dim, style_good, style_label, usage_style, Kv, ACCENT};
use crate::util::{bar, fmt_bytes, fmt_duration, jarr, jb, jf, js};

const HOST_SCRIPT: &str = r#"export LC_ALL=C
echo '@@@OS'; . /etc/os-release 2>/dev/null; echo "$PRETTY_NAME"
echo '@@@DMI'; for f in sys_vendor product_name board_vendor board_name bios_version; do echo "$f=$(cat /sys/class/dmi/id/$f 2>/dev/null)"; done
echo '@@@DT'; [ -r /proc/device-tree/model ] && tr -d '\0' < /proc/device-tree/model; echo
echo '@@@ARC'; grep -E '^(size|c_max) ' /proc/spl/kstat/zfs/arcstats 2>/dev/null
echo '@@@USERS'; who | wc -l
echo '@@@ADDR'; ip -o -4 addr show scope global 2>/dev/null | awk '{print $2, $4}'
echo '@@@PS'; ps -eo pid=,times=,rss=,comm=,args=
true"#;

#[derive(Clone)]
struct Proc {
    pid: u32,
    cputime: f64,
    rss: f64,
    name: String,
}

#[derive(Default)]
struct Data {
    status: Value,
    /// (VMs running, VMs, CTs running, CTs)
    guests: (usize, usize, usize, usize),
    host: HashMap<String, String>,
    procs: Vec<Proc>,
    at: Option<Instant>,
}

#[derive(Default)]
pub struct Overview {
    data: Loader<Data>,
    /// CPU% per pid since the previous sample
    cpu: HashMap<u32, f64>,
    prev: HashMap<u32, f64>,
    prev_at: Option<Instant>,
    snapshot: Option<Data>,
    power: Power,
    scroll: u16,
}

fn parse_sections(out: &str) -> HashMap<String, String> {
    let mut m: HashMap<String, String> = HashMap::new();
    let mut cur = String::new();
    for l in out.lines() {
        if let Some(n) = l.strip_prefix("@@@") {
            cur = n.to_string();
            continue;
        }
        let e = m.entry(cur.clone()).or_default();
        e.push_str(l);
        e.push('\n');
    }
    m
}

fn parse_ps(s: &str) -> Vec<Proc> {
    s.lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            if p.len() < 4 {
                return None;
            }
            let args = p[4..].join(" ");
            let name = match args.split(" -id ").nth(1).and_then(|s| s.split_whitespace().next()) {
                Some(id) if args.contains("kvm") || args.contains("qemu") => format!("VM {id}"),
                _ => p[3].to_string(),
            };
            Some(Proc { pid: p[0].parse().ok()?, cputime: p[1].parse().unwrap_or(0.0), rss: p[2].parse::<f64>().unwrap_or(0.0) * 1024.0, name })
        })
        .collect()
}

impl View for Overview {
    fn title(&self) -> &'static str {
        "Overview"
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let api = ctx.api.clone();
        let node = ctx.node.clone();
        let exec = ctx.exec();
        self.data.load(move || {
            let status = api.get(&format!("/nodes/{node}/status"))?;
            let mut guests = (0, 0, 0, 0);
            if let Ok(r) = api.get("/cluster/resources?type=vm") {
                for g in jarr(&r).iter().filter(|g| js(g, "node") == node && !jb(g, "template")) {
                    let run = js(g, "status") == "running";
                    if js(g, "type") == "qemu" {
                        guests.1 += 1;
                        guests.0 += run as usize;
                    } else {
                        guests.3 += 1;
                        guests.2 += run as usize;
                    }
                }
            }
            let (host, procs) = match exec {
                Some(e) => {
                    let out = e.output("sh", &["-c", HOST_SCRIPT]).unwrap_or_default();
                    let mut h = parse_sections(&out);
                    let procs = parse_ps(&h.remove("PS").unwrap_or_default());
                    (h, procs)
                }
                None => (HashMap::new(), Vec::new()),
            };
            Ok(Data { status, guests, host, procs, at: Some(Instant::now()) })
        });
        self.power.refresh(ctx);
    }

    fn poll(&mut self, ctx: &mut Ctx) -> bool {
        let mut ch = self.power.poll(ctx);
        if self.data.poll() {
            ch = true;
            if let Some(d) = self.data.data.take() {
                // CPU% between two samples (like top)
                let at = d.at.unwrap_or_else(Instant::now);
                let dt = self.prev_at.map(|p| at.duration_since(p).as_secs_f64()).unwrap_or(0.0);
                self.cpu = d
                    .procs
                    .iter()
                    .filter_map(|p| {
                        let prev = self.prev.get(&p.pid)?;
                        (dt > 0.0).then(|| (p.pid, ((p.cputime - prev) / dt * 100.0).max(0.0)))
                    })
                    .collect();
                self.prev = d.procs.iter().map(|p| (p.pid, p.cputime)).collect();
                self.prev_at = Some(at);
                let arch = d.status.get("current-kernel").map(|k| js(k, "machine")).unwrap_or_default();
                if !arch.is_empty() {
                    ctx.node_arch.insert(ctx.node.clone(), arch);
                }
                self.snapshot = Some(d);
            }
        }
        ch
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(3)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some((m, st)) = loading_or_error(&self.data.error, self.data.loading() && self.snapshot.is_none()) {
            kv.styled("", m, st);
        }
        if let Some(d) = &self.snapshot {
            let s = &d.status;
            let sec = |n: &str| d.host.get(n).map(|x| x.trim().to_string()).unwrap_or_default();
            let dmi: HashMap<String, String> = sec("DMI").lines().filter_map(|l| l.split_once('=').map(|(a, b)| (a.to_string(), b.trim().to_string()))).collect();
            let g = |k: &str| dmi.get(k).cloned().unwrap_or_default();
            let cpu = s.get("cpuinfo").cloned().unwrap_or(Value::Null);
            let kern = s.get("current-kernel").cloned().unwrap_or(Value::Null);

            kv.section("Software");
            kv.styled("Console", format!("PXVIRT Console {}", crate::app::APP_VERSION), Style::default().fg(ACCENT));
            kv.add("PVE", js(s, "pveversion").trim_start_matches("pve-manager/").to_string());
            if !sec("OS").is_empty() {
                kv.add("OS", sec("OS"));
            }
            kv.add("Kernel", format!("{} ({})", js(&kern, "release"), js(&kern, "machine")));

            kv.section("System");
            kv.add("Hostname", ctx.node.clone());
            let product = format!("{} {}", g("sys_vendor"), g("product_name")).trim().to_string();
            if !product.is_empty() {
                kv.add("Product", product);
            }
            let board = format!("{} {}", g("board_vendor"), g("board_name")).trim().to_string();
            if !board.is_empty() {
                kv.add("Board", format!("{board}  (BIOS {})", g("bios_version")));
            }
            if !sec("DT").is_empty() {
                kv.add("Model", sec("DT"));
            }
            kv.add("Uptime", fmt_duration(jf(s, "uptime") as u64));
            if let Some(b) = s.get("boot-info") {
                kv.add("Boot mode", format!("{}{}", js(b, "mode"), if jb(b, "secureboot") { ", secure boot" } else { "" }));
            }
            if !sec("USERS").is_empty() {
                kv.add("Logged in", format!("{} {}", sec("USERS"), tr("sessions")));
            }

            kv.section("CPU");
            kv.add("Model", js(&cpu, "model"));
            kv.add("Topology", format!("{} socket(s) · {} cores · {} threads", js(&cpu, "sockets"), js(&cpu, "cores"), js(&cpu, "cpus")));
            let c = jf(s, "cpu");
            kv.styled("Usage", format!("{}  {:.0}%  iowait {:.1}%", bar(c, 16), c * 100.0, jf(s, "wait") * 100.0), usage_style(c));
            let la: Vec<String> = s.get("loadavg").map(jarr).unwrap_or_default().iter().map(|v| v.as_str().unwrap_or("").to_string()).collect();
            kv.add("Load", la.join("  "));
            kv.add("Virtualization", if js(&cpu, "hvm") == "1" { "KVM" } else { "TCG only (no hardware virtualization)" });

            kv.section("Memory");
            for (label, key) in [("RAM", "memory"), ("Swap", "swap")] {
                if let Some(m) = s.get(key) {
                    let (u, t) = (jf(m, "used"), jf(m, "total"));
                    let fr = if t > 0.0 { u / t } else { 0.0 };
                    if t > 0.0 {
                        kv.styled(label, format!("{}  {} / {}  {:.0}%", bar(fr, 16), fmt_bytes(u), fmt_bytes(t), fr * 100.0), usage_style(fr));
                    }
                }
            }
            let arc: HashMap<String, f64> = sec("ARC").lines().filter_map(|l| {
                let p: Vec<&str> = l.split_whitespace().collect();
                (p.len() == 3).then(|| (p[0].to_string(), p[2].parse().unwrap_or(0.0)))
            }).collect();
            if let Some(size) = arc.get("size") {
                kv.add("ZFS ARC", format!("{} / max {}", fmt_bytes(*size), fmt_bytes(arc.get("c_max").cloned().unwrap_or(0.0))));
            }
            if let Some(k) = s.get("ksm") {
                let shared = jf(k, "shared");
                if shared > 0.0 {
                    kv.add("KSM saved", fmt_bytes(shared));
                }
            }

            kv.section("Storage");
            if let Some(r) = s.get("rootfs") {
                let (u, t) = (jf(r, "used"), jf(r, "total"));
                let fr = if t > 0.0 { u / t } else { 0.0 };
                kv.styled("Root FS", format!("{}  {} / {}  {:.0}%", bar(fr, 16), fmt_bytes(u), fmt_bytes(t), fr * 100.0), usage_style(fr));
            }

            kv.section("Guests");
            let (vr, vt, cr, ct) = d.guests;
            kv.styled("VMs", format!("{vr} {} / {vt}", tr("running")), if vr > 0 { style_good() } else { Style::default() });
            kv.styled("Containers", format!("{cr} {} / {ct}", tr("running")), if cr > 0 { style_good() } else { Style::default() });

            let addrs = sec("ADDR");
            if !addrs.is_empty() {
                kv.section("Management URL");
                for l in addrs.lines() {
                    if let Some((iface, cidr)) = l.split_once(' ') {
                        let ip = cidr.split('/').next().unwrap_or("");
                        kv.styled(iface, format!("https://{ip}:8006"), Style::default().fg(ACCENT));
                    }
                }
            }
        }
        draw_lines(f, area, "Overview", focused, kv.lines(), self.scroll);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut lines: Vec<Line> = Vec::new();
        let head = |t: &str| Line::from(Span::styled(tr(t).to_string(), Style::default().add_modifier(Modifier::BOLD)));
        let n = ((area.height as usize).saturating_sub(6) / 2).clamp(3, 12);
        match &self.snapshot {
            Some(d) if !d.procs.is_empty() => {
                let mut by_cpu: Vec<(&Proc, f64)> = d.procs.iter().map(|p| (p, self.cpu.get(&p.pid).cloned().unwrap_or(0.0))).collect();
                by_cpu.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                lines.push(head("Top CPU"));
                if self.cpu.is_empty() {
                    lines.push(Line::from(Span::styled(tr("Sampling..."), style_dim())));
                }
                for (p, c) in by_cpu.iter().take(if self.cpu.is_empty() { 0 } else { n }) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:>6}% ", format!("{c:.1}")), usage_style(c / 100.0)),
                        Span::styled(format!("{:<8} ", p.pid), style_label()),
                        Span::styled(p.name.clone(), if p.name.starts_with("VM ") { Style::default().fg(ACCENT) } else { Style::default() }),
                    ]));
                }
                lines.push(Line::from(""));
                let mut by_mem: Vec<&Proc> = d.procs.iter().collect();
                by_mem.sort_by(|a, b| b.rss.partial_cmp(&a.rss).unwrap_or(std::cmp::Ordering::Equal));
                lines.push(head("Top memory"));
                for p in by_mem.iter().take(n) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:>7} ", fmt_bytes(p.rss)), Style::default()),
                        Span::styled(format!("{:<8} ", p.pid), style_label()),
                        Span::styled(p.name.clone(), if p.name.starts_with("VM ") { Style::default().fg(ACCENT) } else { Style::default() }),
                    ]));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(tr("3 = all processes"), style_dim())));
            }
            Some(_) => lines.push(Line::from(Span::styled(if ctx.exec().is_some() { tr("Loading...") } else { tr("local only") }, style_dim()))),
            None => {}
        }
        draw_lines(f, area, "Top processes", focused, lines, 0);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn on_key(&mut self, key: &crossterm::event::KeyEvent, _ctx: &mut Ctx) -> bool {
        match key.code {
            crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k') => scroll(&mut self.scroll, -1),
            crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j') => scroll(&mut self.scroll, 1),
            crossterm::event::KeyCode::PageUp => scroll(&mut self.scroll, -10),
            crossterm::event::KeyCode::PageDown => scroll(&mut self.scroll, 10),
            _ => return false,
        }
        true
    }

    // the node power actions
    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        self.power.menu(ctx)
    }

    fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        self.power.on_menu(tag, key, ctx);
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        self.power.on_confirm(tag, ctx);
    }

    fn reset(&mut self) {
        self.data.clear();
        self.snapshot = None;
        self.prev.clear();
        self.prev_at = None;
        self.cpu.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps() {
        let v = parse_ps("  101 5 2048 kvm /usr/bin/kvm -id 100 -name x\n  202 1 10 bash -bash\n");
        assert_eq!(v[0].name, "VM 100");
        assert_eq!(v[1].name, "bash");
        assert_eq!(v[0].rss, 2048.0 * 1024.0);
    }
}
