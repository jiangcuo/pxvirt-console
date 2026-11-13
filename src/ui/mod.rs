//! Shared UI pieces: theme, blocks, key/value lists, sparklines, layout helpers.

pub mod form;
pub mod logview;
pub mod popup;
pub mod table;
pub mod task;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};

use crate::i18n::tr;

// Colors limited to the 16 the Linux VT can show.
pub const ACCENT: Color = Color::Cyan;
pub const GOOD: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const BAD: Color = Color::Red;
pub const DIM: Color = Color::DarkGray;
pub const LABEL: Color = Color::Gray;
pub const RULE: Color = Color::DarkGray;
/// The console paints its own background so it looks the same on light or dark terminals.
pub const BG: Color = Color::Black;
pub const FG: Color = Color::White;
/// Top/bottom bars.
pub const BAR: Color = Color::Blue;

pub fn base() -> Style {
    Style::default().bg(BG).fg(FG)
}

/// Clear an area to the console background (use instead of `Clear`).
pub fn clear(f: &mut Frame, area: Rect) {
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(Block::default().style(base()), area);
}

pub fn style_dim() -> Style {
    Style::default().fg(DIM)
}

pub fn style_label() -> Style {
    Style::default().fg(LABEL)
}

pub fn style_good() -> Style {
    Style::default().fg(GOOD)
}

pub fn style_warn() -> Style {
    Style::default().fg(WARN)
}

pub fn style_bad() -> Style {
    Style::default().fg(BAD)
}

/// Selected row / item: blue bar when focused, bold text otherwise.
pub fn style_sel(focused: bool) -> Style {
    if focused {
        Style::default().bg(BAR).fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    }
}

/// Marker drawn in front of the selected row.
pub const SEL_MARK: &str = "▸ ";

/// Framed pane; the focused pane gets a cyan frame and title.
pub fn block<'a>(title: &str, focused: bool) -> Block<'a> {
    let t = tr(title);
    let title_style = if focused { Style::default().fg(ACCENT).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Gray) };
    let mut b = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(if focused { ACCENT } else { RULE }))
        .style(base())
        .padding(Padding::new(1, 1, 0, 0));
    if !t.is_empty() {
        b = b.title(Span::styled(format!(" {t} "), title_style));
    }
    b
}

/// Framed box for popups, menus and forms (they float over the content).
pub fn popup_block<'a>(title: &str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(base())
        .padding(Padding::new(1, 1, 0, 0))
        .title(Span::styled(format!(" {} ", tr(title)), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)))
}

/// Color for a status word (running/stopped/online/ok/error...).
pub fn status_style(s: &str) -> Style {
    let l = s.to_ascii_lowercase();
    if ["running", "online", "ok", "active", "up", "passed", "health_ok", "available", "enabled", "started", "yes"]
        .iter()
        .any(|w| l == *w || l.starts_with(w))
    {
        style_good()
    } else if ["stopped", "offline", "error", "failed", "dead", "down", "health_err", "faulted", "unavailable", "fenced"]
        .iter()
        .any(|w| l == *w || l.starts_with(w))
    {
        style_bad()
    } else if l.is_empty() || l == "-" || l == "unknown" || l == "inactive" {
        style_dim()
    } else {
        style_warn()
    }
}

pub fn usage_style(frac: f64) -> Style {
    if frac >= 0.9 {
        style_bad()
    } else if frac >= 0.75 {
        style_warn()
    } else {
        style_good()
    }
}

/// Pad to a display width (CJK characters count as 2 columns).
pub fn pad(s: &str, w: usize) -> String {
    let sw = unicode_width(s);
    if sw >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - sw))
    }
}

/// Key/value rows; values may carry a style.
pub fn kv_lines(items: &[(String, String, Style)], label_w: usize) -> Vec<Line<'static>> {
    items
        .iter()
        .map(|(k, v, st)| {
            if k.is_empty() && v.is_empty() {
                Line::from("")
            } else if v.is_empty() && !k.is_empty() && k.starts_with('#') {
                Line::from(Span::styled(
                    k.trim_start_matches('#').to_string(),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("{} ", pad(tr(k), label_w)), style_label()),
                    Span::styled(v.clone(), *st),
                ])
            }
        })
        .collect()
}

