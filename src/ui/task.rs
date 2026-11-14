//! Live PVE task log (UPID) popup. Uploads show their transfer progress first and then
//! follow the task PVE starts for the uploaded file.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::popup::Outcome;
use super::{popup_block, centered, hint_spans, status_style, style_bad, style_dim, vsplit};
use crate::api::{Api, UploadProgress};
use crate::loader::Job;
use crate::util::{jarr, js};

struct Upload {
    job: Job<serde_json::Value>,
    prog: Arc<UploadProgress>,
    started: Instant,
    file: String,
}

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
    upload: Option<Upload>,
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
            upload: None,
        };
        t.tick();
        t
    }

    /// A running upload; becomes a normal task view once PVE returns the task id.
    pub fn upload(api: Arc<Api>, node: &str, title: &str, file: &str, job: Job<serde_json::Value>, prog: Arc<UploadProgress>) -> Self {
        let mut t = Self::new(api, node, "", title);
        t.upload = Some(Upload { job, prog, started: Instant::now(), file: file.to_string() });
        t
    }

    pub fn ok(&self) -> bool {
        self.exit == "OK"
    }

    pub fn tick(&mut self) {
        if let Some(u) = &self.upload {
            let Some(r) = u.job.poll() else { return };
            let secs = u.started.elapsed().as_secs_f64().max(0.1);
            let sent = u.prog.sent.load(Ordering::Relaxed) as f64;
            self.lines.push(format!("upload of {} finished: {} in {:.0}s ({}/s)", u.file, crate::util::fmt_bytes(sent), secs, crate::util::fmt_bytes(sent / secs)));
            self.upload = None;
            match r {
                Ok(v) => match crate::api::is_upid(&v) {
                    Some(upid) => self.upid = upid,
                    None => {
                        self.done = true;
                        self.exit = "OK".into();
                        self.status = "stopped".into();
                    }
                },
                Err(e) => {
                    self.lines.push(format!("ERROR: {e:#}"));
                    self.done = true;
                    self.exit = format!("{e:#}");
                    self.status = "stopped".into();
                }
            }
            return;
        }
        if self.upid.is_empty() {
            return;
        }
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
            KeyCode::Char('x') if self.upload.is_some() && !self.stop_requested => {
                self.stop_requested = true;
                if let Some(u) = &self.upload {
                    u.prog.cancel.store(true, Ordering::Relaxed);
                }
                self.lines.push("[cancel requested]".into());
            }
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
        if let Some(u) = &self.upload {
            let total = u.prog.total.load(Ordering::Relaxed) as f64;
            let sent = u.prog.sent.load(Ordering::Relaxed) as f64;
            let secs = u.started.elapsed().as_secs_f64().max(0.1);
            let rate = sent / secs;
            let frac = if total > 0.0 { (sent / total).min(1.0) } else { 0.0 };
            let eta = if rate > 0.0 && total > sent { crate::util::fmt_duration(((total - sent) / rate) as u64) } else { "-".into() };
            let barw = (parts[0].width as usize).saturating_sub(14).min(80);
            let mut lines = vec![
                Line::from(Span::styled(u.file.clone(), Style::default().fg(super::ACCENT))),
                Line::from(""),
                Line::from(format!("{} {:>5.1}%", crate::util::bar(frac, barw), frac * 100.0)),
                Line::from(format!(
                    "{} / {}   {}/s   {} {eta}",
                    crate::util::fmt_bytes(sent),
                    crate::util::fmt_bytes(total),
                    crate::util::fmt_bytes(rate),
                    crate::i18n::tr("remaining")
                )),
            ];
            if sent >= total && total > 0.0 {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(crate::i18n::tr("waiting for the node to store the file..."), style_dim())));
            }
            for l in &self.lines {
                lines.push(Line::from(l.clone()));
            }
            f.render_widget(Paragraph::new(lines).block(popup_block(&format!("{} [{}]", self.title, crate::i18n::tr("uploading")))), parts[0]);
            let hints = vec![("Esc", "continue in background"), ("x", "cancel upload")];
            f.render_widget(Paragraph::new(Line::from(hint_spans(&hints))).style(super::base()), parts[1]);
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_phase_shows_progress_then_result() {
        let prog = Arc::new(UploadProgress::default());
        prog.total.store(200 << 20, Ordering::Relaxed);
        prog.sent.store(50 << 20, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let job = Job::spawn(move || {
            let _ = rx.recv();
            Ok(serde_json::Value::Null)
        });
        let mut t = TaskView::upload(Arc::new(Api::pvesh()), "n1", "Upload a.iso", "/tmp/a.iso", job, prog);
        let render = |t: &mut TaskView| {
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
            term.draw(|f| t.draw(f, f.area())).unwrap();
            let b = term.backend().buffer().clone();
            (0..20).map(|y| (0..100).map(|x| b[(x, y)].symbol().to_string()).collect::<String>()).collect::<Vec<_>>().join("\n")
        };
        let out = render(&mut t);
        assert!(out.contains("25.0%"), "{out}");
        assert!(out.contains("/tmp/a.iso"), "{out}");
        assert!(!t.done);
        // the upload returns no task id: finished
        tx.send(()).unwrap();
        for _ in 0..100 {
            t.tick();
            if t.done {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(t.done && t.ok());
    }
}
