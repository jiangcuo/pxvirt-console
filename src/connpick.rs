//! Saved connections: every `--ssh` / `--remote` target is remembered in
//! ~/.config/pxvirt-console/connections.json; started without a target on a machine that is
//! not a PVE node (or with `--connect`), the console shows this list to pick from.

use std::io;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Terminal,
};
use serde::{Deserialize, Serialize};

use crate::i18n::tr;
use crate::ui::form::{self, Form};
use crate::ui::popup::Outcome;
use crate::ui::table::{col, Row, Table};
use crate::ui::{hint_spans, style_bad, style_dim, ACCENT};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conn {
    pub name: String,
    /// "ssh" or "api"
    pub kind: String,
    /// `user@host[:port]` (ssh) or `host[:port]` (api)
    pub target: String,
    #[serde(default)]
    pub last_used: i64,
}

fn path() -> Option<std::path::PathBuf> {
    crate::config::user_path("connections.json")
}

pub fn load() -> Vec<Conn> {
    path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(list: &[Conn]) -> Result<()> {
    let p = path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(list)?)?;
    Ok(())
}

/// Remember a target that was just used (new entry or updated "last used").
pub fn remember(kind: &str, target: &str) {
    let mut list = load();
    let now = chrono::Utc::now().timestamp();
    match list.iter_mut().find(|c| c.kind == kind && c.target == target) {
        Some(c) => c.last_used = now,
        None => list.push(Conn { name: default_name(target), kind: kind.into(), target: target.into(), last_used: now }),
    }
    let _ = save(&list);
}

fn default_name(target: &str) -> String {
    let host = target.rsplit('@').next().unwrap_or(target);
    host.split(':').next().unwrap_or(host).to_string()
}

/// `--list`: the saved connections as a plain table, most recently used first.
pub fn list_text(list: &[Conn]) -> String {
    if list.is_empty() {
        return "no saved connections (they are added when you use --ssh or --remote)\n".into();
    }
    let list = sorted(list.to_vec());
    let w = list.iter().map(|c| c.name.chars().count()).max().unwrap_or(4).max(4);
    let t = list.iter().map(|c| c.target.len()).max().unwrap_or(6).max(6);
    let mut out = format!("{:<w$}  TYPE  {:<t$}  LAST USED\n", "NAME", "TARGET");
    for c in &list {
        let used = if c.last_used == 0 { "-".to_string() } else { crate::util::fmt_time(c.last_used).chars().take(16).collect() };
        out.push_str(&format!("{:<w$}  {:<4}  {:<t$}  {used}\n", c.name, if c.kind == "ssh" { "ssh" } else { "api" }, c.target));
    }
    out
}

/// Saved entry by name (for `--connect NAME`).
pub fn find(name: &str) -> Option<Conn> {
    load().into_iter().find(|c| c.name == name || c.target == name)
}

pub fn valid_target(kind: &str, t: &str) -> bool {
    let ok_chars = !t.is_empty() && !t.starts_with('-') && t.chars().all(|c| c.is_ascii_alphanumeric() || "@.:-_[]".contains(c));
    ok_chars && (kind == "ssh" || !t.contains('@'))
}

fn sorted(mut v: Vec<Conn>) -> Vec<Conn> {
    v.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.name.cmp(&b.name)));
    v
}

enum Mode {
    List,
    /// editing: index of the entry (None = new)
    Edit(Option<usize>, Form),
    Delete(usize),
}

pub struct Picker {
    list: Vec<Conn>,
    table: Table,
    mode: Mode,
    msg: Option<(String, bool)>,
}

impl Picker {
    pub fn new(list: Vec<Conn>) -> Self {
        let mut p = Self { list: sorted(list), table: Table::default(), mode: Mode::List, msg: None };
        if p.list.is_empty() {
            p.mode = Mode::Edit(None, Self::form(None));
        }
        p
    }