/// Builder for key/value detail panes.
#[derive(Default)]
pub struct Kv {
    pub items: Vec<(String, String, Style)>,
}

impl Kv {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&mut self, k: &str, v: impl Into<String>) -> &mut Self {
        self.items.push((k.to_string(), v.into(), Style::default()));
        self
    }
    pub fn styled(&mut self, k: &str, v: impl Into<String>, st: Style) -> &mut Self {
        self.items.push((k.to_string(), v.into(), st));
        self
    }
    pub fn status(&mut self, k: &str, v: impl Into<String>) -> &mut Self {
        let v = v.into();
        let st = status_style(&v);
        self.items.push((k.to_string(), v, st));
        self
    }
    pub fn section(&mut self, title: &str) -> &mut Self {
        if !self.items.is_empty() {
            self.items.push((String::new(), String::new(), Style::default()));
        }
        self.items.push((format!("#{}", tr(title)), String::new(), Style::default()));
        self
    }
    pub fn blank(&mut self) -> &mut Self {
        self.items.push((String::new(), String::new(), Style::default()));
        self
    }
    pub fn lines(&self) -> Vec<Line<'static>> {
        let w = self
            .items
            .iter()
            .filter(|(k, _, _)| !k.starts_with('#'))
            .map(|(k, _, _)| unicode_width(tr(k)))
            .max()
            .unwrap_or(8)
            .min(22);
        kv_lines(&self.items, w)
    }
}

pub fn unicode_width(s: &str) -> usize {
    use unicode_width_shim::width;
    width(s)
}

mod unicode_width_shim {
    // ratatui re-exports unicode-width through its text layer; use Span width for consistency.
    pub fn width(s: &str) -> usize {
        ratatui::text::Span::raw(s).width()
    }
}

/// Paragraph with scroll inside a block.
pub fn draw_lines(f: &mut Frame, area: Rect, title: &str, focused: bool, lines: Vec<Line<'static>>, scroll: u16) {
    let p = Paragraph::new(lines)
        .block(block(title, focused))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

pub fn draw_message_in(f: &mut Frame, area: Rect, title: &str, focused: bool, msg: &str, style: Style) {
    let p = Paragraph::new(Line::from(Span::styled(msg.to_string(), style)))
        .block(block(title, focused))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// Unicode sparkline of the last `width` values (scaled to max).
pub fn sparkline(values: &[f64], width: usize, max: Option<f64>) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if width == 0 {
        return String::new();
    }
    let start = values.len().saturating_sub(width);
    let slice = &values[start..];
    let m = max.unwrap_or_else(|| slice.iter().cloned().fold(0.0, f64::max)).max(1e-9);
    let mut s: String = " ".repeat(width.saturating_sub(slice.len()));
    for v in slice {
        let i = ((v / m) * 7.0).round().clamp(0.0, 7.0) as usize;
        s.push(BARS[i]);
    }
    s
}

pub fn centered(width: u16, height: u16, r: Rect) -> Rect {
    let w = width.min(r.width.saturating_sub(2)).max(10.min(r.width));
    let h = height.min(r.height.saturating_sub(2)).max(3.min(r.height));
    Rect {
        x: r.x + (r.width.saturating_sub(w)) / 2,
        y: r.y + (r.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

pub fn hsplit(area: Rect, constraints: &[Constraint]) -> Vec<Rect> {
    Layout::default().direction(Direction::Horizontal).constraints(constraints.to_vec()).split(area).to_vec()
}

pub fn vsplit(area: Rect, constraints: &[Constraint]) -> Vec<Rect> {
    Layout::default().direction(Direction::Vertical).constraints(constraints.to_vec()).split(area).to_vec()
}

/// Hint line spans: `key label  key label ...`
pub fn hint_spans(hints: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut v = Vec::new();
    for (i, (k, l)) in hints.iter().enumerate() {
        v.push(Span::styled(if i == 0 { format!(" {k}") } else { format!("  {k}") }, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
        v.push(Span::styled(format!(" {}", tr(l)), Style::default().fg(Color::White)));
    }
    v
}

pub fn loading_or_error(err: &Option<String>, loading: bool) -> Option<(String, Style)> {
    if let Some(e) = err {
        Some((format!("{}: {e}", tr("Error")), style_bad()))
    } else if loading {
        Some((tr("Loading...").to_string(), style_dim()))
    } else {
        None
    }
}
