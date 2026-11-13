//! Full-screen log viewer: follow mode, regex-free substring search with highlight,
//! filter-only mode, priority and time-range filters (journalctl), export.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::popup::Outcome;
use super::{block, hint_spans, style_bad, style_warn, vsplit, ACCENT};
use crate::api::Api;
use crate::exec::{Exec, Stream};
use crate::i18n::tr;
use crate::loader::Job;
use crate::util::{jarr, js};

const MAX_LINES: usize = 200_000;

#[derive(Clone)]
pub enum Source {
    /// journalctl on the node (local or ssh)
    Journal { exec: Exec, unit: Option<String>, kernel: bool, extra: Vec<String> },
    /// /nodes/{node}/journal
    ApiJournal { api: Arc<Api>, node: String },
    /// /nodes/{node}/syslog?service=
    ApiSyslog { api: Arc<Api>, node: String, service: String },
    /// Streaming command output (ping, traceroute, apt...)
    Command { exec: Exec, prog: String, args: Vec<String> },
    Static(Vec<String>),
}

const PRIOS: [&str; 6] = ["", "err", "warning", "notice", "info", "debug"];
const RANGES: [(&str, &str); 6] = [
    ("", "all"),
    ("boot", "this boot"),
    ("-15m", "15 min"),
    ("-1h", "1 hour"),
    ("-24h", "24 hours"),
    ("today", "today"),
];

struct Loaded {
    lines: Vec<String>,
    cursor: Option<String>,
    next_start: Option<u64>,
}

pub struct LogViewer {
    pub title: String,
    source: Source,
    lines: Vec<String>,
    /// top line index when not following
    top: usize,
    follow: bool,
    search: String,
    typing: bool,
    filter_only: bool,
    prio: usize,
    range: usize,
    limit: usize,
    job: Option<Job<Loaded>>,
    stream: Option<Stream>,
    cursor: Option<String>,
    next_start: Option<u64>,
    last_poll: Instant,
    status: String,
    finished: Option<bool>,
    view_h: usize,
    match_idx: Option<usize>,
}

impl LogViewer {
    pub fn new(title: &str, source: Source) -> Self {
        let mut v = Self {
            title: title.to_string(),
            source,
            lines: Vec::new(),
            top: 0,
            follow: true,
            search: String::new(),
            typing: false,
            filter_only: false,
            prio: 0,
            range: 1,
            limit: 3000,
            job: None,
            stream: None,
            cursor: None,
            next_start: None,
            last_poll: Instant::now(),
            status: String::new(),
            finished: None,
            view_h: 20,
            match_idx: None,
        };
        if let Source::Journal { kernel: false, unit: None, ref extra, .. } = v.source {
            if extra.is_empty() {
                v.range = 3; // whole journal: default to last hour
            }
        }
        v.reload();
        v
    }

    /// Start with a priority filter: 1=err, 2=warning, 3=notice, 4=info, 5=debug.
    pub fn with_prio(mut self, p: usize) -> Self {
        self.prio = p.min(PRIOS.len() - 1);
        self.reload();
        self
    }

    /// Time range index: 0=all 1=boot 2=15m 3=1h 4=24h 5=today.
    pub fn with_range(mut self, r: usize) -> Self {
        self.range = r.min(RANGES.len() - 1);
        self.reload();
        self
    }

    fn is_journal(&self) -> bool {
        matches!(self.source, Source::Journal { .. })
    }

    fn journal_args(&self, follow: bool) -> Vec<String> {
        let Source::Journal { unit, kernel, extra, .. } = &self.source else { return vec![] };
        let mut a: Vec<String> = vec!["--no-pager".into(), "-o".into(), "short-iso".into()];
        if follow {
            a.extend(["-f".into(), "-n".into(), "0".into()]);
        } else {
            a.extend(["-n".into(), self.limit.to_string()]);
        }
        if let Some(u) = unit {
            a.extend(["-u".into(), u.clone()]);
        }
        if *kernel {
            a.push("-k".into());
        }
        a.extend(extra.iter().cloned());
        if !PRIOS[self.prio].is_empty() {
            a.extend(["-p".into(), PRIOS[self.prio].into()]);
        }
        match RANGES[self.range].0 {
            "" => {}
            "boot" => a.push("-b".into()),
            r => a.push(format!("--since={r}")),
        }
        a
    }