    fn form(c: Option<&Conn>) -> Form {
        let (name, kind, target) = c.map(|c| (c.name.as_str(), c.kind.as_str(), c.target.as_str())).unwrap_or(("", "ssh", ""));
        Form::new(
            if c.is_some() { "Edit connection" } else { "New connection" },
            "conn",
            vec![
                form::select("kind", "Type", vec![("ssh".into(), "SSH (full features)".into()), ("api".into(), "PVE API only".into())], kind),
                form::text("target", "Target", target).req(true).help("ssh: root@node[:port]   api: node[:port]"),
                form::text("name", "Name", name).help("empty = host name"),
            ],
        )
    }

    fn selected(&self) -> Option<usize> {
        let k = self.table.selected()?;
        self.list.iter().position(|c| format!("{}|{}", c.kind, c.target) == k)
    }

    /// Handle a key; Some(conn) = connect, Err = quit.
    pub fn on_key(&mut self, key: &crossterm::event::KeyEvent) -> std::result::Result<Option<Conn>, ()> {
        self.msg = None;
        match &mut self.mode {
            Mode::Edit(idx, f) => {
                let idx = *idx;
                match f.handle_key(key) {
                    Outcome::Close => self.mode = Mode::List,
                    Outcome::Form(_, v) => {
                        let get = |k: &str| v.iter().find(|x| x.0 == k).map(|x| x.1.trim().to_string()).unwrap_or_default();
                        let (kind, target) = (get("kind"), get("target"));
                        if !valid_target(&kind, &target) {
                            self.msg = Some((tr("invalid target").into(), true));
                            return Ok(None);
                        }
                        let name = if get("name").is_empty() { default_name(&target) } else { get("name") };
                        let c = Conn { name, kind, target, last_used: idx.map(|i| self.list[i].last_used).unwrap_or(0) };
                        match idx {
                            Some(i) => self.list[i] = c,
                            None => self.list.push(c),
                        }
                        self.persist();
                        self.mode = Mode::List;
                    }
                    _ => {}
                }
                if self.list.is_empty() && matches!(self.mode, Mode::List) {
                    return Err(());
                }
                Ok(None)
            }
            Mode::Delete(i) => {
                let i = *i;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.list.remove(i);
                    self.persist();
                }
                self.mode = Mode::List;
                Ok(None)
            }
            Mode::List => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                    return Err(());
                }
                match key.code {
                    KeyCode::Enter => return Ok(self.selected().map(|i| self.list[i].clone())),
                    KeyCode::Char('a') | KeyCode::Char('n') => self.mode = Mode::Edit(None, Self::form(None)),
                    KeyCode::Char('e') => {
                        if let Some(i) = self.selected() {
                            self.mode = Mode::Edit(Some(i), Self::form(Some(&self.list[i])));
                        }
                    }
                    KeyCode::Char('x') | KeyCode::Delete => {
                        if let Some(i) = self.selected() {
                            self.mode = Mode::Delete(i);
                        }
                    }
                    _ => {
                        self.table.handle_key(key);
                    }
                }
                Ok(None)
            }
        }
    }

    fn persist(&mut self) {
        if let Err(e) = save(&self.list) {
            self.msg = Some((format!("{}: {e:#}", tr("cannot save")), true));
        }
    }

    pub fn draw(&mut self, f: &mut ratatui::Frame) {
        let a = f.area();
        crate::ui::clear(f, a);
        let head = Line::from(vec![Span::styled(" PXVIRT ", Style::default().fg(ACCENT)), Span::raw(format!("│ {}", tr("Connections")))]);
        f.render_widget(Paragraph::new(head), Rect::new(a.x, a.y, a.width, 1));
        let body = Rect::new(a.x, a.y + 1, a.width, a.height.saturating_sub(3));
        let rows: Vec<Row> = self
            .list
            .iter()
            .map(|c| {
                let used = if c.last_used == 0 { "-".to_string() } else { crate::util::fmt_time(c.last_used).chars().take(16).collect() };
                Row::new(format!("{}|{}", c.kind, c.target)).c(c.name.clone()).c(if c.kind == "ssh" { "SSH" } else { "API" }).c(c.target.clone()).cs(used, style_dim())
            })
            .collect();
        let cols = [col("Name", Constraint::Length(22)), col("Type", Constraint::Length(5)), col("Target", Constraint::Min(24)), col("Last used", Constraint::Length(16))];
        let status = rows.is_empty().then(|| (tr("No saved connections: press a to add one").to_string(), style_dim()));
        self.table.draw(f, body, "Connections", &cols, &rows, matches!(self.mode, Mode::List), status);
        let hints: Vec<(&str, &str)> = match self.mode {
            Mode::Delete(_) => vec![("y", "delete"), ("n", "keep")],
            _ => vec![("Enter", "connect"), ("a", "add"), ("e", "edit"), ("x", "delete"), ("q", "quit")],
        };
        let mut line = hint_spans(&hints);
        if let Mode::Delete(i) = self.mode {
            line.insert(0, Span::styled(format!("{} {}?  ", tr("Delete"), self.list[i].name), style_bad()));
        }
        if let Some((m, err)) = &self.msg {
            line.insert(0, Span::styled(format!("{m}  "), if *err { style_bad() } else { Style::default() }));
        }
        f.render_widget(Paragraph::new(Line::from(line)), Rect::new(a.x, a.y + a.height.saturating_sub(1), a.width, 1));
        if let Mode::Edit(_, form) = &self.mode {
            form.draw(f, body);
        }
    }
}

