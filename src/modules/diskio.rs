//! Disk IO from the live sampler (/proc/diskstats): helpers for the disk pages and the
//! Disks → IO page listing every block device.

use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    Frame,
};

use super::{scroll, View};
use crate::ctx::Ctx;
use crate::i18n::tr;
use crate::metrics::DiskStat;
use crate::ui::table::{col, Row, Table};
use crate::ui::{draw_lines, sparkline, style_dim, style_label, usage_style, Kv, ACCENT};
use crate::util::{fmt_bytes, fmt_rate};

/// Latency style: SSD-ish below 5 ms, HDD-ish below 30 ms, worse above.
pub fn lat_style(ms: f64) -> Style {
    usage_style(if ms <= 5.0 { 0.1 } else if ms <= 30.0 { 0.75 } else { 0.95 })
}

pub fn ms(v: f64) -> String {
    if v <= 0.0 {
        "-".into()
    } else if v < 10.0 {
        format!("{v:.1}ms")
    } else {
        format!("{v:.0}ms")
    }
}

fn rate(v: f64) -> String {
    if v < 1.0 {
        "-".into()
    } else {
        fmt_rate(v)
    }
}

fn iops(v: f64) -> String {
    if v < 0.5 {
        "-".into()
    } else {
        format!("{v:.0}")
    }
}

/// Table cells: read, write, IOPS, latency, util (with styles).
pub fn cells(row: Row, st: Option<&DiskStat>) -> Row {
    match st {
        Some(s) => {
            let lat = s.r_await.max(s.w_await);
            row.c(rate(s.r_bps))
                .c(rate(s.w_bps))
                .c(iops(s.r_iops + s.w_iops))
                .cs(ms(lat), lat_style(lat))
                .cs(format!("{:.0}%", s.util * 100.0), usage_style(s.util))
        }
        None => row.c("").c("").c("").c("").c(""),
    }
}

/// Column definitions matching [`cells`].
pub fn cols() -> [crate::ui::table::Col; 5] {
    [
        col("Read", Constraint::Length(9)),
        col("Write", Constraint::Length(9)),
        col("IOPS", Constraint::Length(6)),
        col("Latency", Constraint::Length(7)),
        col("Util", Constraint::Length(5)),
    ]
}

/// "IO" section of a details pane.
pub fn detail(kv: &mut Kv, s: &DiskStat) {
    kv.section("IO");
    kv.add("Read", format!("{}  {} IOPS  {} {}", fmt_rate(s.r_bps), iops(s.r_iops), tr("latency"), ms(s.r_await)));
    kv.add("Write", format!("{}  {} IOPS  {} {}", fmt_rate(s.w_bps), iops(s.w_iops), tr("latency"), ms(s.w_await)));
    kv.styled("Util", format!("{:.0}%   {} {}", s.util * 100.0, tr("in flight"), s.inflight), usage_style(s.util));
    kv.add("Since boot", format!("{} {} / {} {}", tr("read"), fmt_bytes(s.r_total as f64), tr("written"), fmt_bytes(s.w_total as f64)));
}

/// Sparkline graphs of a device (read, write, util, latency).
pub fn graphs(s: &DiskStat, width: usize) -> Vec<Line<'static>> {
    let w = width.saturating_sub(4).max(10);
    let mut out = vec![Line::from("")];
    let mut g = |title: String, h: &[f64], max: Option<f64>, st: Style| {
        out.push(Line::from(Span::styled(title, style_label())));
        out.push(Line::from(Span::styled(sparkline(h, w, max), st)));
    };
    g(format!("{} {}", tr("Read"), fmt_rate(s.r_bps)), &s.r_hist, None, Style::default().fg(ACCENT));
    g(format!("{} {}", tr("Write"), fmt_rate(s.w_bps)), &s.w_hist, None, crate::ui::style_warn());
    g(format!("{} {:.0}%", tr("Util"), s.util * 100.0), &s.util_hist, Some(1.0), usage_style(s.util));
    g(format!("{} {}", tr("Latency"), ms(s.r_await.max(s.w_await))), &s.lat_hist, None, lat_style(s.r_await.max(s.w_await)));
    out
}

