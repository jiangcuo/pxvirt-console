//! Clickable action strip shown at the bottom of the graphical console.
//!
//! Every key goes to the guest except one reserved combination that opens the
//! console menu (SDL: Ctrl+Alt+Shift+Q, terminal: Ctrl+]). Actions (Ctrl+Alt+Del,
//! special keys, close...) are buttons on the bar or entries of that menu, usable
//! with the mouse or, in the menu, with the keyboard. The pointer over the bar and
//! the menu is not forwarded to the guest.

use crate::i18n::tr;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    CtrlAltDel,
    CtrlAltBackspace,
    CtrlAltF(u8),
    F12,
    Esc,
    /// Open / close the special-key menu.
    Menu,
    /// Cycle the terminal image protocol (terminal renderer only).
    Protocol,
    /// Ask the server for a full frame.
    Refresh,
    Close,
}

impl Act {
    /// English label; also the i18n lookup key.
    pub fn label(self) -> &'static str {
        match self {
            Act::CtrlAltDel => "Ctrl+Alt+Del",
            Act::CtrlAltBackspace => "Ctrl+Alt+Backspace",
            Act::CtrlAltF(1) => "Ctrl+Alt+F1",
            Act::CtrlAltF(2) => "Ctrl+Alt+F2",
            Act::CtrlAltF(3) => "Ctrl+Alt+F3",
            Act::CtrlAltF(_) => "Ctrl+Alt+F7",
            Act::F12 => "F12",
            Act::Esc => "Esc",
            Act::Menu => "Keys",
            Act::Protocol => "Image protocol",
            Act::Refresh => "Refresh",
            Act::Close => "Close",
        }
    }

    /// Label with the padding the bar draws around it.
    pub fn text(self) -> String {
        format!(" {} ", tr(self.label()))
    }

    /// Untranslated label (the SDL bitmap font is ASCII only).
    #[cfg(feature = "sdl")]
    pub fn text_en(self) -> String {
        format!(" {} ", self.label())
    }
}

/// Buttons shown on the bar.
pub const BAR: [Act; 6] = [Act::CtrlAltDel, Act::Esc, Act::F12, Act::Menu, Act::Refresh, Act::Close];

/// Special-key menu of the terminal renderer.
pub const MENU_TERM: [Act; 10] = [
    Act::CtrlAltBackspace,
    Act::CtrlAltF(1),
    Act::CtrlAltF(2),
    Act::CtrlAltF(3),
    Act::CtrlAltF(7),
    Act::F12,
    Act::Esc,
    Act::Protocol,
    Act::Refresh,
    Act::Close,
];

/// Special-key menu of the SDL renderer (no image-protocol switching).
#[cfg(feature = "sdl")]
pub const MENU_SDL: [Act; 9] = [
    Act::CtrlAltBackspace,
    Act::CtrlAltF(1),
    Act::CtrlAltF(2),
    Act::CtrlAltF(3),
    Act::CtrlAltF(7),
    Act::F12,
    Act::Esc,
    Act::Refresh,
    Act::Close,
];

/// Keysyms for actions that inject keys into the guest: (modifiers, keysym).
pub fn keysyms(a: Act) -> Option<(&'static [u32], u32)> {
    use super::keys::{fkey, ALT_L, BACKSPACE, CONTROL_L, DELETE};
    let mods: &'static [u32] = &[CONTROL_L, ALT_L];
    match a {
        Act::CtrlAltDel => Some((mods, DELETE)),
        Act::CtrlAltBackspace => Some((mods, BACKSPACE)),
        Act::CtrlAltF(n) => Some((mods, fkey(n))),
        Act::F12 => Some((&[], fkey(12))),
        Act::Esc => Some((&[], 0xff1b)),
        _ => None,
    }
}

/// A clickable area in renderer units (terminal cells or screen pixels).
#[derive(Clone, Copy, Debug)]
pub struct Button {
    pub act: Act,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Button {
    pub fn new(act: Act, x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { act, x, y, w, h }
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && y >= self.y && x < self.x.saturating_add(self.w) && y < self.y.saturating_add(self.h)
    }
}

/// Lay buttons out left to right starting at `x0`, separated by `gap`.
/// `width` returns the size of a label in the same unit as the rect (cells or pixels).
/// Buttons that do not fit in `max_w` are dropped.
pub fn layout(acts: &[Act], x0: u16, y: u16, h: u16, max_w: u16, gap: u16, width: impl Fn(&str) -> u16) -> Vec<Button> {
    let mut out = Vec::new();
    let mut x = x0;
    for a in acts {
        let w = width(&a.text()).max(1);
        if x.saturating_add(w) > max_w {
            break;
        }
        out.push(Button::new(*a, x, y, w, h));
        x = x.saturating_add(w).saturating_add(gap);
    }
    out
}

pub fn hit(buttons: &[Button], x: u16, y: u16) -> Option<Act> {
    buttons.iter().find(|b| b.contains(x, y)).map(|b| b.act)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_stops_at_edge() {
        let b = layout(&BAR, 0, 23, 1, 40, 1, |s| s.chars().count() as u16);
        assert_eq!(b[0].act, Act::CtrlAltDel);
        assert!(b.windows(2).all(|w| w[0].x + w[0].w < w[1].x));
        assert!(b.iter().all(|b| b.x + b.w <= 40));
    }

    #[test]
    fn hit_finds_button() {
        let b = layout(&BAR, 0, 23, 1, 200, 1, |s| s.chars().count() as u16);
        let r = b[1];
        assert_eq!(hit(&b, r.x + 1, r.y), Some(r.act));
        assert_eq!(hit(&b, 200, 23), None);
    }
}
