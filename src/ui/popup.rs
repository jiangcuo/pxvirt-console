//! Modal popups: context menu, confirmation, message, scrollable text.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::form::Form;
use super::logview::LogViewer;
use super::task::TaskView;
use super::{popup_block, centered, style_bad, style_dim, style_good, style_sel, style_warn, ACCENT};
use crate::i18n::tr;

#[derive(Clone)]
pub struct MenuItem {
    pub key: char,
    pub label: String,
    /// None = enabled; Some(reason) = shown greyed out with the reason.
    pub disabled: Option<String>,
}

pub fn item(key: char, label: &str) -> MenuItem {
    MenuItem { key, label: label.to_string(), disabled: None }
}

impl MenuItem {
    pub fn disabled_if(mut self, cond: bool, reason: &str) -> Self {
        if cond && self.disabled.is_none() {
            self.disabled = Some(reason.to_string());
        }
        self
    }
    pub fn require(self, r: Result<(), String>) -> Self {
        match r {
            Ok(()) => self,
            Err(e) => self.disabled_if(true, &e),
        }
    }
}

pub struct Menu {
    pub title: String,
    pub tag: String,
    pub items: Vec<MenuItem>,
    pub sel: usize,
}

pub enum ConfirmKind {
    YesNo,
    /// User must type this text exactly.
    Typed(String),
    /// y = keep, n = revert, no answer before the deadline = timeout.
    Countdown(std::time::Instant),
}

pub struct Confirm {
    pub title: String,
    pub tag: String,
    pub body: Vec<String>,
    pub kind: ConfirmKind,
    pub input: String,
    pub danger: bool,
}

pub struct TextBox {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: u16,
    pub error: bool,
}

pub enum Popup {
    Menu(Menu),
    Confirm(Confirm),
    Text(TextBox),
    Form(Form),
    Log(Box<LogViewer>),
    Task(Box<TaskView>),
}

pub enum Outcome {
    None,
    Close,
    /// Menu item chosen: (menu tag, key)
    Menu(String, char),
    /// Confirm accepted: tag
    Confirmed(String),
    /// Form submitted: tag + values
    Form(String, Vec<(String, String)>),
}

impl Popup {
    pub fn menu(title: &str, tag: &str, items: Vec<MenuItem>) -> Self {
        let sel = items.iter().position(|i| i.disabled.is_none()).unwrap_or(0);
        Popup::Menu(Menu { title: title.to_string(), tag: tag.to_string(), items, sel })
    }

    pub fn confirm(title: &str, tag: &str, body: Vec<String>, danger: bool) -> Self {
        Popup::Confirm(Confirm {
            title: title.to_string(),
            tag: tag.to_string(),
            body,
            kind: ConfirmKind::YesNo,
            input: String::new(),
            danger,
        })
    }

    pub fn confirm_typed(title: &str, tag: &str, body: Vec<String>, expected: &str) -> Self {
        Popup::Confirm(Confirm {
            title: title.to_string(),
            tag: tag.to_string(),
            body,
            kind: ConfirmKind::Typed(expected.to_string()),
            input: String::new(),
            danger: true,
        })
    }

    /// Keep/revert question that answers itself with "<tag>:timeout" when time runs out.
    pub fn countdown(title: &str, tag: &str, body: Vec<String>, secs: u64) -> Self {
        Popup::Confirm(Confirm {
            title: title.to_string(),
            tag: tag.to_string(),
            body,
            kind: ConfirmKind::Countdown(std::time::Instant::now() + std::time::Duration::from_secs(secs)),
            input: String::new(),
            danger: true,
        })
    }

    /// For countdown confirms: the outcome tag once the deadline passed.
    pub fn expired(&self) -> Option<String> {
        match self {
            Popup::Confirm(Confirm { kind: ConfirmKind::Countdown(d), tag, .. }) if std::time::Instant::now() >= *d => Some(format!("{tag}:timeout")),
            _ => None,
        }
    }

    pub fn text(title: &str, text: &str) -> Self {
        Popup::Text(TextBox {
            title: title.to_string(),
            lines: text.lines().map(|s| s.to_string()).collect(),
            scroll: 0,
            error: false,
        })
    }

    pub fn error(title: &str, text: &str) -> Self {
        Popup::Text(TextBox {
            title: title.to_string(),
            lines: text.lines().map(|s| s.to_string()).collect(),
            scroll: 0,
            error: true,
        })
    }

