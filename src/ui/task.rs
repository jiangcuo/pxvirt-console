//! Live PVE task log (UPID) popup.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::popup::Outcome;
use super::{popup_block, centered, hint_spans, status_style, style_bad, style_dim, vsplit};
use crate::api::Api;
use crate::loader::Job;
use crate::util::{jarr, js};

struct Poll {
    lines: Vec<String>,
    status: String,
    exit: String,
}

pub struct TaskView {
    pub title: String,
    pub node: String,
    pub upid: String,
    api: Arc<Api>,
    lines: Vec<String>,
    status: String,
    exit: String,
    job: Option<Job<Poll>>,
    last: Instant,
    scroll_back: usize,
    pub done: bool,
    stop_requested: bool,
}

impl TaskView {
    pub fn new(api: Arc<Api>, node: &str, upid: &str, title: &str) -> Self {
        let mut t = Self {
            title: title.to_string(),
            node: node.to_string(),
            upid: upid.to_string(),
            api,
            lines: Vec::new(),
            status: "running".into(),
            exit: String::new(),
            job: None,
            last: Instant::now() - Duration::from_secs(10),
            scroll_back: 0,
            done: false,
            stop_requested: false,
        };
        t.tick();
        t
    }

    pub fn ok(&self) -> bool {
        self.exit == "OK"
    }

    pub fn tick(&mut self) {
        if let Some(j) = &self.job {
            if let Some(r) = j.poll() {
                self.job = None;
                match r {
                    Ok(p) => {
                        self.lines.extend(p.lines);
                        self.status = p.status;
                        self.exit = p.exit;
                        if self.status == "stopped" {
                            self.done = true;
                        }
                    }
                    Err(e) => self.lines.push(format!("error: {e:#}")),
                }
            }
            return;
        }
        if self.done || self.last.elapsed() < Duration::from_millis(1000) {
            return;
        }
        self.last = Instant::now();
        let api = self.api.clone();
        let base = format!("/nodes/{}/tasks/{}", self.node, crate::util::urlenc(&self.upid));
        let start = self.lines.len();
        self.job = Some(Job::spawn(move || {
            let st = api.get(&format!("{base}/status"))?;
            let log = api.get(&format!("{base}/log?start={start}&limit=1000"))?;
            let lines: Vec<String> = jarr(&log).iter().map(|l| js(l, "t")).filter(|l| l != "no content").collect();
            Ok(Poll { lines, status: js(&st, "status"), exit: js(&st, "exitstatus") })
        }));
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => return Outcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_back = (self.scroll_back + 1).min(self.lines.len()),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_back = self.scroll_back.saturating_sub(1),
            KeyCode::PageUp => self.scroll_back = (self.scroll_back + 10).min(self.lines.len()),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
            KeyCode::Char('x') if !self.done && !self.stop_requested => {
                self.stop_requested = true;
                let api = self.api.clone();
                let path = format!("/nodes/{}/tasks/{}", self.node, crate::util::urlenc(&self.upid));
                std::thread::spawn(move || {
                    let _ = api.delete(&path, &[]);
                });
                self.lines.push("[stop requested]".into());
            }
            _ => {}
        }
        Outcome::None
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let w = area.width.saturating_sub(8).min(120);
        let h = area.height.saturating_sub(4).min(30);
        let r = centered(w, h, area);
        crate::ui::clear(f, r);
        let parts = vsplit(r, &[Constraint::Min(3), Constraint::Length(1)]);
        let body_h = parts[0].height.saturating_sub(2) as usize;
        let end = self.lines.len().saturating_sub(self.scroll_back);
        let start = end.saturating_sub(body_h);
        let lines: Vec<Line> = self.lines[start..end]
            .iter()
            .map(|l| {
                let st = if l.contains("ERROR") || l.contains("error") { style_bad() } else { Style::default() };
                Line::from(Span::styled(l.clone(), st))
            })
            .collect();
        let state = if self.done {
            if self.exit.is_empty() { "stopped".to_string() } else { self.exit.clone() }
        } else {
            "running...".to_string()
        };
        let title = format!("{} [{}]", self.title, state);
        let mut blk = popup_block(&title);
        if self.done {
            blk = blk.border_style(if self.ok() { status_style("ok") } else { style_bad() });
        }
        f.render_widget(Paragraph::new(lines).block(blk), parts[0]);
        let mut hints = vec![("Esc", "close"), ("PgUp/PgDn", "scroll")];
        if !self.done {
            hints.push(("x", "stop task"));
        }
        let mut spans = hint_spans(&hints);
        spans.push(Span::styled(format!("  {}", crate::util::truncate(&self.upid, 60)), style_dim()));
        f.render_widget(Paragraph::new(Line::from(spans)).style(super::base()), parts[1]);
    }
}