/// Show the list until the user picks a connection (None = quit).
pub fn run() -> Result<Option<Conn>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut t = Terminal::new(CrosstermBackend::new(out))?;
    let mut p = Picker::new(load());
    let res = loop {
        t.draw(|f| p.draw(f))?;
        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match p.on_key(&k) {
                Ok(Some(c)) => break Some(c),
                Ok(None) => {}
                Err(()) => break None,
            }
        }
    };
    disable_raw_mode()?;
    execute!(t.backend_mut(), LeaveAlternateScreen)?;
    t.show_cursor()?;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::from(c)
    }

    #[test]
    fn targets_and_names() {
        assert!(valid_target("ssh", "root@10.0.0.1:2222") && valid_target("api", "pve1:8006") && valid_target("ssh", "[fd00::1]"));
        assert!(!valid_target("api", "root@pve1") && !valid_target("ssh", "-oProxyCommand=x") && !valid_target("ssh", "a b") && !valid_target("ssh", ""));
        assert_eq!(default_name("root@pve1.lan:22"), "pve1.lan");
    }

    #[test]
    fn list_output() {
        let l = vec![Conn { name: "a".into(), kind: "api".into(), target: "pve0".into(), last_used: 0 }, Conn { name: "node1".into(), kind: "ssh".into(), target: "root@10.0.0.1".into(), last_used: 5 }];
        let t = list_text(&l);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "NAME   TYPE  TARGET         LAST USED");
        assert!(lines[1].starts_with("node1  ssh   root@10.0.0.1  1970-01-01"));
        assert_eq!(lines[2], "a      api   pve0           -");
        assert!(list_text(&[]).starts_with("no saved connections"));
    }

    #[test]
    fn picker_flow() {
        let list = vec![
            Conn { name: "old".into(), kind: "api".into(), target: "pve0".into(), last_used: 10 },
            Conn { name: "node1".into(), kind: "ssh".into(), target: "root@192.168.18.11".into(), last_used: 20 },
        ];
        let mut p = Picker::new(list);
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        t.draw(|f| p.draw(f)).unwrap();
        let b = t.backend().buffer().clone();
        let text: String = (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect();
        assert!(text.contains("root@192.168.18.11") && text.contains("1970-01-01"));
        // most recent first
        assert_eq!(p.on_key(&key(KeyCode::Enter)).unwrap().map(|c| c.name), Some("node1".into()));
        assert!(p.on_key(&key(KeyCode::Char('q'))).is_err());
        // empty list starts with the "new connection" form; Esc there quits
        let mut p = Picker::new(vec![]);
        assert!(matches!(p.mode, Mode::Edit(None, _)));
        assert!(p.on_key(&key(KeyCode::Esc)).is_err());
    }
}
