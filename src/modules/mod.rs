//! Top-level modules (System, Network, Disks, Services & Logs, PVE) and the View trait.

pub mod disks;
pub mod guestcfg;
pub mod hardware;
pub mod memory;
pub mod netmgmt;
pub mod network;
pub mod overview;
pub mod pve;
pub mod processes;
pub mod pve_create;
pub mod services;
pub mod system;
pub mod updates;

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

use crate::ctx::Ctx;
use crate::ui::popup::MenuItem;
use crate::ui::table::Table;

pub trait View {
    /// Submenu label (English; translated at draw time).
    fn title(&self) -> &'static str;

    /// Start (re)loading data for the current node.
    fn refresh(&mut self, ctx: &mut Ctx);

    /// Poll background loaders; return true if something changed.
    fn poll(&mut self, _ctx: &mut Ctx) -> bool {
        false
    }

    /// Auto-refresh interval in seconds (None = manual only).
    fn refresh_secs(&self) -> Option<u64> {
        Some(10)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, focused: bool);

    fn draw_detail(&mut self, _f: &mut Frame, _area: Rect, _ctx: &Ctx, _focused: bool) {}

    fn has_detail(&self) -> bool {
        true
    }

    /// Optional extra column on very wide terminals; return false if not provided.
    fn draw_extra(&mut self, _f: &mut Frame, _area: Rect, _ctx: &Ctx) -> bool {
        false
    }

    /// View specific keys (checked before menu hotkeys and table navigation).
    fn on_key(&mut self, _key: &KeyEvent, _ctx: &mut Ctx) -> bool {
        false
    }

    fn table(&mut self) -> Option<&mut Table> {
        None
    }

    /// Context menu for the current selection. Item keys double as direct hotkeys.
    fn menu(&self, _ctx: &Ctx) -> Vec<MenuItem> {
        Vec::new()
    }

    fn menu_title(&self) -> String {
        crate::i18n::tr("Actions").to_string()
    }

    /// A menu item was chosen (tag "ctx" for the context menu).
    fn on_menu(&mut self, _tag: &str, _key: char, _ctx: &mut Ctx) {}

    fn on_confirm(&mut self, _tag: &str, _ctx: &mut Ctx) {}

    fn on_form(&mut self, _tag: &str, _values: &[(String, String)], _ctx: &mut Ctx) {}

    /// Extra hint entries for the bottom line.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    fn scroll_detail(&mut self, _delta: i32) {}

    /// Node context changed: drop cached data.
    fn reset(&mut self) {}

    /// The page only works with host commands (local or SSH mode); hidden in API-only remote mode.
    fn needs_host(&self) -> bool {
        false
    }
}

pub struct Module {
    pub title: &'static str,
    pub views: Vec<Box<dyn View>>,
    pub sel: usize,
}

pub fn build_modules() -> Vec<Module> {
    vec![
        Module { title: "System", views: system::views(), sel: 0 },
        Module { title: "Network", views: network::views(), sel: 0 },
        Module { title: "Disks", views: disks::views(), sel: 0 },
        Module { title: "Services & Logs", views: services::views(), sel: 0 },
        Module { title: "PVE", views: pve::views(), sel: 0 },
    ]
}

/// Helper: value of a form field.
pub fn fv<'a>(values: &'a [(String, String)], key: &str) -> &'a str {
    values.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or("")
}

/// Detail scroll helper used by most views.
pub fn scroll(cur: &mut u16, delta: i32) {
    *cur = (*cur as i32 + delta).max(0) as u16;
}