    /// Full-screen popups hide the main UI.
    pub fn fullscreen(&self) -> bool {
        matches!(self, Popup::Log(_))
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> Outcome {
        match self {
            Popup::Menu(m) => menu_key(m, key),
            Popup::Confirm(c) => confirm_key(c, key),
            Popup::Text(t) => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Outcome::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    t.scroll = t.scroll.saturating_sub(1);
                    Outcome::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    t.scroll = t.scroll.saturating_add(1).min(t.lines.len() as u16);
                    Outcome::None
                }
                KeyCode::PageUp => {
                    t.scroll = t.scroll.saturating_sub(10);
                    Outcome::None
                }
                KeyCode::PageDown => {
                    t.scroll = t.scroll.saturating_add(10).min(t.lines.len() as u16);
                    Outcome::None
                }
                _ => Outcome::None,
            },
            Popup::Form(f) => f.handle_key(key),
            Popup::Log(l) => l.handle_key(key),
            Popup::Task(t) => t.handle_key(key),
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        match self {
            Popup::Menu(m) => draw_menu(f, area, m),
            Popup::Confirm(c) => draw_confirm(f, area, c),
            Popup::Text(t) => draw_text(f, area, t),
            Popup::Form(fm) => fm.draw(f, area),
            Popup::Log(l) => l.draw(f, area),
            Popup::Task(t) => t.draw(f, area),
        }
    }

    pub fn tick(&mut self) {
        match self {
            Popup::Log(l) => l.tick(),
            Popup::Task(t) => t.tick(),
            _ => {}
        }
    }
}

fn menu_key(m: &mut Menu, key: &KeyEvent) -> Outcome {
    let n = m.items.len();
    match key.code {
        KeyCode::Esc | KeyCode::Left => Outcome::Close,
        KeyCode::Up | KeyCode::Char('k') if n > 0 && !has_key(m, 'k') => {
            m.sel = (m.sel + n - 1) % n;
            Outcome::None
        }
        KeyCode::Down | KeyCode::Char('j') if n > 0 && !has_key(m, 'j') => {
            m.sel = (m.sel + 1) % n;
            Outcome::None
        }
        KeyCode::Up if n > 0 => {
            m.sel = (m.sel + n - 1) % n;
            Outcome::None
        }
        KeyCode::Down if n > 0 => {
            m.sel = (m.sel + 1) % n;
            Outcome::None
        }
        KeyCode::Enter | KeyCode::Right => match m.items.get(m.sel) {
            Some(it) if it.disabled.is_none() => Outcome::Menu(m.tag.clone(), it.key),
            _ => Outcome::None,
        },
        KeyCode::Char(c) => match m.items.iter().position(|i| i.key == c) {
            Some(i) if m.items[i].disabled.is_none() => Outcome::Menu(m.tag.clone(), c),
            Some(i) => {
                m.sel = i;
                Outcome::None
            }
            None => Outcome::None,
        },
        _ => Outcome::None,
    }
}

fn has_key(m: &Menu, c: char) -> bool {
    m.items.iter().any(|i| i.key == c)
}

fn confirm_key(c: &mut Confirm, key: &KeyEvent) -> Outcome {
    match &c.kind {
        ConfirmKind::YesNo => match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Outcome::Close,
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => Outcome::Confirmed(c.tag.clone()),
            _ => Outcome::None,
        },
        ConfirmKind::Countdown(_) => match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => Outcome::Confirmed(format!("{}:keep", c.tag)),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Outcome::Confirmed(format!("{}:revert", c.tag)),
            _ => Outcome::None,
        },
        ConfirmKind::Typed(expected) => match key.code {
            KeyCode::Esc => Outcome::Close,
            KeyCode::Enter => {
                if &c.input == expected {
                    Outcome::Confirmed(c.tag.clone())
                } else {
                    Outcome::None
                }
            }
            KeyCode::Backspace => {
                c.input.pop();
                Outcome::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                c.input.clear();
                Outcome::None
            }
            KeyCode::Char(ch) => {
                c.input.push(ch);
                Outcome::None
            }
            _ => Outcome::None,
        },
    }
}

