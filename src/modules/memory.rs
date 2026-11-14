//! System > Memory: RAM breakdown, ZFS ARC, swap, huge pages, KSM, NUMA and the biggest
//! memory consumers, with the usual tuning actions (ARC limit, swappiness, huge pages...).

use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    Frame,
};
use std::collections::HashMap;

use super::{fv, scroll, View};
use crate::ctx::{ActionOutcome, Ctx};
use crate::exec::Exec;
use crate::i18n::tr;
use crate::loader::Loader;
use crate::ui::form::{self, Form};
use crate::ui::popup::{item, MenuItem, Popup};
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, loading_or_error, style_dim, style_warn, usage_style, Kv, ACCENT};
use crate::util::{bar, fmt_bytes};

const FILES: &str = "/proc/meminfo /proc/swaps /proc/sys/vm/swappiness /proc/sys/vm/overcommit_memory \
/proc/spl/kstat/zfs/arcstats /sys/module/zfs/parameters/zfs_arc_max /sys/module/zfs/parameters/zfs_arc_min \
/sys/kernel/mm/transparent_hugepage/enabled /sys/kernel/mm/transparent_hugepage/defrag \
/sys/kernel/mm/ksm/run /sys/kernel/mm/ksm/pages_sharing /sys/kernel/mm/ksm/pages_shared \
/sys/kernel/mm/hugepages/hugepages-*/nr_hugepages /sys/kernel/mm/hugepages/hugepages-*/free_hugepages \
/sys/devices/system/node/node*/meminfo";

#[derive(Default)]
struct MemData {
    files: HashMap<String, String>,
    /// (rss bytes, pid, name, vmid)
    top: Vec<(f64, String, String, String)>,
}

impl MemData {
    fn file(&self, p: &str) -> &str {
        self.files.get(p).map(|s| s.as_str()).unwrap_or("")
    }

    fn meminfo(&self) -> HashMap<String, f64> {
        parse_meminfo(self.file("/proc/meminfo"))
    }

    fn arc(&self) -> HashMap<String, f64> {
        self.file("/proc/spl/kstat/zfs/arcstats")
            .lines()
            .skip(2)
            .filter_map(|l| {
                let p: Vec<&str> = l.split_whitespace().collect();
                (p.len() == 3).then(|| (p[0].to_string(), p[2].parse().unwrap_or(0.0)))
            })
            .collect()
    }
}

fn parse_meminfo(s: &str) -> HashMap<String, f64> {
    s.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let mut it = v.split_whitespace();
            let n: f64 = it.next()?.parse().ok()?;
            let unit = it.next().unwrap_or("");
            Some((k.trim().to_string(), if unit == "kB" { n * 1024.0 } else { n }))
        })
        .collect()
}

/// Selected value of a sysfs choice file like "[always] madvise never".
fn sysfs_choice(s: &str) -> String {
    s.split_whitespace().find(|w| w.starts_with('[')).map(|w| w.trim_matches(|c| c == '[' || c == ']').to_string()).unwrap_or_default()
}

