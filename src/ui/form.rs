//! Simple modal forms (text, password, number, select, checkbox).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::popup::Outcome;
use super::{popup_block, centered, style_bad, style_dim, style_label, style_sel, ACCENT};
use crate::i18n::tr;

#[derive(Clone)]
pub enum Kind {
    Text,
    Password,
    Number,
    Select(Vec<(String, String)>),
    /// Several choices; the value is the chosen values separated by spaces.
    Multi(Vec<(String, String)>),
    Check,
    Info,
}

#[derive(Clone)]
pub struct Field {
    pub key: String,
    pub label: String,
    pub kind: Kind,
    pub value: String,
    pub help: String,
    pub required: bool,
}

pub fn text(key: &str, label: &str, value: &str) -> Field {
    Field { key: key.into(), label: label.into(), kind: Kind::Text, value: value.into(), help: String::new(), required: false }
}
pub fn password(key: &str, label: &str) -> Field {
    Field { key: key.into(), label: label.into(), kind: Kind::Password, value: String::new(), help: String::new(), required: true }
}
pub fn number(key: &str, label: &str, value: &str) -> Field {
    Field { key: key.into(), label: label.into(), kind: Kind::Number, value: value.into(), help: String::new(), required: true }
}
pub fn select(key: &str, label: &str, options: Vec<(String, String)>, value: &str) -> Field {
    let v = if options.iter().any(|(o, _)| o == value) {
        value.to_string()
    } else {
        options.first().map(|o| o.0.clone()).unwrap_or_default()
    };
    Field { key: key.into(), label: label.into(), kind: Kind::Select(options), value: v, help: String::new(), required: false }
}
/// Multiple choice (Space toggles in the picker); value = "a b c".
pub fn multi(key: &str, label: &str, options: Vec<(String, String)>, value: &str) -> Field {
    Field { key: key.into(), label: label.into(), kind: Kind::Multi(options), value: value.split_whitespace().collect::<Vec<_>>().join(" "), help: String::new(), required: false }
}

pub fn check(key: &str, label: &str, on: bool) -> Field {
    Field {
        key: key.into(),
        label: label.into(),
        kind: Kind::Check,
        value: if on { "1".into() } else { "0".into() },
        help: String::new(),
        required: false,
    }
}
pub fn info(label: &str, value: &str) -> Field {
    Field { key: String::new(), label: label.into(), kind: Kind::Info, value: value.into(), help: String::new(), required: false }
}

/// Options from plain strings (value == label).
pub fn opts<S: AsRef<str>>(v: &[S]) -> Vec<(String, String)> {
    v.iter().map(|s| (s.as_ref().to_string(), s.as_ref().to_string())).collect()
}

impl Field {
    pub fn req(mut self, r: bool) -> Self {
        self.required = r;
        self
    }
    pub fn help(mut self, h: &str) -> Self {
        self.help = h.into();
        self
    }
}

struct Picker {
    field: usize,
    filter: String,
    sel: usize,
}

pub struct Form {
    pub title: String,
    pub tag: String,
    pub fields: Vec<Field>,
    sel: usize,
    error: Option<String>,
    picker: Option<Picker>,
}

impl Form {
    pub fn new(title: &str, tag: &str, fields: Vec<Field>) -> Self {
        let mut f = Self { title: title.into(), tag: tag.into(), fields, sel: 0, error: None, picker: None };
        f.sel = f.next_editable(0, true);
        f
    }

    fn editable(&self, i: usize) -> bool {
        i == self.fields.len() || !matches!(self.fields[i].kind, Kind::Info)
    }

    fn next_editable(&self, from: usize, forward: bool) -> usize {
        let n = self.fields.len() + 1; // + submit button
        let mut i = from % n;
        for _ in 0..n {
            if self.editable(i) {
                return i;
            }
            i = if forward { (i + 1) % n } else { (i + n - 1) % n };
        }
        0
    }