    fn since_epoch(&self) -> Option<i64> {
        let now = chrono::Local::now().timestamp();
        match RANGES[self.range].0 {
            "-15m" => Some(now - 900),
            "-1h" => Some(now - 3600),
            "-24h" => Some(now - 86400),
            "today" => Some(
                chrono::Local::now()
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .map(|d| d.and_local_timezone(chrono::Local).single().map(|t| t.timestamp()).unwrap_or(now))
                    .unwrap_or(now),
            ),
            _ => None,
        }
    }

    pub fn reload(&mut self) {
        self.stream = None;
        self.lines.clear();
        self.cursor = None;
        self.next_start = None;
        self.finished = None;
        self.status = tr("Loading...").to_string();
        let limit = self.limit;
        match self.source.clone() {
            Source::Static(l) => {
                self.lines = l;
                self.status.clear();
            }
            Source::Command { exec, prog, args } => {
                let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                match exec.stream(&prog, &a) {
                    Ok(s) => {
                        self.stream = Some(s);
                        self.status = "running".into();
                    }
                    Err(e) => self.status = format!("{e:#}"),
                }
            }
            Source::Journal { exec, .. } => {
                let args = self.journal_args(false);
                self.job = Some(Job::spawn(move || {
                    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                    let out = exec.output("journalctl", &a)?;
                    Ok(Loaded { lines: out.lines().map(|s| s.to_string()).collect(), cursor: None, next_start: None })
                }));
            }
            Source::ApiJournal { api, node } => {
                let since = self.since_epoch();
                self.job = Some(Job::spawn(move || {
                    let path = match since {
                        Some(s) => format!("/nodes/{node}/journal?since={s}&lastentries={limit}"),
                        None => format!("/nodes/{node}/journal?lastentries={limit}"),
                    };
                    let v = api.get(&path)?;
                    Ok(parse_journal(&v))
                }));
            }
            Source::ApiSyslog { api, node, service } => {
                self.job = Some(Job::spawn(move || {
                    let first = api.get_full(&format!("/nodes/{node}/syslog?service={service}&limit=1"))?;
                    let total = first.get("total").and_then(|t| t.as_u64()).unwrap_or(0);
                    let start = total.saturating_sub(limit as u64);
                    let v = api.get_full(&format!("/nodes/{node}/syslog?service={service}&start={start}&limit={limit}"))?;
                    let rows = v.get("data").map(jarr).unwrap_or_default();
                    let lines: Vec<String> = rows.iter().map(|r| js(r, "t")).filter(|l| l != "no content").collect();
                    Ok(Loaded { next_start: Some(start + rows.len() as u64), lines, cursor: None })
                }));
            }
        }
    }

    pub fn tick(&mut self) {
        if let Some(job) = &self.job {
            if let Some(r) = job.poll() {
                self.job = None;
                match r {
                    Ok(l) => {
                        let follow_poll = self.cursor.is_some() || self.next_start.is_some();
                        if follow_poll && !self.lines.is_empty() {
                            self.lines.extend(l.lines);
                        } else {
                            self.lines = l.lines;
                        }
                        if l.cursor.is_some() {
                            self.cursor = l.cursor;
                        }
                        if l.next_start.is_some() {
                            self.next_start = l.next_start;
                        }
                        self.status.clear();
                        if self.is_journal() && self.stream.is_none() {
                            if let Source::Journal { exec, .. } = &self.source {
                                let args = self.journal_args(true);
                                let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                                self.stream = exec.stream("journalctl", &a).ok();
                            }
                        }
                    }
                    Err(e) => self.status = format!("{}: {e:#}", tr("Error")),
                }
                self.trim();
            }
        }
        if let Some(s) = &self.stream {
            let new = s.drain(5000);
            if !new.is_empty() {
                self.lines.extend(new);
                self.trim();
            }
        }
        if let Some(s) = &mut self.stream {
            if let Some(ok) = s.finished() {
                let rest = s.drain(usize::MAX);
                self.lines.extend(rest);
                if matches!(self.source, Source::Command { .. }) {
                    self.lines.push(format!("[{}]", if ok { "done" } else { "exited with error" }));
                    self.finished = Some(ok);
                    self.status = if ok { "done".into() } else { "failed".into() };
                }
                self.stream = None;
            }
        }
        // API follow polling
        if self.follow && self.job.is_none() && self.last_poll.elapsed() > Duration::from_secs(2) {
            self.last_poll = Instant::now();
            match self.source.clone() {
                Source::ApiJournal { api, node } => {
                    if let Some(c) = self.cursor.clone() {
                        self.job = Some(Job::spawn(move || {
                            let v = api.get(&format!("/nodes/{node}/journal?startcursor={}", crate::util::urlenc(&c)))?;
                            Ok(parse_journal(&v))
                        }));
                    }
                }
                Source::ApiSyslog { api, node, service } => {
                    if let Some(start) = self.next_start {
                        self.job = Some(Job::spawn(move || {
                            let v = api.get_full(&format!("/nodes/{node}/syslog?service={service}&start={start}&limit=500"))?;
                            let rows = v.get("data").map(jarr).unwrap_or_default();
                            let lines: Vec<String> = rows.iter().map(|r| js(r, "t")).filter(|l| l != "no content").collect();
                            Ok(Loaded { next_start: Some(start + lines.len() as u64), lines, cursor: None })
                        }));
                    }
                }
                _ => {}
            }
        }
    }

