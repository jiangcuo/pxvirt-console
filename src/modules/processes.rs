//! System > Processes: live CPU% (delta between refreshes, like top), sortable columns,
//! guest (VMID) mapping for QEMU/LXC, terminate / kill.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    Frame,
};
use std::collections::HashMap;
use std::time::Instant;

use super::{scroll, View};
use crate::ctx::Ctx;
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, style_bad, style_warn, Kv, ACCENT};
use crate::util::{fmt_bytes, fmt_duration};

#[derive(Clone)]
struct Proc {
    pid: u32,
    user: String,
    cpu: f64,
    mem: f64,
    rss: u64,
    threads: u32,
    stat: String,
    etime: u64,
    cputime: f64,
    comm: String,
    args: String,
    vmid: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Cpu,
    Mem,
    Pid,
    Threads,
    Name,
    Time,
}

const SORT_KEYS: [(char, Sort, &str); 6] = [
    ('c', Sort::Cpu, "CPU"),
    ('m', Sort::Mem, "memory"),
    ('p', Sort::Pid, "PID"),
    ('t', Sort::Threads, "threads"),
    ('n', Sort::Name, "name"),
    ('T', Sort::Time, "CPU time"),
];

pub struct Processes {
    data: Loader<(Instant, Vec<Proc>)>,
    procs: Vec<Proc>,
    prev: HashMap<u32, f64>,
    prev_at: Option<Instant>,
    table: Table,
    sort: Sort,
    desc: bool,
    scroll: u16,
}

impl Default for Processes {
    fn default() -> Self {
        Self { data: Loader::default(), procs: Vec::new(), prev: HashMap::new(), prev_at: None, table: Table::default(), sort: Sort::Cpu, desc: true, scroll: 0 }
    }
}

fn parse_ps(out: &str) -> Vec<Proc> {
    let mut v = Vec::new();
    for l in out.lines() {
        let p: Vec<&str> = l.split_whitespace().collect();
        if p.len() < 10 {
            continue;
        }
        let args = p[9..].join(" ");
        let vmid = if args.contains("/usr/bin/kvm") || args.contains("qemu-system") {
            args.split(" -id ").nth(1).and_then(|s| s.split_whitespace().next()).unwrap_or("").to_string()
        } else if args.starts_with("[lxc monitor]") {
            args.split_whitespace().last().unwrap_or("").to_string()
        } else {
            String::new()
        };
        v.push(Proc {
            pid: p[0].parse().unwrap_or(0),
            user: p[1].into(),
            cpu: 0.0,
            mem: p[2].parse().unwrap_or(0.0),
            rss: p[3].parse::<u64>().unwrap_or(0) * 1024,
            threads: p[4].parse().unwrap_or(0),
            stat: p[5].into(),
            etime: p[6].parse().unwrap_or(0),
            cputime: p[7].parse().unwrap_or(0.0),
            comm: p[8].into(),
            args,
            vmid,
        });
    }
    v
}

impl Processes {
    fn sel(&self) -> Option<&Proc> {
        let k = self.table.selected()?;
        self.procs.iter().find(|p| p.pid.to_string() == k)
    }

    fn arrow(&self, s: Sort) -> &'static str {
        if self.sort != s {
            ""
        } else if self.desc {
            " ▼"
        } else {
            " ▲"
        }
    }
}

