//! Selectable, filterable, multi-select table used by every list view.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Cell, HighlightSpacing, Paragraph, Row as TRow, Table as TTable, TableState},
    Frame,
};
use std::collections::HashSet;

use super::{block, style_dim, ACCENT, SEL_MARK};
use crate::i18n::tr;

#[derive(Clone)]
pub struct Row {
    pub key: String,
    pub cells: Vec<(String, Style)>,
    /// Dim the whole row (e.g. stopped guests, disabled items).
    pub dim: bool,
}

impl Row {
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into(), cells: Vec::new(), dim: false }
    }
    pub fn c(mut self, s: impl Into<String>) -> Self {
        self.cells.push((s.into(), Style::default()));
        self
    }
    pub fn cs(mut self, s: impl Into<String>, st: Style) -> Self {
        self.cells.push((s.into(), st));
        self
    }
    pub fn dim(mut self, d: bool) -> Self {
        self.dim = d;
        self
    }
    fn text(&self) -> String {
        self.cells.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(" ").to_lowercase()
    }
}

pub struct Col {
    pub title: String,
    pub width: Constraint,
}

pub fn col(title: &str, width: Constraint) -> Col {
    Col { title: tr(title).to_string(), width }
}

#[derive(Default)]
pub struct Table {
    pub filter: String,
    pub marked: HashSet<String>,
    sel_key: Option<String>,
    sel: usize,
    keys: Vec<String>,
    page: usize,
    pub multi: bool,
}

impl Table {
    /// Selected row key (from the last draw).
    pub fn selected(&self) -> Option<&str> {
        self.keys.get(self.sel).map(|s| s.as_str())
    }

    /// Marked keys, or the selected one when nothing is marked.
    pub fn targets(&self) -> Vec<String> {
        if !self.marked.is_empty() {
            let mut v: Vec<String> = self.keys.iter().filter(|k| self.marked.contains(*k)).cloned().collect();
            v.extend(self.marked.iter().filter(|k| !self.keys.contains(k)).cloned());
            v
        } else {
            self.selected().map(|s| vec![s.to_string()]).unwrap_or_default()
        }
    }

    fn apply(&mut self, rows: &[Row]) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        let vis: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| f.is_empty() || f.split_whitespace().all(|w| r.text().contains(w)))
            .map(|(i, _)| i)
            .collect();
        self.keys = vis.iter().map(|i| rows[*i].key.clone()).collect();
        if let Some(k) = &self.sel_key {
            if let Some(i) = self.keys.iter().position(|x| x == k) {
                self.sel = i;
            }
        }
        if self.sel >= self.keys.len() {
            self.sel = self.keys.len().saturating_sub(1);
        }
        self.sel_key = self.keys.get(self.sel).cloned();
        vis
    }

    fn set_sel(&mut self, i: usize) {
        self.sel = i.min(self.keys.len().saturating_sub(1));
        self.sel_key = self.keys.get(self.sel).cloned();
    }

    /// Navigation keys. Returns true when handled.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let n = self.keys.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.set_sel(self.sel.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if n > 0 {
                    self.set_sel((self.sel + 1).min(n - 1));
                }
            }
            KeyCode::PageUp => self.set_sel(self.sel.saturating_sub(self.page.max(1))),
            KeyCode::PageDown => self.set_sel(self.sel + self.page.max(1)),
            KeyCode::Home => self.set_sel(0),
            KeyCode::End | KeyCode::Char('G') => self.set_sel(n.saturating_sub(1)),
            KeyCode::Char(' ') if self.multi => {
                if let Some(k) = self.selected().map(|s| s.to_string()) {
                    if !self.marked.remove(&k) {
                        self.marked.insert(k);
                    }
                    if n > 0 {
                        self.set_sel((self.sel + 1).min(n - 1));
                    }
                }
            }
            _ => return false,
        }
        true
    }

    pub fn draw(
        &mut self,
        f: &mut Frame,
        area: Rect,
        title: &str,
        cols: &[Col],
        rows: &[Row],
        focused: bool,
        status: Option<(String, Style)>,
    ) {
        let vis = self.apply(rows);
        let mut t = tr(title).to_string();
        if !self.filter.is_empty() {
            t = format!("{t} [/{}] {}/{}", self.filter, vis.len(), rows.len());
        } else if !rows.is_empty() {
            t = format!("{t} ({})", rows.len());
        }
        if !self.marked.is_empty() {
            t = format!("{t} *{}", self.marked.len());
        }
        if let (Some((msg, _)), false) = (&status, rows.is_empty()) {
            t = format!("{t} · {msg}");
        }
        let blk = block(&t, focused);
        if rows.is_empty() || vis.is_empty() {
            let (msg, st) = status.unwrap_or_else(|| (tr("No data").to_string(), style_dim()));
            f.render_widget(Paragraph::new(Line::from(Span::styled(msg, st))).block(blk), area);
            return;
        }
        // drop right-most columns that do not fit (narrow terminals)
        let avail = area.width.saturating_sub(4) as usize;
        let minw = |c: &Constraint| match c {
            Constraint::Length(n) | Constraint::Min(n) => *n as usize,
            Constraint::Percentage(_) | Constraint::Ratio(_, _) | Constraint::Fill(_) => 4,
            Constraint::Max(n) => (*n as usize).min(8),
        };
        let mut ncols = cols.len();
        while ncols > 2 {
            let need: usize = cols[..ncols].iter().map(|c| minw(&c.width) + 2).sum::<usize>() + 2 + if self.multi { 3 } else { 0 };
            if need <= avail {
                break;
            }
            ncols -= 1;
        }
        let mut widths: Vec<Constraint> = Vec::new();
        if self.multi {
            widths.push(Constraint::Length(1));
        }
        widths.extend(cols[..ncols].iter().map(|c| c.width));
        let mut header_cells: Vec<Cell> = Vec::new();
        if self.multi {
            header_cells.push(Cell::from(""));
        }
        header_cells.extend(cols[..ncols].iter().map(|c| Cell::from(c.title.clone())));
        let header = TRow::new(header_cells).style(Style::default().fg(Color::Gray)).bottom_margin(0);
        let trows: Vec<TRow> = vis
            .iter()
            .map(|i| {
                let r = &rows[*i];
                let mut cells: Vec<Cell> = Vec::new();
                if self.multi {
                    cells.push(Cell::from(if self.marked.contains(&r.key) { "*" } else { " " }));
                }
                cells.extend(r.cells.iter().take(ncols).map(|(s, st)| {
                    let st = if r.dim && st.fg.is_none() { style_dim() } else { *st };
                    Cell::from(Span::styled(s.clone(), st))
                }));
                TRow::new(cells)
            })
            .collect();
        self.page = area.height.saturating_sub(4) as usize;
        let mut state = TableState::default();
        state.select(Some(self.sel));
        let table = TTable::new(trows, widths)
            .header(header)
            .block(blk)
            .row_highlight_style(super::style_sel(focused))
            .highlight_symbol(Text::from(Span::styled(SEL_MARK, if focused { Style::default().fg(ACCENT) } else { style_dim() })))
            .highlight_spacing(HighlightSpacing::Always)
            .column_spacing(2);
        f.render_stateful_widget(table, area, &mut state);
    }
}