fn kind(s: &DiskStat) -> &'static str {
    let n = s.name.as_str();
    if n.starts_with("nvme") {
        "NVMe"
    } else if n.starts_with("dm-") {
        "dm"
    } else if n.starts_with("md") {
        "md"
    } else if n.starts_with("zd") {
        "zvol"
    } else if n.starts_with("rbd") {
        "rbd"
    } else if n.starts_with("nbd") {
        "nbd"
    } else if s.physical {
        "disk"
    } else {
        "virtual"
    }
}

/// Disks → IO: every block device, busiest first.
#[derive(Default)]
pub struct DiskIo {
    table: Table,
    scroll: u16,
    /// 0 = busiest, 1 = name
    sort: usize,
    all: bool,
}

impl DiskIo {
    fn list(&self, ctx: &Ctx) -> Vec<DiskStat> {
        let m = ctx.metrics.lock().unwrap();
        let mut v: Vec<DiskStat> = m.disks.iter().filter(|d| self.all || d.physical || d.r_bps + d.w_bps > 0.0 || d.name.starts_with("nvme")).cloned().collect();
        match self.sort {
            1 => v.sort_by(|a, b| a.name.cmp(&b.name)),
            _ => v.sort_by(|a, b| (b.util, b.r_bps + b.w_bps).partial_cmp(&(a.util, a.r_bps + a.w_bps)).unwrap_or(std::cmp::Ordering::Equal)),
        }
        v
    }
}

impl View for DiskIo {
    fn title(&self) -> &'static str {
        "IO"
    }

    fn refresh(&mut self, _ctx: &mut Ctx) {}

    fn refresh_secs(&self) -> Option<u64> {
        Some(1)
    }

    fn needs_host(&self) -> bool {
        true
    }

    fn table(&mut self) -> Option<&mut Table> {
        Some(&mut self.table)
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("o", "sort"), ("v", "all devices")]
    }

    fn on_key(&mut self, key: &crossterm::event::KeyEvent, _ctx: &mut Ctx) -> bool {
        match key.code {
            crossterm::event::KeyCode::Char('o') => self.sort = (self.sort + 1) % 2,
            crossterm::event::KeyCode::Char('v') => self.all = !self.all,
            _ => return false,
        }
        true
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let list = self.list(ctx);
        let rows: Vec<Row> = list
            .iter()
            .map(|s| {
                let r = Row::new(s.name.clone()).c(s.name.clone()).cs(kind(s), style_dim());
                cells(r, Some(s)).c(if s.inflight > 0 { s.inflight.to_string() } else { String::new() }).dim(s.util < 0.001 && s.r_bps + s.w_bps < 1.0)
            })
            .collect();
        let mut cols = vec![col("Device", Constraint::Length(10)), col("Type", Constraint::Length(7))];
        cols.extend(cols_owned());
        cols.push(col("Queue", Constraint::Length(5)));
        let title = format!("{} [{}{}]", tr("Disk IO"), tr(if self.sort == 0 { "busiest first" } else { "by name" }), if self.all { " · all" } else { "" });
        let empty = if ctx.metrics.lock().unwrap().disks.is_empty() { Some((tr("Waiting for samples...").to_string(), style_dim())) } else { None };
        self.table.draw(f, area, &title, &cols, &rows, focused, empty);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool) {
        let mut kv = Kv::new();
        let mut lines = Vec::new();
        if let Some(s) = self.table.selected().and_then(|k| self.list(ctx).into_iter().find(|d| d.name == k)) {
            kv.add("Device", format!("/dev/{}  ({})", s.name, kind(&s)));
            detail(&mut kv, &s);
            lines = graphs(&s, area.width as usize);
        }
        let mut all = kv.lines();
        all.extend(lines);
        draw_lines(f, area, "Details", focused, all, self.scroll);
    }

    fn scroll_detail(&mut self, d: i32) {
        scroll(&mut self.scroll, d);
    }
}

fn cols_owned() -> Vec<crate::ui::table::Col> {
    cols().into_iter().collect()
}
