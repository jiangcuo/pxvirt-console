//! Full-screen VNC viewer with SDL2. On the physical console (no X/Wayland) SDL uses
//! KMSDRM and reads the keyboard/mouse through evdev.
//!
//! The console never grabs a key combination: every key goes to the guest. Actions
//! (Ctrl+Alt+Del, special keys, close) are buttons on a status bar drawn at the bottom
//! of the screen (and in a pop-up menu), clicked with the mouse.

use anyhow::{anyhow, Result};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Keycode, Mod};
use sdl2::mouse::MouseButton;
use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::Rect;
use std::time::Duration;

use super::bar::{self, Act, Button};
use super::keys;
use super::rfb::{snapshot_rgb, Client};

/// Status bar height in pixels (the 8x8 font is drawn at 2x).
const BAR_H: u32 = 24;
const SCALE: u32 = 2;

/// True if the kernel exposes a pointer (relative mouse or absolute touch device).
/// Without one the status bar cannot be clicked, so the emergency exit chord stays.
fn has_mouse() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/input") else { return true };
    for e in dir.flatten() {
        if !e.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        let caps = e.path().join("device/capabilities");
        for file in ["rel", "abs"] {
            let Ok(s) = std::fs::read_to_string(caps.join(file)) else { continue };
            // hex words, most significant first
            let mut words: Vec<u64> = s.split_whitespace().filter_map(|w| u64::from_str_radix(w, 16).ok()).collect();
            words.reverse();
            // REL_X/REL_Y or ABS_X/ABS_Y
            if words.first().copied().unwrap_or(0) & 0b11 == 0b11 {
                return true;
            }
        }
    }
    false
}

pub fn on_physical_console() -> bool {
    std::env::var("SSH_CONNECTION").is_err()
        && std::env::var("DISPLAY").is_err()
        && std::env::var("WAYLAND_DISPLAY").is_err()
        && std::env::var("TERM").map(|t| t == "linux").unwrap_or(false)
        && std::path::Path::new("/dev/dri").exists()
}

fn fit(out: (u32, u32), fb: (u32, u32)) -> (Rect, f64) {
    let s = (out.0 as f64 / fb.0.max(1) as f64).min(out.1 as f64 / fb.1.max(1) as f64);
    let w = (fb.0 as f64 * s) as u32;
    let h = (fb.1 as f64 * s) as u32;
    (Rect::new(((out.0 - w) / 2) as i32, ((out.1 - h) / 2) as i32, w.max(1), h.max(1)), s)
}

fn text_width(s: &str) -> u32 {
    (s.chars().count() as u32) * 8 * SCALE
}

/// Draw `text` with the 8x8 bitmap font; bit 0 of each row is the leftmost pixel.
fn draw_text(c: &mut sdl2::render::Canvas<sdl2::video::Window>, x: i32, y: i32, text: &str, color: Color) {
    let _ = c.set_draw_color(color);
    let mut rects = Vec::new();
    let mut px = x;
    for ch in text.chars() {
        let ch = if (ch as u32) < 127 { ch } else { '?' };
        let glyph = &font8x8::legacy::BASIC_LEGACY[ch as usize];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8u32 {
                if bits & (1 << col) != 0 {
                    rects.push(Rect::new(px + (col * SCALE) as i32, y + (row as u32 * SCALE) as i32, SCALE, SCALE));
                }
            }
        }
        px += (8 * SCALE) as i32;
    }
    let _ = c.fill_rects(&rects);
}