fn load(exec: Exec) -> anyhow::Result<MemData> {
    let script = format!(
        "for f in {FILES}; do [ -r \"$f\" ] && {{ echo \"@@@ $f\"; cat \"$f\"; }}; done; echo '@@@ PS'; ps -eo rss=,pid=,comm=,args= --sort=-rss | head -20"
    );
    let out = exec.output("sh", &["-c", &script])?;
    let mut d = MemData::default();
    let mut cur: Option<String> = None;
    let mut buf = String::new();
    let flush = |cur: &Option<String>, buf: &mut String, d: &mut MemData| {
        if let Some(c) = cur {
            d.files.insert(c.clone(), std::mem::take(buf));
        }
        buf.clear();
    };
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("@@@ ") {
            flush(&cur, &mut buf, &mut d);
            cur = Some(name.to_string());
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    flush(&cur, &mut buf, &mut d);
    for l in d.files.get("PS").cloned().unwrap_or_default().lines() {
        let p: Vec<&str> = l.split_whitespace().collect();
        if p.len() < 3 {
            continue;
        }
        let args = p[3..].join(" ");
        let vmid = if args.contains(" -id ") { args.split(" -id ").nth(1).and_then(|s| s.split_whitespace().next()).unwrap_or("").to_string() } else { String::new() };
        d.top.push((p[0].parse::<f64>().unwrap_or(0.0) * 1024.0, p[1].to_string(), p[2].to_string(), vmid));
    }
    Ok(d)
}

#[derive(Default)]
pub struct Memory {
    data: Loader<MemData>,
    table: Table,
    scroll: u16,
}

impl View for Memory {
    fn title(&self) -> &'static str {
        "Memory"
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn refresh(&mut self, ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else {
            self.data.error = Some(tr("local only").into());
            return;
        };
        self.data.load(move || load(exec));
    }

    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        self.data.poll()
    }

    fn refresh_secs(&self) -> Option<u64> {
        Some(5)
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        if let Some((m, st)) = loading_or_error(&self.data.error, self.data.loading() && self.data.data.is_none()) {
            kv.styled("", m, st);
        }
        if let Some(d) = &self.data.data {
            let mi = d.meminfo();
            let g = |k: &str| mi.get(k).cloned().unwrap_or(0.0);
            let total = g("MemTotal");
            let avail = g("MemAvailable");
            let used = total - avail;
            let frac = if total > 0.0 { used / total } else { 0.0 };
            let arc = d.arc();
            let arc_size = arc.get("size").cloned().unwrap_or(0.0);

            kv.section("RAM");
            kv.add("Total", fmt_bytes(total));
            kv.styled("Used", format!("{}  {}  {:.0}%", bar(frac, 16), fmt_bytes(used), frac * 100.0), usage_style(frac));
            kv.add("Available", fmt_bytes(avail));
            kv.add("Free", fmt_bytes(g("MemFree")));
            kv.add("Page cache", format!("{}  (buffers {})", fmt_bytes(g("Cached") + g("SReclaimable")), fmt_bytes(g("Buffers"))));
            kv.add("Shared", fmt_bytes(g("Shmem")));
            kv.add("Slab", format!("{}  (unreclaimable {})", fmt_bytes(g("Slab")), fmt_bytes(g("SUnreclaim"))));
            kv.add("Dirty / writeback", format!("{} / {}", fmt_bytes(g("Dirty")), fmt_bytes(g("Writeback"))));
            let commit = g("Committed_AS");
            let limit = g("CommitLimit");
            kv.styled(
                "Committed",
                format!("{} of {} ({:.0}%)", fmt_bytes(commit), fmt_bytes(limit), if limit > 0.0 { commit / limit * 100.0 } else { 0.0 }),
                if commit > limit { style_warn() } else { Style::default() },
            );
            kv.add("Overcommit", match d.file("/proc/sys/vm/overcommit_memory").trim() {
                "0" => "0 (heuristic)".to_string(),
                "1" => "1 (always)".to_string(),
                "2" => "2 (strict)".to_string(),
                o => o.to_string(),
            });
            if arc_size > 0.0 {
                kv.styled("Used w/o ARC", format!("{}  ({:.0}%)", fmt_bytes(used - arc_size), (used - arc_size) / total * 100.0), style_dim());
            }

            if !arc.is_empty() {
                let cmax = arc.get("c_max").cloned().unwrap_or(0.0);
                let af = if cmax > 0.0 { arc_size / cmax } else { 0.0 };
                let hits = arc.get("hits").cloned().unwrap_or(0.0);
                let miss = arc.get("misses").cloned().unwrap_or(0.0);
                kv.section("ZFS ARC");
                kv.add("Size", format!("{}  {}  of max {}", bar(af, 16), fmt_bytes(arc_size), fmt_bytes(cmax)));
                kv.add("Target / min", format!("{} / {}", fmt_bytes(arc.get("c").cloned().unwrap_or(0.0)), fmt_bytes(arc.get("c_min").cloned().unwrap_or(0.0))));
                if hits + miss > 0.0 {
                    kv.add("Hit ratio", format!("{:.1}%", hits / (hits + miss) * 100.0));
                }
                kv.add("MRU / MFU", format!("{} / {}", fmt_bytes(arc.get("mru_size").cloned().unwrap_or(0.0)), fmt_bytes(arc.get("mfu_size").cloned().unwrap_or(0.0))));
                let p = d.file("/sys/module/zfs/parameters/zfs_arc_max").trim().parse::<f64>().unwrap_or(0.0);
                kv.add("zfs_arc_max", if p == 0.0 { "0 (default)".to_string() } else { fmt_bytes(p) });
            }

            kv.section("Swap");
            let st = g("SwapTotal");
            let su = st - g("SwapFree");
            if st > 0.0 {
                let sf = su / st;
                kv.styled("Used", format!("{}  {} of {}", bar(sf, 16), fmt_bytes(su), fmt_bytes(st)), usage_style(sf));
            } else {
                kv.styled("Used", "no swap", style_dim());
            }
            kv.add("Swappiness", d.file("/proc/sys/vm/swappiness").trim().to_string());
            kv.add("Swap cache", fmt_bytes(g("SwapCached")));
            for l in d.file("/proc/swaps").lines().skip(1) {
                let p: Vec<&str> = l.split_whitespace().collect();
                if p.len() >= 5 {
                    let size = p[2].parse::<f64>().unwrap_or(0.0) * 1024.0;
                    let used = p[3].parse::<f64>().unwrap_or(0.0) * 1024.0;
                    kv.styled(p[0], format!("{} {} used of {}  prio {}", p[1], fmt_bytes(used), fmt_bytes(size), p[4]), style_dim());
                }
            }

            kv.section("Huge pages");
            let mut sizes: Vec<&String> = d.files.keys().filter(|k| k.contains("/hugepages-") && k.ends_with("/nr_hugepages")).collect();
            sizes.sort();
            for k in sizes {
                let dir = k.trim_end_matches("/nr_hugepages");
                let size = dir.rsplit("hugepages-").next().unwrap_or("").trim_end_matches("kB").parse::<f64>().unwrap_or(0.0) * 1024.0;
                let total: f64 = d.file(k).trim().parse().unwrap_or(0.0);
                let free: f64 = d.file(&format!("{dir}/free_hugepages")).trim().parse().unwrap_or(0.0);
                kv.add(
                    &format!("{} pages", fmt_bytes(size)),
                    if total > 0.0 { format!("{total:.0} total, {free:.0} free = {} reserved", fmt_bytes(total * size)) } else { "0".into() },
                );
            }
            kv.add("Reserved / surplus", format!("{} / {}", g("HugePages_Rsvd"), g("HugePages_Surp")));
            kv.add("Transparent (THP)", format!(
                "{}  defrag {}  in use {}",
                sysfs_choice(d.file("/sys/kernel/mm/transparent_hugepage/enabled")),
                sysfs_choice(d.file("/sys/kernel/mm/transparent_hugepage/defrag")),
                fmt_bytes(g("AnonHugePages"))
            ));

            kv.section("KSM");
            let run = d.file("/sys/kernel/mm/ksm/run").trim().to_string();
            let sharing: f64 = d.file("/sys/kernel/mm/ksm/pages_sharing").trim().parse().unwrap_or(0.0);
            kv.styled("State", if run == "1" { "running" } else { "stopped" }, if run == "1" { crate::ui::style_good() } else { style_dim() });
            kv.add("Saved", fmt_bytes(sharing * 4096.0));

            let mut nodes: Vec<&String> = d.files.keys().filter(|k| k.starts_with("/sys/devices/system/node/")).collect();
            if nodes.len() > 1 {
                nodes.sort();
                kv.section("NUMA");
                for n in nodes {
                    let name = n.split('/').nth(5).unwrap_or("");
                    let m: HashMap<String, f64> = d
                        .file(n)
                        .lines()
                        .filter_map(|l| {
                            let p: Vec<&str> = l.split_whitespace().collect();
                            (p.len() >= 4).then(|| (p[2].trim_end_matches(':').to_string(), p[3].parse::<f64>().unwrap_or(0.0) * 1024.0))
                        })
                        .collect();
                    let t = m.get("MemTotal").cloned().unwrap_or(0.0);
                    let fr = m.get("MemFree").cloned().unwrap_or(0.0);
                    kv.add(name, format!("{} free of {}", fmt_bytes(fr), fmt_bytes(t)));
                }
            }
        }
        draw_lines(f, area, "Memory", focused, kv.lines(), self.scroll);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, _ctx: &Ctx, focused: bool) {
        let rows: Vec<Row> = self
            .data
            .data
            .iter()
            .flat_map(|d| d.top.iter())
            .map(|(rss, pid, name, vmid)| {
                Row::new(pid.clone())
                    .c(fmt_bytes(*rss))
                    .c(pid.clone())
                    .cs(if vmid.is_empty() { name.clone() } else { format!("VM {vmid}") }, if vmid.is_empty() { Style::default() } else { Style::default().fg(ACCENT) })
            })
            .collect();
        let cols = [col("RSS", Constraint::Length(8)), col("PID", Constraint::Length(8)), col("Process", Constraint::Min(10))];
        self.table.draw(f, area, "Top memory", &cols, &rows, focused, None);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }

    fn on_key(&mut self, key: &crossterm::event::KeyEvent, _ctx: &mut Ctx) -> bool {
        // the left pane is a scrollable report
        match key.code {
            crossterm::event::KeyCode::PageDown => {
                scroll(&mut self.scroll, 10);
                true
            }
            crossterm::event::KeyCode::PageUp => {
                scroll(&mut self.scroll, -10);
                true
            }
            _ => false,
        }
    }

    fn menu(&self, ctx: &Ctx) -> Vec<MenuItem> {
        let r = ctx.need_root().and(ctx.need_exec("Sys.Modify").map(|_| ()));
        let has_arc = self.data.data.as_ref().map(|d| !d.arc().is_empty()).unwrap_or(false);
        vec![
            item('z', "Set ZFS ARC limit").require(r.clone()).disabled_if(!has_arc, "ZFS not loaded"),
            item('w', "Set swappiness").require(r.clone()),
            item('p', "Set huge pages").require(r.clone()),
            item('T', "Transparent huge pages mode").require(r.clone()),
            item('K', "Start / stop KSM").require(r.clone()),
            item('d', "Drop page cache").require(r.clone()),
            item('S', "Flush swap (swapoff -a; swapon -a)").require(r),
        ]
    }

    fn on_menu(&mut self, tag: &str, key: char, ctx: &mut Ctx) {
        if tag == "thp" {
            let mode = match key {
                '1' => "always",
                '2' => "madvise",
                _ => "never",
            };
            self.write(ctx, "/sys/kernel/mm/transparent_hugepage/enabled", mode, &format!("THP = {mode}"));
            return;
        }
        let d = self.data.data.as_ref();
        let file = |p: &str| d.map(|d| d.file(p).trim().to_string()).unwrap_or_default();
        match key {
            'z' => {
                let total = d.map(|d| d.meminfo().get("MemTotal").cloned().unwrap_or(0.0)).unwrap_or(0.0);
                let cur = file("/sys/module/zfs/parameters/zfs_arc_max").parse::<f64>().unwrap_or(0.0);
                let gib = if cur > 0.0 { format!("{:.0}", cur / 1073741824.0) } else { String::new() };
                ctx.popup(Popup::Form(Form::new(
                    "Set ZFS ARC limit",
                    "arc",
                    vec![
                        form::info("RAM", &fmt_bytes(total)),
                        form::number("gib", "Max ARC (GiB)", &gib).help("0 = default (half of RAM); PVE recommends 2 GiB + 1 GiB per TiB of storage"),
                        form::check("persist", "Keep after reboot", true),
                    ],
                )));
            }
            'w' => ctx.popup(Popup::Form(Form::new(
                "Set swappiness",
                "swappiness",
                vec![
                    form::number("value", "vm.swappiness", &file("/proc/sys/vm/swappiness")).help("0-200, lower = swap later; 10 is common on hypervisors"),
                    form::check("persist", "Keep after reboot", true),
                ],
            ))),
            'p' => {
                let mut opts = Vec::new();
                if let Some(d) = d {
                    let mut ks: Vec<&String> = d.files.keys().filter(|k| k.ends_with("/nr_hugepages")).collect();
                    ks.sort();
                    for k in ks {
                        let size = k.split("hugepages-").nth(1).and_then(|s| s.split('/').next()).unwrap_or("").to_string();
                        opts.push((size.clone(), format!("{size} (now {})", d.file(k).trim())));
                    }
                }
                if opts.is_empty() {
                    ctx.popup(Popup::error("Huge pages", "this kernel exposes no huge page sizes"));
                    return;
                }
                ctx.popup(Popup::Form(Form::new(
                    "Set huge pages",
                    "huge",
                    vec![
                        form::select("size", "Page size", opts, "2048kB"),
                        form::number("count", "Number of pages", "0").help("memory is reserved immediately and is not available to the host"),
                        form::check("persist", "Keep after reboot (2 MiB only)", true),
                    ],
                )));
            }
            'T' => ctx.popup(Popup::menu("Transparent huge pages", "thp", vec![item('1', "always"), item('2', "madvise"), item('3', "never")])),
            'K' => {
                let run = file("/sys/kernel/mm/ksm/run");
                let to = if run == "1" { "0" } else { "1" };
                self.write(ctx, "/sys/kernel/mm/ksm/run", to, if to == "1" { "Start KSM" } else { "Stop KSM" });
            }
            'd' => ctx.popup(Popup::confirm(
                "Drop page cache",
                "drop",
                vec!["Free page cache, dentries and inodes (sync; echo 3 > drop_caches)?".into(), "Disk reads will be slower until the cache warms up again.".into()],
                false,
            )),
            'S' => ctx.popup(Popup::confirm(
                "Flush swap",
                "swap",
                vec!["Move all swapped pages back into RAM (swapoff -a; swapon -a)?".into(), "Needs enough free RAM; this can take minutes.".into()],
                true,
            )),
            _ => {}
        }
    }

    fn on_confirm(&mut self, tag: &str, ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else { return };
        match tag {
            "drop" => ctx.exec_call("Drop page cache", exec, "sh", vec!["-c".into(), "sync && echo 3 > /proc/sys/vm/drop_caches".into()]),
            "swap" => ctx.exec_call("Flush swap", exec, "sh", vec!["-c".into(), "swapoff -a && swapon -a".into()]),
            _ => {}
        }
    }

    fn on_form(&mut self, tag: &str, v: &[(String, String)], ctx: &mut Ctx) {
        let Some(exec) = ctx.exec() else { return };
        let persist = fv(v, "persist") == "1";
        match tag {
            "arc" => {
                let gib: f64 = fv(v, "gib").parse().unwrap_or(0.0);
                let bytes = (gib * 1073741824.0) as u64;
                let mut script = String::from("echo \"$1\" > /sys/module/zfs/parameters/zfs_arc_max");
                if persist {
                    script.push_str(
                        "; f=/etc/modprobe.d/zfs.conf; touch $f; sed -i '/^options zfs zfs_arc_max=/d' $f; \
                         [ \"$1\" != 0 ] && echo \"options zfs zfs_arc_max=$1\" >> $f; \
                         if [ \"$(findmnt -n -o FSTYPE /)\" = zfs ]; then update-initramfs -u -k all >/dev/null 2>&1; fi",
                    );
                }
                run(ctx, exec, &format!("ZFS ARC max = {}", if bytes == 0 { "default".into() } else { fmt_bytes(bytes as f64) }), &script, &[bytes.to_string()]);
            }
            "swappiness" => {
                let val = fv(v, "value").parse::<u32>().unwrap_or(60).min(200).to_string();
                let mut script = String::from("sysctl -q -w vm.swappiness=\"$1\"");
                if persist {
                    script.push_str(&persist_sysctl("vm.swappiness"));
                }
                run(ctx, exec, &format!("vm.swappiness = {val}"), &script, &[val]);
            }
            "huge" => {
                let size = fv(v, "size").to_string();
                let count = fv(v, "count").parse::<u64>().unwrap_or(0).to_string();
                if !size.chars().all(|c| c.is_ascii_alphanumeric()) {
                    return;
                }
                let mut script = format!("echo \"$1\" > /sys/kernel/mm/hugepages/hugepages-{size}/nr_hugepages; cat /sys/kernel/mm/hugepages/hugepages-{size}/nr_hugepages");
                if persist && size == "2048kB" {
                    script.push_str(&persist_sysctl("vm.nr_hugepages"));
                }
                run(ctx, exec, &format!("huge pages {size} = {count}"), &script, &[count]);
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.data.clear();
    }
}

impl Memory {
    fn write(&self, ctx: &mut Ctx, path: &str, value: &str, title: &str) {
        if let Some(exec) = ctx.exec() {
            run(ctx, exec, title, "echo \"$1\" > \"$2\"", &[value.to_string(), path.to_string()]);
        }
    }
}

/// Shell snippet appended to persist a sysctl value ($1) in /etc/sysctl.d.
fn persist_sysctl(key: &str) -> String {
    format!("; f=/etc/sysctl.d/99-pxvirt-console.conf; touch $f; sed -i '/^{key} *=/d' $f; echo \"{key} = $1\" >> $f")
}

/// Run a root shell snippet with positional arguments (no string interpolation of user input).
fn run(ctx: &mut Ctx, exec: Exec, title: &str, script: &str, args: &[String]) {
    let mut a = vec!["-c".to_string(), script.to_string(), "sh".to_string()];
    a.extend(args.iter().cloned());
    let t = title.to_string();
    ctx.act(title.to_string(), move || {
        let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        exec.output("sh", &refs)?;
        Ok(ActionOutcome::Msg(format!("{t}: OK")))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_units() {
        let m = parse_meminfo("MemTotal:       16023088 kB\nHugePages_Total:       4\n");
        assert_eq!(m["MemTotal"], 16023088.0 * 1024.0);
        assert_eq!(m["HugePages_Total"], 4.0);
    }

    #[test]
    fn choice() {
        assert_eq!(sysfs_choice("always [madvise] never"), "madvise");
    }
}