fn draw_menu(f: &mut Frame, area: Rect, m: &Menu) {
    let w = m
        .items
        .iter()
        .map(|i| {
            super::unicode_width(tr(&i.label)) + 7 + i.disabled.as_ref().map(|d| super::unicode_width(d) + 2).unwrap_or(0)
        })
        .max()
        .unwrap_or(20)
        .max(super::unicode_width(tr(&m.title)) + 6) as u16;
    let r = centered(w + 6, m.items.len() as u16 + 2, area);
    crate::ui::clear(f, r);
    let lines: Vec<Line> = m
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let sel = i == m.sel;
            let base = if it.disabled.is_some() { style_dim() } else { Style::default() };
            let mut spans = vec![
                Span::styled(if sel { super::SEL_MARK } else { "  " }, Style::default().fg(ACCENT)),
                Span::styled(format!("{}  ", it.key), if it.disabled.is_some() { style_dim() } else { Style::default().fg(ACCENT) }),
                Span::styled(tr(&it.label).to_string(), if sel { style_sel(true) } else { base }),
            ];
            if let Some(d) = &it.disabled {
                spans.push(Span::styled(format!("  {d}"), style_dim()));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(popup_block(&m.title)), r);
}

fn draw_confirm(f: &mut Frame, area: Rect, c: &Confirm) {
    let mut lines: Vec<Line> = c.body.iter().map(|s| Line::from(s.clone())).collect();
    lines.push(Line::from(""));
    match &c.kind {
        ConfirmKind::YesNo => {
            lines.push(Line::from(vec![
                Span::styled(" [Y] ", if c.danger { style_bad() } else { style_good() }),
                Span::raw(tr("Confirm")),
                Span::raw("    "),
                Span::styled(" [N] ", Style::default().fg(ACCENT)),
                Span::raw(tr("Cancel")),
            ]));
        }
        ConfirmKind::Countdown(deadline) => {
            let left = deadline.saturating_duration_since(std::time::Instant::now()).as_secs();
            lines.push(Line::from(Span::styled(
                format!("{} {left}s", if crate::i18n::is_zh() { "自动恢复倒计时" } else { "Reverting automatically in" }),
                style_warn().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(vec![
                Span::styled(" [Y] ", style_good()),
                Span::raw(if crate::i18n::is_zh() { "保留新配置" } else { "keep" }),
                Span::raw("    "),
                Span::styled(" [N] ", style_bad()),
                Span::raw(if crate::i18n::is_zh() { "立即恢复" } else { "revert now" }),
            ]));
        }
        ConfirmKind::Typed(expected) => {
            lines.push(Line::from(vec![
                Span::raw(if crate::i18n::is_zh() { "输入 " } else { "Type " }),
                Span::styled(expected.clone(), style_warn().add_modifier(Modifier::BOLD)),
                Span::raw(if crate::i18n::is_zh() { " 以确认：" } else { " to confirm:" }),
            ]));
            let ok = &c.input == expected;
            lines.push(Line::from(Span::styled(
                format!("> {}_", c.input),
                if ok { style_good() } else { Style::default() },
            )));
            lines.push(Line::from(Span::styled("Enter / Esc", style_dim())));
        }
    }
    let w = c.body.iter().map(|s| super::unicode_width(s)).max().unwrap_or(30).clamp(36, 90) as u16 + 4;
    let h = lines.len() as u16 + 2;
    let r = centered(w + 4, h, area);
    crate::ui::clear(f, r);
    let mut blk = popup_block(&c.title);
    if c.danger {
        blk = blk.border_style(style_bad());
    }
    f.render_widget(Paragraph::new(lines).block(blk).wrap(Wrap { trim: false }), r);
}

fn draw_text(f: &mut Frame, area: Rect, t: &TextBox) {
    let w = t.lines.iter().map(|s| super::unicode_width(s)).max().unwrap_or(30).clamp(30, (area.width as usize).saturating_sub(4)) as u16 + 4;
    let h = (t.lines.len() as u16 + 3).min(area.height.saturating_sub(2));
    let r = centered(w + 4, h, area);
    crate::ui::clear(f, r);
    let st = if t.error { style_bad() } else { Style::default() };
    let lines: Vec<Line> = t.lines.iter().map(|s| Line::from(Span::styled(s.clone(), st))).collect();
    let mut blk = popup_block(&t.title);
    if t.error {
        blk = blk.border_style(style_bad());
    }
    f.render_widget(Paragraph::new(lines).block(blk).wrap(Wrap { trim: false }).scroll((t.scroll, 0)), r);
}