    fn trim(&mut self) {
        if self.lines.len() > MAX_LINES {
            let n = self.lines.len() - MAX_LINES;
            self.lines.drain(..n);
            self.top = self.top.saturating_sub(n);
        }
    }

    fn matches(&self, l: &str) -> bool {
        !self.search.is_empty() && l.to_lowercase().contains(&self.search.to_lowercase())
    }

    fn visible(&self) -> Vec<usize> {
        if self.filter_only && !self.search.is_empty() {
            (0..self.lines.len()).filter(|i| self.matches(&self.lines[*i])).collect()
        } else {
            (0..self.lines.len()).collect()
        }
    }

    fn jump(&mut self, forward: bool) {
        if self.search.is_empty() {
            return;
        }
        let vis = self.visible();
        let cur = self.match_idx.unwrap_or(if forward { self.top } else { self.top + self.view_h });
        let found = if forward {
            vis.iter().position(|i| *i > cur && self.matches(&self.lines[*i]))
        } else {
            vis.iter().rposition(|i| *i < cur && self.matches(&self.lines[*i]))
        };
        if let Some(p) = found {
            self.follow = false;
            self.match_idx = Some(vis[p]);
            self.top = p.saturating_sub(self.view_h / 2);
            self.status.clear();
        } else {
            self.status = if crate::i18n::is_zh() { "没有更多匹配".into() } else { "no more matches".into() };
        }
    }