pub fn run(client: &mut Client, title: &str) -> Result<()> {
    let windowed = std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok();
    if !windowed {
        std::env::set_var("SDL_VIDEODRIVER", "kmsdrm");
    }
    let sdl = sdl2::init().map_err(|e| anyhow!("SDL init: {e}"))?;
    let video = sdl.video().map_err(|e| anyhow!("SDL video: {e}"))?;
    let (fw, fh) = {
        let f = client.fb.lock().unwrap();
        (f.w as u32, f.h as u32)
    };
    let mut wb = video.window(&format!("{title} - click Close on the status bar"), fw.max(640), fh.max(480));
    if windowed {
        wb.resizable();
    } else {
        wb.fullscreen_desktop();
    }
    let window = wb.build().map_err(|e| anyhow!("SDL window: {e}"))?;
    let mut canvas = match window.into_canvas().accelerated().build() {
        Ok(c) => c,
        Err(_) => {
            let w = video.window(title, fw, fh).fullscreen_desktop().build().map_err(|e| anyhow!("{e}"))?;
            w.into_canvas().software().build().map_err(|e| anyhow!("SDL renderer: {e}"))?
        }
    };
    sdl.mouse().show_cursor(false);
    let tc = canvas.texture_creator();
    let mut tex_size = (fw, fh);
    let mut tex = tc.create_texture_streaming(PixelFormatEnum::RGB24, fw.max(1), fh.max(1)).map_err(|e| anyhow!("{e}"))?;
    let mut pump = sdl.event_pump().map_err(|e| anyhow!("{e}"))?;
    let mut last_gen = u64::MAX;
    let mut dst = Rect::new(0, 0, fw.max(1), fh.max(1));
    let mut scale = 1.0;
    let mut buttons: u8 = 0;
    let mut bar_buttons: Vec<Button> = Vec::new();
    let mut menu_rows: Vec<Button> = Vec::new();
    let mut menu = false;
    let mut hover: Option<Act> = None;
    let mut dirty = true;
    // No pointer on this machine: keep Ctrl+Alt+Q as the only way out.
    let no_mouse = !has_mouse();

    loop {
        let out = canvas.output_size().map_err(|e| anyhow!("{e}"))?;
        let bar_top = out.1.saturating_sub(BAR_H);
        for ev in pump.poll_iter() {
            match ev {
                Event::Quit { .. } => return Ok(()),
                Event::KeyDown { keycode: Some(k), keymod, .. } => {
                    if no_mouse && k == Keycode::Q && keymod.contains(Mod::LCTRLMOD | Mod::LALTMOD) {
                        return Ok(());
                    }
                    // Everything else is forwarded; no combination is reserved for the console.
                    if let Some(sym) = keys::from_sdl(k) {
                        client.key(true, sym);
                    }
                }
                Event::KeyUp { keycode: Some(k), .. } => {
                    if let Some(sym) = keys::from_sdl(k) {
                        client.key(false, sym);
                    }
                }
                Event::MouseMotion { x, y, .. } => {
                    let act = bar::hit(&menu_rows, x.max(0) as u16, y.max(0) as u16)
                        .or_else(|| bar::hit(&bar_buttons, x.max(0) as u16, y.max(0) as u16));
                    if act != hover {
                        hover = act;
                        dirty = true;
                    }
                    // the status bar and the menu are ours: never move the guest pointer under them
                    // (except while a guest button is held, so a drag does not get stuck)
                    if act.is_some() || menu || (buttons == 0 && y >= 0 && y as u32 >= bar_top) {
                        continue;
                    }
                    let (px, py) = map(x, y, dst, scale, tex_size);
                    client.pointer_event(px, py, buttons);
                }
                Event::MouseButtonDown { mouse_btn, x, y, .. } | Event::MouseButtonUp { mouse_btn, x, y, .. } => {
                    let down = matches!(ev, Event::MouseButtonDown { .. });
                    let act = bar::hit(&menu_rows, x.max(0) as u16, y.max(0) as u16)
                        .or_else(|| bar::hit(&bar_buttons, x.max(0) as u16, y.max(0) as u16));
                    if let Some(a) = act {
                        if down {
                            match a {
                                Act::Menu => {
                                    menu = !menu;
                                    hover = None;
                                }
                                Act::Refresh => client.request_full(),
                                Act::Close => return Ok(()),
                                a => {
                                    if let Some((mods, sym)) = bar::keysyms(a) {
                                        client.tap(mods, sym);
                                    }
                                    menu = false;
                                }
                            }
                            dirty = true;
                            last_gen = u64::MAX;
                        }
                        continue;
                    }
                    if menu {
                        // click outside the menu just closes it
                        if down {
                            menu = false;
                            dirty = true;
                        }
                        continue;
                    }
                    if buttons == 0 && y >= 0 && y as u32 >= bar_top {
                        continue; // empty part of the bar
                    }
                    let bit = match mouse_btn {
                        MouseButton::Left => 1,
                        MouseButton::Middle => 2,
                        MouseButton::Right => 4,
                        _ => 0,
                    };
                    buttons = if down { buttons | bit } else { buttons & !bit };
                    let (px, py) = map(x, y, dst, scale, tex_size);
                    client.pointer_event(px, py, buttons);
                }
                Event::MouseWheel { y, .. } => {
                    if menu || hover.is_some() {
                        continue;
                    }
                    let (px, py) = client.pointer;
                    let b = if y > 0 { 8 } else { 16 };
                    client.pointer_event(px, py, buttons | b);
                    client.pointer_event(px, py, buttons);
                }
                Event::Window { win_event: WindowEvent::SizeChanged(..), .. } | Event::Window { win_event: WindowEvent::Exposed, .. } => last_gen = u64::MAX,
                _ => {}
            }
        }

        let (gen, closed) = {
            let f = client.fb.lock().unwrap();
            (f.gen, f.closed.clone())
        };
        if let Some(reason) = closed {
            return Err(anyhow!("console closed: {reason}"));
        }
        if gen != last_gen || dirty {
            dirty = false;
            last_gen = gen;
            let (w, h, rgb) = {
                let f = client.fb.lock().unwrap();
                snapshot_rgb(&f, client.pointer)
            };
            let (w, h) = (w as u32, h as u32);
            if (w, h) != tex_size && w > 0 && h > 0 {
                tex = tc.create_texture_streaming(PixelFormatEnum::RGB24, w, h).map_err(|e| anyhow!("{e}"))?;
                tex_size = (w, h);
            }
            tex.update(None, &rgb, (w * 3) as usize).map_err(|e| anyhow!("{e}"))?;
            let (d, s) = fit((out.0, bar_top), tex_size);
            dst = d;
            scale = s;
            canvas.set_draw_color(Color::RGB(0, 0, 0));
            canvas.clear();
            canvas.copy(&tex, None, Some(dst)).map_err(|e| anyhow!("{e}"))?;

            // status bar
            let mut bb = Vec::new();
            let mut x = 4i32;
            for a in &bar::BAR {
                let label = a.text();
                let w = text_width(&label);
                if x + w as i32 > out.0 as i32 {
                    break;
                }
                bb.push(Button::new(*a, x as u16, bar_top as u16, w as u16, BAR_H as u16));
                x += w as i32 + 8;
            }
            bar_buttons = bb;
            let used = bar_buttons.last().map(|b| (b.x as u32) + (b.w as u32)).unwrap_or(0);
            let _ = canvas.set_draw_color(Color::RGB(0, 0, 128));
            let _ = canvas.fill_rect(Rect::new(0, bar_top as i32, out.0, BAR_H));
            for b in &bar_buttons {
                let sel = hover == Some(b.act);
                if sel {
                    let _ = canvas.set_draw_color(Color::RGB(255, 255, 255));
                    let _ = canvas.fill_rect(sdl_rect(*b));
                }
                let fg = if sel { Color::RGB(0, 0, 128) } else { Color::RGB(255, 255, 255) };
                draw_text(&mut canvas, b.x as i32, b.y as i32 + 4, &b.act.text(), fg);
            }
            if used + 8 < out.0 {
                let info = if no_mouse {
                    format!("{title}  {w}x{h}  no mouse: Ctrl+Alt+Q closes")
                } else {
                    format!("{title}  {w}x{h}")
                };
                let avail = (out.0 - used - 8) / (8 * SCALE);
                let s: String = info.chars().take(avail as usize).collect();
                draw_text(&mut canvas, (used + 8) as i32, bar_top as i32 + 4, &s, Color::RGB(200, 200, 200));
            }
            // special-key menu
            menu_rows.clear();
            if menu {
                let w = text_width(" Ctrl+Alt+Backspace ") + 16;
                let h = bar::MENU_SDL.len() as u32 * BAR_H + 8;
                let x = (out.0.saturating_sub(w) / 2) as i32;
                let y = ((bar_top.saturating_sub(h)) / 2) as i32;
                let r = Rect::new(x, y, w, h);
                let _ = canvas.set_draw_color(Color::RGB(0, 0, 0));
                let _ = canvas.fill_rect(r);
                for (i, a) in bar::MENU_SDL.iter().enumerate() {
                    let br = Button::new(*a, (x + 8) as u16, (y + 4 + (i as u32 * BAR_H) as i32) as u16, w as u16 - 16, BAR_H as u16);
                    menu_rows.push(br);
                    if hover == Some(*a) {
                        let _ = canvas.set_draw_color(Color::RGB(255, 255, 255));
                        let _ = canvas.fill_rect(sdl_rect(br));
                    }
                    let fg = if hover == Some(*a) { Color::RGB(0, 0, 0) } else { Color::RGB(255, 255, 255) };
                    draw_text(&mut canvas, br.x as i32, br.y as i32 + 4, &a.text(), fg);
                }
            }
            canvas.present();
        }
        std::thread::sleep(Duration::from_millis(8));
    }
}

fn sdl_rect(b: Button) -> Rect {
    Rect::new(b.x as i32, b.y as i32, b.w as u32, b.h as u32)
}

fn map(x: i32, y: i32, dst: Rect, scale: f64, fb: (u32, u32)) -> (u16, u16) {
    let fx = ((x - dst.x()) as f64 / scale).clamp(0.0, fb.0.saturating_sub(1) as f64);
    let fy = ((y - dst.y()) as f64 / scale).clamp(0.0, fb.1.saturating_sub(1) as f64);
    (fx as u16, fy as u16)
}