impl View for Processes {
    fn title(&self) -> &'static str {
        "Processes"
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else {
            self.data.error = Some(tr("local only").into());
            return;
        };
        self.data.load(move || {
            let out = exec.output("ps", &["-eo", "pid=,user:16=,pmem=,rss=,nlwp=,stat=,etimes=,times=,comm=,args="])?;
            Ok((Instant::now(), parse_ps(&out)))
        });
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        if !self.data.poll() {
            return false;
        }
        if let Some((at, list)) = self.data.data.take() {
            let dt = self.prev_at.map(|p| at.duration_since(p).as_secs_f64()).unwrap_or(0.0);
            let mut procs = list;
            for p in procs.iter_mut() {
                if let (Some(prev), true) = (self.prev.get(&p.pid), dt > 0.0) {
                    p.cpu = ((p.cputime - prev) / dt * 100.0).max(0.0);
                } else if p.etime > 0 {
                    p.cpu = p.cputime / p.etime as f64 * 100.0;
                }
            }
            self.prev = procs.iter().map(|p| (p.pid, p.cputime)).collect();
            self.prev_at = Some(at);
            self.procs = procs;
        }
        true
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(2)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("c/m/p/t/n", "sort")]
    }

    fn on_key(&mut self, key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        if let KeyCode::Char(c) = key.code {
            if let Some((_, s, _)) = SORT_KEYS.iter().find(|(k, _, _)| *k == c) {
                if self.sort == *s {
                    self.desc = !self.desc;
                } else {
                    self.sort = *s;
                    self.desc = !matches!(s, Sort::Pid | Sort::Name);
                }
                return true;
            }
        }
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut procs = self.procs.clone();
        let ord = |a: f64, b: f64| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
        match self.sort {
            Sort::Cpu => procs.sort_by(|a, b| ord(a.cpu, b.cpu)),
            Sort::Mem => procs.sort_by_key(|p| p.rss),
            Sort::Pid => procs.sort_by_key(|p| p.pid),
            Sort::Threads => procs.sort_by_key(|p| p.threads),
            Sort::Name => procs.sort_by_key(|p| p.comm.to_lowercase()),
            Sort::Time => procs.sort_by(|a, b| ord(a.cputime, b.cputime)),
        }
        if self.desc {
            procs.reverse();
        }
        let total_cpu: f64 = procs.iter().map(|p| p.cpu).sum();
        let rows: Vec<Row> = procs
            .iter()
            .map(|p| {
                let name = if p.vmid.is_empty() { p.comm.clone() } else { format!("{} (VM {})", p.comm, p.vmid) };
                Row::new(p.pid.to_string())
                    .c(p.pid.to_string())
                    .cs(name, if p.vmid.is_empty() { Style::default() } else { Style::default().fg(ACCENT) })
                    .cs(format!("{:.1}", p.cpu), if p.cpu >= 90.0 { style_bad() } else if p.cpu >= 30.0 { style_warn() } else { Style::default() })
                    .c(fmt_bytes(p.rss as f64))
                    .c(p.threads.to_string())
                    .c(p.user.clone())
                    .c(fmt_duration(p.cputime as u64))
                    .c(p.args.clone())
                    .dim(p.cpu < 0.1 && p.stat.starts_with('S'))
            })
            .collect();
        let cols = [
            col(&format!("PID{}", self.arrow(Sort::Pid)), Constraint::Length(9)),
            col(&format!("{}{}", tr("Name"), self.arrow(Sort::Name)), Constraint::Length(18)),
            col(&format!("CPU%{}", self.arrow(Sort::Cpu)), Constraint::Length(7)),
            col(&format!("{}{}", tr("Memory"), self.arrow(Sort::Mem)), Constraint::Length(9)),
            col(&format!("{}{}", tr("Threads"), self.arrow(Sort::Threads)), Constraint::Length(7)),
            col("User", Constraint::Length(10)),
            col(&format!("{}{}", tr("CPU time"), self.arrow(Sort::Time)), Constraint::Length(10)),
            col("Command", Constraint::Min(20)),
        ];
        let title = format!("{}  ·  {} {:.0}%", tr("Processes"), tr("total CPU"), total_cpu);
        self.table.draw(f, area, &title, &cols, &rows, focused, loading_or_error(&self.data.error, self.data.loading() && self.procs.is_empty()));
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some(p) = self.sel() {
            kv.add("PID", p.pid.to_string());
            kv.add("User", p.user.clone());
            kv.add("State", p.stat.clone());
            kv.add("CPU", format!("{:.1}%", p.cpu));
            kv.add("Memory", format!("{} ({:.1}%)", fmt_bytes(p.rss as f64), p.mem));
            kv.add("Threads", p.threads.to_string());
            kv.add("CPU time", fmt_duration(p.cputime as u64));
            kv.add("Running for", fmt_duration(p.etime));
            if !p.vmid.is_empty() {
                kv.styled("Guest", p.vmid.clone(), Style::default().fg(ACCENT));
            }
            kv.section("Command line");
            for (i, chunk) in p.args.split(" -").enumerate() {
                kv.add("", if i == 0 { chunk.to_string() } else { format!("-{chunk}") });
            }
        }
        draw_lines(f, area, "Details", focused, kv.lines(), self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        if self.sel().is_none() {
            return vec![];
        }
        let r = ctx.need_root().and(ctx.need_exec("Sys.Modify").map(|_| ()));
        vec![item('x', "Terminate (SIGTERM)").require(r.clone()), item('X', "Kill (SIGKILL)").require(r)]
    }

    fn on_menu(&mut self, _tag: &str, key: char, ctx: &mut Ctx) {
        let Some(p) = self.sel() else { return };
        let sig = if key == 'X' { "KILL" } else { "TERM" };
        let mut body = vec![format!("PID {}: {}", p.pid, crate::util::truncate(&p.args, 70))];
        if !p.vmid.is_empty() {
            body.push(format!("This is the process of guest {}. Prefer stopping the guest instead.", p.vmid));
        }
        ctx.popup(Popup::confirm(&format!("Send SIG{sig}"), &format!("{sig}:{}", p.pid), body, key == 'X' || !p.vmid.is_empty()));
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        if let (Some((sig, pid)), Some(exec)) = (tag.split_once(':'), ctx.exec()) {
            ctx.exec_call(format!("kill -{sig} {pid}"), exec, "kill", vec![format!("-{sig}"), pid.to_string()]);
        }
    }

    fn reset(&mut self) {
        self.data.clear();
        self.procs.clear();
        self.prev.clear();
        self.prev_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_line() {
        let v = parse_ps("  1234 root 1.5 20480 4 Sl 3600 125 kvm /usr/bin/kvm -id 101 -name vm1\n");
        assert_eq!(v[0].pid, 1234);
        assert_eq!(v[0].rss, 20480 * 1024);
        assert_eq!(v[0].cputime, 125.0);
        assert_eq!(v[0].vmid, "101");
    }
}