    fn export(&mut self) {
        let dir = if crate::util::is_root_process() { "/root" } else { "/tmp" };
        let path = format!("{dir}/pxvirt-console-{}.log", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        let vis = self.visible();
        let text: String = vis.iter().map(|i| format!("{}\n", self.lines[*i])).collect();
        self.status = match std::fs::write(&path, text) {
            Ok(_) => format!("saved {path}"),
            Err(e) => format!("save failed: {e}"),
        };
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> Outcome {
        if self.typing {
            match key.code {
                KeyCode::Esc => {
                    self.typing = false;
                    self.search.clear();
                }
                KeyCode::Enter => {
                    self.typing = false;
                    self.match_idx = None;
                    self.jump(false);
                }
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Char(c) => self.search.push(c),
                _ => {}
            }
            return Outcome::None;
        }
        let total = self.visible().len();
        let max_top = total.saturating_sub(self.view_h);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if !self.search.is_empty() && key.code == KeyCode::Esc {
                    self.search.clear();
                    self.filter_only = false;
                } else {
                    return Outcome::Close;
                }
            }
            KeyCode::Char('/') => {
                self.typing = true;
                self.search.clear();
            }
            KeyCode::Char('n') => self.jump(true),
            KeyCode::Char('N') => self.jump(false),
            KeyCode::Char('&') => {
                self.filter_only = !self.filter_only;
                self.top = 0;
            }
            KeyCode::Char(' ') => {
                self.follow = !self.follow;
                if !self.follow {
                    self.top = max_top;
                }
            }
            KeyCode::Char('p') if self.is_journal() => {
                self.prio = (self.prio + 1) % PRIOS.len();
                self.reload();
            }
            KeyCode::Char('t') if !matches!(self.source, Source::Command { .. } | Source::Static(_) | Source::ApiSyslog { .. }) => {
                self.range = (self.range + 1) % RANGES.len();
                self.reload();
            }
            KeyCode::Char('+') => {
                self.limit = (self.limit * 4).min(MAX_LINES);
                self.reload();
            }
            KeyCode::Char('w') => self.export(),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Up | KeyCode::Char('k') => {
                if self.follow {
                    self.follow = false;
                    self.top = max_top;
                }
                self.top = self.top.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.follow {
                    self.top = (self.top + 1).min(max_top);
                }
            }
            KeyCode::PageUp => {
                if self.follow {
                    self.follow = false;
                    self.top = max_top;
                }
                self.top = self.top.saturating_sub(self.view_h);
            }
            KeyCode::PageDown => {
                if !self.follow {
                    self.top = (self.top + self.view_h).min(max_top);
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.follow = false;
                self.top = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.follow = true;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(s) = &mut self.stream {
                    s.kill();
                }
            }
            _ => {}
        }
        Outcome::None
    }

    fn line_style(l: &str) -> Style {
        let low = l.to_lowercase();
        if low.contains("error") || low.contains("fail") || low.contains("panic") || low.contains("critical") {
            style_bad()
        } else if low.contains("warn") {
            style_warn()
        } else {
            Style::default()
        }
    }

    fn highlight(&self, l: &str, base: Style) -> Line<'static> {
        if self.search.is_empty() {
            return Line::from(Span::styled(l.to_string(), base));
        }
        let low = l.to_lowercase();
        let pat = self.search.to_lowercase();
        let mut spans = Vec::new();
        let mut pos = 0;
        while let Some(i) = low[pos..].find(&pat) {
            let s = pos + i;
            let e = s + pat.len();
            if !l.is_char_boundary(s) || !l.is_char_boundary(e) {
                break;
            }
            spans.push(Span::styled(l[pos..s].to_string(), base));
            spans.push(Span::styled(l[s..e].to_string(), Style::default().bg(ratatui::style::Color::Yellow).fg(ratatui::style::Color::Black)));
            pos = e;
        }
        spans.push(Span::styled(l[pos..].to_string(), base));
        Line::from(spans)
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        crate::ui::clear(f, area);
        let parts = vsplit(area, &[Constraint::Min(3), Constraint::Length(1)]);
        let body = parts[0];
        self.view_h = body.height.saturating_sub(2) as usize;
        let vis = self.visible();
        let max_top = vis.len().saturating_sub(self.view_h);
        if self.follow {
            self.top = max_top;
        }
        self.top = self.top.min(max_top);
        let lines: Vec<Line> = vis
            .iter()
            .skip(self.top)
            .take(self.view_h)
            .map(|i| {
                let l = &self.lines[*i];
                let mut ln = self.highlight(l, Self::line_style(l));
                if Some(*i) == self.match_idx {
                    ln = ln.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                ln
            })
            .collect();
        let mut flags = Vec::new();
        flags.push(if self.follow { "FOLLOW".to_string() } else { "PAUSED".to_string() });
        if self.is_journal() && !PRIOS[self.prio].is_empty() {
            flags.push(format!("prio<={}", PRIOS[self.prio]));
        }
        if !matches!(self.source, Source::Command { .. } | Source::Static(_) | Source::ApiSyslog { .. }) {
            flags.push(RANGES[self.range].1.to_string());
        }
        if self.filter_only {
            flags.push("FILTER".into());
        }
        let title = format!(
            "{}  [{}]  {}/{}",
            self.title,
            flags.join(" "),
            (self.top + self.view_h).min(vis.len()),
            vis.len()
        );
        f.render_widget(Paragraph::new(lines).block(block(&title, true)), body);
        if !self.status.is_empty() {
            let w = (self.status.chars().count() as u16 + 2).min(body.width.saturating_sub(4));
            f.render_widget(
                Paragraph::new(Span::styled(format!(" {} ", self.status), Style::default().fg(ACCENT))),
                Rect::new(body.x + 2, body.y + body.height - 1, w, 1),
            );
        }
        let foot = if self.typing {
            Line::from(vec![Span::styled(tr("Search: "), Style::default().fg(ACCENT)), Span::raw(format!("{}_", self.search))])
        } else {
            let mut h: Vec<(&str, &str)> = vec![("Esc", "close"), ("Space", "follow"), ("/", "search"), ("n/N", "next"), ("&", "filter")];
            if self.is_journal() {
                h.push(("p", "priority"));
            }
            if !matches!(self.source, Source::Command { .. } | Source::Static(_) | Source::ApiSyslog { .. }) {
                h.push(("t", "time"));
            }
            h.extend([("+", "more"), ("w", "export"), ("r", "refresh")]);
            Line::from(hint_spans(&h))
        };
        f.render_widget(Paragraph::new(foot).style(ratatui::style::Style::default().bg(super::BAR).fg(ratatui::style::Color::White)), parts[1]);
    }
}

fn parse_journal(v: &serde_json::Value) -> Loaded {
    let arr = jarr(v);
    let mut lines = Vec::new();
    let mut cursor = None;
    for e in arr {
        if let Some(s) = e.as_str() {
            if s.starts_with("s=") && s.contains(";i=") {
                cursor = Some(s.to_string());
            } else {
                lines.push(s.to_string());
            }
        }
    }
    Loaded { lines, cursor, next_start: None }
}
