//! X11 keysyms for VNC key events.

use crossterm::event::{KeyCode, KeyModifiers};

pub const SHIFT_L: u32 = 0xffe1;
pub const CONTROL_L: u32 = 0xffe3;
pub const ALT_L: u32 = 0xffe9;
pub const DELETE: u32 = 0xffff;
pub const BACKSPACE: u32 = 0xff08;

pub fn fkey(n: u8) -> u32 {
    0xffbe + (n as u32 - 1)
}

fn char_sym(c: char) -> u32 {
    let cp = c as u32;
    if (0x20..=0xff).contains(&cp) {
        cp
    } else {
        0x0100_0000 + cp
    }
}

/// Characters that need Shift on a US keyboard (the layout QEMU assumes by default).
fn needs_shift(c: char) -> bool {
    c.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(c)
}

/// Translate a terminal key into (modifier keysyms, keysym).
pub fn from_crossterm(code: KeyCode, mods: KeyModifiers) -> Option<(Vec<u32>, u32)> {
    let mut m = Vec::new();
    if mods.contains(KeyModifiers::CONTROL) {
        m.push(CONTROL_L);
    }
    if mods.contains(KeyModifiers::ALT) {
        m.push(ALT_L);
    }
    let sym = match code {
        KeyCode::Char(c) => {
            if needs_shift(c) && !m.contains(&SHIFT_L) {
                m.push(SHIFT_L);
            }
            // Ctrl+letter arrives as lowercase letter with CONTROL
            char_sym(c)
        }
        KeyCode::Enter => 0xff0d,
        KeyCode::Backspace => BACKSPACE,
        KeyCode::Tab => 0xff09,
        KeyCode::BackTab => {
            m.push(SHIFT_L);
            0xff09
        }
        KeyCode::Esc => 0xff1b,
        KeyCode::Delete => DELETE,
        KeyCode::Insert => 0xff63,
        KeyCode::Home => 0xff50,
        KeyCode::End => 0xff57,
        KeyCode::PageUp => 0xff55,
        KeyCode::PageDown => 0xff56,
        KeyCode::Left => 0xff51,
        KeyCode::Up => 0xff52,
        KeyCode::Right => 0xff53,
        KeyCode::Down => 0xff54,
        KeyCode::F(n) if (1..=24).contains(&n) => fkey(n),
        _ => return None,
    };
    if mods.contains(KeyModifiers::SHIFT) && !m.contains(&SHIFT_L) && !matches!(code, KeyCode::Char(_)) {
        m.push(SHIFT_L);
    }
    Some((m, sym))
}

#[cfg(feature = "sdl")]
pub fn from_sdl(k: sdl2::keyboard::Keycode) -> Option<u32> {
    use sdl2::keyboard::Keycode as K;
    let raw = k.into_i32();
    // printable ASCII keycodes are their character codes
    if (0x20..0x7f).contains(&raw) {
        return Some(raw as u32);
    }
    Some(match k {
        K::RETURN | K::RETURN2 => 0xff0d,
        K::ESCAPE => 0xff1b,
        K::BACKSPACE => BACKSPACE,
        K::TAB => 0xff09,
        K::DELETE => DELETE,
        K::INSERT => 0xff63,
        K::HOME => 0xff50,
        K::END => 0xff57,
        K::PAGEUP => 0xff55,
        K::PAGEDOWN => 0xff56,
        K::LEFT => 0xff51,
        K::UP => 0xff52,
        K::RIGHT => 0xff53,
        K::DOWN => 0xff54,
        K::F1 => fkey(1),
        K::F2 => fkey(2),
        K::F3 => fkey(3),
        K::F4 => fkey(4),
        K::F5 => fkey(5),
        K::F6 => fkey(6),
        K::F7 => fkey(7),
        K::F8 => fkey(8),
        K::F9 => fkey(9),
        K::F10 => fkey(10),
        K::F11 => fkey(11),
        K::F12 => fkey(12),
        K::LSHIFT => SHIFT_L,
        K::RSHIFT => 0xffe2,
        K::LCTRL => CONTROL_L,
        K::RCTRL => 0xffe4,
        K::LALT => ALT_L,
        K::RALT => 0xffea,
        K::LGUI => 0xffeb,
        K::RGUI => 0xffec,
        K::CAPSLOCK => 0xffe5,
        K::NUMLOCKCLEAR => 0xff7f,
        K::SCROLLLOCK => 0xff14,
        K::PRINTSCREEN => 0xff61,
        K::PAUSE => 0xff13,
        K::MENU | K::APPLICATION => 0xff67,
        K::KP_ENTER => 0xff8d,
        K::KP_0 => 0xffb0,
        K::KP_1 => 0xffb1,
        K::KP_2 => 0xffb2,
        K::KP_3 => 0xffb3,
        K::KP_4 => 0xffb4,
        K::KP_5 => 0xffb5,
        K::KP_6 => 0xffb6,
        K::KP_7 => 0xffb7,
        K::KP_8 => 0xffb8,
        K::KP_9 => 0xffb9,
        K::KP_PERIOD => 0xffae,
        K::KP_PLUS => 0xffab,
        K::KP_MINUS => 0xffad,
        K::KP_MULTIPLY => 0xffaa,
        K::KP_DIVIDE => 0xffaf,
        _ => return None,
    })
}