    fn validate(&self) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        for f in &self.fields {
            if matches!(f.kind, Kind::Info) {
                continue;
            }
            if f.required && f.value.trim().is_empty() {
                return Err(format!("{}: {}", tr(&f.label), if crate::i18n::is_zh() { "必填" } else { "required" }));
            }
            if matches!(f.kind, Kind::Number) && !f.value.is_empty() && f.value.trim().parse::<f64>().is_err() {
                return Err(format!("{}: {}", tr(&f.label), if crate::i18n::is_zh() { "需要数字" } else { "must be a number" }));
            }
            out.push((f.key.clone(), f.value.trim().to_string()));
        }
        Ok(out)
    }

    fn submit(&mut self) -> Outcome {
        match self.validate() {
            Ok(v) => Outcome::Form(self.tag.clone(), v),
            Err(e) => {
                self.error = Some(e);
                Outcome::None
            }
        }
    }

    fn picker_options(&self, p: &Picker) -> Vec<(String, String)> {
        let opts = match &self.fields[p.field].kind {
            Kind::Select(o) | Kind::Multi(o) => o,
            _ => return vec![],
        };
        let f = p.filter.to_lowercase();
        opts.iter()
            .filter(|(v, l)| f.is_empty() || v.to_lowercase().contains(&f) || l.to_lowercase().contains(&f))
            .cloned()
            .collect()
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> Outcome {
        if let Some(mut p) = self.picker.take() {
            let opts = self.picker_options(&p);
            let is_multi = matches!(self.fields[p.field].kind, Kind::Multi(_));
            if is_multi {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => return Outcome::None,
                    KeyCode::Char(' ') => {
                        if let Some((v, _)) = opts.get(p.sel) {
                            let f = &mut self.fields[p.field];
                            let mut chosen: Vec<String> = f.value.split_whitespace().map(|s| s.to_string()).collect();
                            if let Some(i) = chosen.iter().position(|c| c == v) {
                                chosen.remove(i);
                            } else {
                                chosen.push(v.clone());
                            }
                            f.value = chosen.join(" ");
                        }
                        self.picker = Some(p);
                        return Outcome::None;
                    }
                    _ => {}
                }
            }
            match key.code {
                KeyCode::Esc => return Outcome::None,
                KeyCode::Enter => {
                    if let Some((v, _)) = opts.get(p.sel) {
                        self.fields[p.field].value = v.clone();
                    }
                    return Outcome::None;
                }
                KeyCode::Up => p.sel = p.sel.saturating_sub(1),
                KeyCode::Down => p.sel = (p.sel + 1).min(opts.len().saturating_sub(1)),
                KeyCode::PageUp => p.sel = p.sel.saturating_sub(10),
                KeyCode::PageDown => p.sel = (p.sel + 10).min(opts.len().saturating_sub(1)),
                KeyCode::Backspace => {
                    p.filter.pop();
                    p.sel = 0;
                }
                KeyCode::Char(c) => {
                    p.filter.push(c);
                    p.sel = 0;
                }
                _ => {}
            }
            self.picker = Some(p);
            return Outcome::None;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return self.submit();
        }
        let n = self.fields.len();
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Up | KeyCode::BackTab => {
                self.sel = self.next_editable(self.sel + n, false);
                return Outcome::None;
            }
            KeyCode::Down | KeyCode::Tab => {
                self.sel = self.next_editable(self.sel + 1, true);
                return Outcome::None;
            }
            _ => {}
        }
        if self.sel >= n {
            // submit button
            if key.code == KeyCode::Enter {
                return self.submit();
            }
            return Outcome::None;
        }
        self.error = None;
        let sel = self.sel;
        let field = &mut self.fields[sel];
        match (&field.kind, key.code) {
            (Kind::Select(opts), KeyCode::Left) | (Kind::Select(opts), KeyCode::Right) => {
                if !opts.is_empty() {
                    let i = opts.iter().position(|(v, _)| *v == field.value).unwrap_or(0);
                    let j = if key.code == KeyCode::Left { (i + opts.len() - 1) % opts.len() } else { (i + 1) % opts.len() };
                    field.value = opts[j].0.clone();
                }
            }
            (Kind::Select(opts), KeyCode::Enter) | (Kind::Select(opts), KeyCode::Char(' ')) => {
                let sel = opts.iter().position(|(v, _)| *v == field.value).unwrap_or(0);
                self.picker = Some(Picker { field: self.sel, filter: String::new(), sel });
            }
            (Kind::Multi(_), KeyCode::Enter) | (Kind::Multi(_), KeyCode::Char(' ')) => {
                self.picker = Some(Picker { field: self.sel, filter: String::new(), sel: 0 });
            }
            (Kind::Check, KeyCode::Char(' ')) | (Kind::Check, KeyCode::Left) | (Kind::Check, KeyCode::Right) => {
                field.value = if field.value == "1" { "0".into() } else { "1".into() };
            }
            (Kind::Text | Kind::Password | Kind::Number, KeyCode::Char('u')) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                field.value.clear();
            }
            (Kind::Text | Kind::Password | Kind::Number, KeyCode::Char(c)) => field.value.push(c),
            (Kind::Text | Kind::Password | Kind::Number, KeyCode::Backspace) => {
                field.value.pop();
            }
            (_, KeyCode::Enter) => {
                self.sel = self.next_editable(self.sel + 1, true);
            }
            _ => {}
        }
        Outcome::None
    }

    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let lw = self.fields.iter().map(|x| super::unicode_width(tr(&x.label))).max().unwrap_or(10).min(24);
        let mut lines: Vec<Line> = Vec::new();
        let mut sel_line = 0;
        for (i, fd) in self.fields.iter().enumerate() {
            let focused = i == self.sel;
            if focused {
                sel_line = lines.len();
            }
            let label = Span::styled(format!("{} ", super::pad(tr(&fd.label), lw)), if focused { Style::default().fg(ACCENT).add_modifier(Modifier::BOLD) } else { style_label() });
            let val = match &fd.kind {
                Kind::Password => "*".repeat(fd.value.chars().count()),
                Kind::Check => if fd.value == "1" { "[x]".into() } else { "[ ]".into() },
                Kind::Select(opts) => {
                    let l = opts.iter().find(|(v, _)| *v == fd.value).map(|(_, l)| l.clone()).unwrap_or(fd.value.clone());
                    format!("< {l} >")
                }
                Kind::Multi(_) => {
                    if fd.value.is_empty() {
                        format!("< ({}) >", tr("none"))
                    } else {
                        format!("< {} >", fd.value)
                    }
                }
                _ => fd.value.clone(),
            };
            let cursor = if focused && matches!(fd.kind, Kind::Text | Kind::Password | Kind::Number) { "_" } else { "" };
            let vstyle = if focused { style_sel(true) } else if matches!(fd.kind, Kind::Info) { style_dim() } else { Style::default() };
            let mut spans = vec![label, Span::styled(format!("{val}{cursor}"), vstyle)];
            if fd.required && !matches!(fd.kind, Kind::Info | Kind::Check) {
                spans.push(Span::styled(" *", style_dim()));
            }
            lines.push(Line::from(spans));
            if focused && !fd.help.is_empty() {
                lines.push(Line::from(Span::styled(format!("{:lw$} {}", "", tr(&fd.help)), style_dim())));
            }
        }
        lines.push(Line::from(""));
        let submit_focus = self.sel >= self.fields.len();
        lines.push(Line::from(vec![
            Span::raw(format!("{:lw$} ", "")),
            Span::styled(format!("[ {} ]", tr("OK")), if submit_focus { style_sel(true) } else { Style::default().fg(ACCENT) }),
            Span::styled(format!("   Ctrl-S {} · Esc {}", tr("submit"), tr("Cancel")), style_dim()),
        ]));
        if let Some(e) = &self.error {
            lines.push(Line::from(Span::styled(e.clone(), style_bad())));
        }
        let w = (lw as u16 + 46).min(area.width.saturating_sub(4));
        let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
        let r = centered(w, h, area);
        let scroll = (sel_line as u16).saturating_sub(r.height.saturating_sub(4));
        crate::ui::clear(f, r);
        f.render_widget(Paragraph::new(lines).block(popup_block(&self.title)).scroll((scroll, 0)), r);

        if let Some(p) = &self.picker {
            let opts = self.picker_options(p);
            let pw = opts.iter().map(|(_, l)| super::unicode_width(l) + 4).max().unwrap_or(20).clamp(36, 80) as u16 + 4;
            let ph = (opts.len() as u16 + 3).clamp(5, area.height.saturating_sub(4));
            let pr = centered(pw, ph, area);
            crate::ui::clear(f, pr);
            let visible = ph.saturating_sub(3) as usize;
            let start = p.sel.saturating_sub(visible.saturating_sub(1));
            let multi_val = match &self.fields[p.field].kind {
                Kind::Multi(_) => Some(self.fields[p.field].value.split_whitespace().map(|s| s.to_string()).collect::<Vec<_>>()),
                _ => None,
            };
            let head = if multi_val.is_some() { format!("/{}_   Space {} · Enter {}", p.filter, tr("select"), tr("OK")) } else { format!("/{}_", p.filter) };
            let mut pl = vec![Line::from(Span::styled(head, Style::default().fg(ACCENT)))];
            for (i, (v, l)) in opts.iter().enumerate().skip(start).take(visible) {
                let text = match &multi_val {
                    Some(chosen) => format!("{} {l}", if chosen.contains(v) { "[x]" } else { "[ ]" }),
                    None => l.clone(),
                };
                pl.push(Line::from(Span::styled(text, if i == p.sel { style_sel(true) } else { Style::default() })));
            }
            f.render_widget(Paragraph::new(pl).block(popup_block(&self.fields[p.field].label)), pr);
        }
    }
}
