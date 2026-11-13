//! VNC viewer inside the terminal: Kitty graphics (own zlib encoder), Sixel / iTerm2 /
//! Unicode half blocks (ratatui-image). Keyboard and mouse are forwarded to the guest.
//!
//! Every key goes to the guest except Ctrl+], which opens the console menu (works without
//! a mouse: Up/Down, Enter, Esc). `Ctrl+Alt+Del`, special keys and closing are also
//! buttons at the bottom of the screen.

use anyhow::Result;
use base64::Engine;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use image::{DynamicImage, RgbImage};
use ratatui::{
    layout::{Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    Image, Resize,
};
use std::io::Write;
use std::time::{Duration, Instant};

use super::bar::{self, Act, Button};
use super::keys;
use super::rfb::{snapshot_rgb, Client};
use crate::ui::{block, centered, unicode_width};
use crate::Term;

/// Two image ids used alternately (double buffering): the new frame is shown first and
/// only then the previous one deleted, so the picture never blanks between frames.
const KITTY_IDS: [u32; 2] = [31, 32];

#[derive(Clone, Copy, PartialEq)]
enum Proto {
    Kitty,
    Lib(ProtocolType),
}

struct Geometry {
    area: Rect,
    font: (u16, u16),
    scale: f64,
    cols: u16,
    rows: u16,
}

fn geometry(area: Rect, font: (u16, u16), fb: (usize, usize)) -> Geometry {
    let (fw, fh) = (font.0.max(1) as f64, font.1.max(1) as f64);
    let (w, h) = (fb.0.max(1) as f64, fb.1.max(1) as f64);
    let scale = ((area.width as f64 * fw) / w).min((area.height as f64 * fh) / h);
    let cols = ((w * scale / fw).ceil() as u16).clamp(1, area.width.max(1));
    let rows = ((h * scale / fh).ceil() as u16).clamp(1, area.height.max(1));
    Geometry { area, font, scale, cols, rows }
}

fn kitty_frame(out: &mut impl Write, g: &Geometry, img: &RgbImage, id: u32) -> std::io::Result<()> {
    // send at most the displayed pixel size
    let tw = ((g.cols as u32) * g.font.0 as u32).min(img.width()).max(1);
    let th = ((g.rows as u32) * g.font.1 as u32).min(img.height()).max(1);
    let scaled;
    let img = if tw < img.width() || th < img.height() {
        scaled = image::imageops::resize(img, tw, th, image::imageops::FilterType::Triangle);
        &scaled
    } else {
        img
    };
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(img.as_raw())?;
    let data = z.finish()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    write!(out, "\x1b[{};{}H", g.area.y + 1, g.area.x + 1)?;
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
    for (i, c) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        if i == 0 {
            write!(
                out,
                "\x1b_Ga=T,f=24,o=z,s={},v={},i={id},p=1,q=2,C=1,c={},r={},m={more};",
                img.width(),
                img.height(),
                g.cols,
                g.rows
            )?;
        } else {
            write!(out, "\x1b_Gm={more};")?;
        }
        out.write_all(c)?;
        write!(out, "\x1b\\")?;
    }
    out.flush()
}

/// Delete (and free) one frame image.
fn kitty_delete_id(out: &mut impl Write, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
    let _ = out.flush();
}

fn kitty_delete(out: &mut impl Write) {
    for id in KITTY_IDS {
        kitty_delete_id(out, id);
    }
}

/// Terminals known to implement SGR-Pixels mouse reporting (DEC mode 1016).
/// `PXVIRT_PIXEL_MOUSE=0/1` overrides the detection.
fn pixel_mouse_supported(kitty_graphics: bool) -> bool {
    if let Ok(v) = std::env::var("PXVIRT_PIXEL_MOUSE") {
        return v == "1";
    }
    let term = std::env::var("TERM").unwrap_or_default();
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    kitty_graphics
        || std::env::var("KITTY_WINDOW_ID").is_ok()
        || std::env::var("WEZTERM_EXECUTABLE").is_ok()
        || matches!(prog.as_str(), "ghostty" | "wezterm")
        || term.contains("kitty")
        || term.contains("ghostty")
        || term.starts_with("foot")
}

fn bar_buttons(acts: &[Act], x0: u16, y: u16, width: u16) -> Vec<Button> {
    bar::layout(acts, x0, y, 1, width, 1, |s| unicode_width(s) as u16)
}

fn menu_rows(rect: Rect) -> Vec<Button> {
    let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width.saturating_sub(2), rect.height.saturating_sub(2));
    bar::MENU_TERM
        .iter()
        .enumerate()
        .take(inner.height as usize)
        .map(|(i, a)| Button::new(*a, inner.x, inner.y + i as u16, inner.width, 1))
        .collect()
}

pub fn run(terminal: &mut Term, client: &mut Client, title: &str, fps: u32) -> Result<()> {
    let queried = Picker::from_query_stdio();
    let font_known = queried.is_ok();
    let mut picker = queried.unwrap_or_else(|_| Picker::halfblocks());
    let mut proto = match picker.protocol_type() {
        ProtocolType::Kitty => Proto::Kitty,
        p => Proto::Lib(p),
    };
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let mut stdout = std::io::stdout();
    execute!(stdout, EnableMouseCapture)?;
    // SGR-Pixels (mode 1016): mouse positions in pixels instead of cells -> exact clicks
    let pixel_mouse = font_known && pixel_mouse_supported(picker.protocol_type() == ProtocolType::Kitty);
    if pixel_mouse {
        write!(stdout, "\x1b[?1016h")?;
        stdout.flush()?;
    }
    if enhanced {
        let _ = execute!(stdout, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    terminal.clear()?;

    // keyboard selection in the menu
    let mut sel: usize = 0;
    let interval = Duration::from_millis(1000 / fps.clamp(1, 60) as u64);
    let mut last_gen = u64::MAX;
    // which of KITTY_IDS holds the frame on screen
    let mut kitty_slot = 0usize;
    let mut kitty_shown = false;
    let mut last_draw = Instant::now() - interval;
    let mut menu = false;
    let mut dirty_layout = true;
    let mut lib_proto: Option<ratatui_image::protocol::Protocol> = None;
    let mut geom: Option<Geometry> = None;
    let mut buttons: Vec<Button> = Vec::new();
    let mut rows: Vec<Button> = Vec::new();
    let mut menu_rect: Option<Rect> = None;
    let mut hover: Option<Act> = None;

    let result = loop {
        // ---------------------------------------------------------------- input
        if event::poll(Duration::from_millis(15))? {
            match event::read()? {
                // Every key goes to the guest except Ctrl+] (console menu).
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    // terminals send Ctrl+] as 0x1d, which crossterm may report as Ctrl+5
                    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char(']') | KeyCode::Char('5')) {
                        menu = !menu;
                        sel = 0;
                        hover = None;
                        if proto == Proto::Kitty {
                            kitty_delete(&mut stdout);
                        }
                        dirty_layout = true;
                        last_gen = u64::MAX;
                        continue;
                    }
                    if menu {
                        let n = bar::MENU_TERM.len();
                        let mut run: Option<Act> = None;
                        match k.code {
                            KeyCode::Up => sel = (sel + n - 1) % n,
                            KeyCode::Down => sel = (sel + 1) % n,
                            KeyCode::Esc => menu = false,
                            KeyCode::Enter => run = Some(bar::MENU_TERM[sel]),
                            _ => {}
                        }
                        if let Some(a) = run {
                            match a {
                                Act::Close => break Ok(()),
                                Act::Refresh => client.request_full(),
                                Act::Protocol => {
                                    proto = match proto {
                                        Proto::Kitty => Proto::Lib(ProtocolType::Sixel),
                                        Proto::Lib(ProtocolType::Sixel) => Proto::Lib(ProtocolType::Halfblocks),
                                        Proto::Lib(ProtocolType::Halfblocks) => Proto::Lib(ProtocolType::Iterm2),
                                        _ => Proto::Kitty,
                                    };
                                    kitty_delete(&mut stdout);
                                    terminal.clear()?;
                                }
                                a => {
                                    if let Some((mods, sym)) = bar::keysyms(a) {
                                        client.tap(mods, sym);
                                    }
                                }
                            }
                            menu = false;
                        }
                        dirty_layout = true;
                        last_gen = u64::MAX;
                        continue;
                    }
                    if let Some((mods, sym)) = keys::from_crossterm(k.code, k.modifiers) {
                        client.tap(&mods, sym);
                    }
                }
                Event::Mouse(mut m) => {
                    // in pixel mode crossterm reports pixels; keep them and derive the cell
                    let (pcol, prow) = (m.column, m.row);
                    if pixel_mouse {
                        let f = picker.font_size();
                        m.column = pcol / f.width.max(1);
                        m.row = prow / f.height.max(1);
                    }
                    // The bar row and the menu panel are ours; everything else is forwarded.
                    let (col, row) = (m.column, m.row);
                    let on_menu = menu_rect.map(|r| r.contains((col, row).into())).unwrap_or(false);
                    let on_bar = row >= geom.as_ref().map(|g| g.area.height).unwrap_or(u16::MAX);
                    let act = bar::hit(&rows, col, row).or_else(|| bar::hit(&buttons, col, row));
                    if menu && !on_menu && act.is_none() {
                        // click outside the menu closes it (and is swallowed)
                        if matches!(m.kind, MouseEventKind::Down(_)) {
                            menu = false;
                            hover = None;
                            dirty_layout = true;
                            last_gen = u64::MAX;
                        }
                        continue;
                    }
                    // A drag that started over the video keeps reporting even over the bar,
                    // otherwise the guest would be left with a button pressed.
                    if act.is_none() && (on_bar || on_menu) && client.buttons() == 0 {
                        // empty part of the bar / menu: swallow, do not move the guest pointer
                        if hover.take().is_some() {
                            dirty_layout = true;
                        }
                        continue;
                    }
                    if let Some(a) = act {
                        hover = Some(a);
                        if let Some(i) = bar::MENU_TERM.iter().position(|m| *m == a).filter(|_| menu) {
                            sel = i;
                        }
                        if matches!(m.kind, MouseEventKind::Down(_)) {
                            match a {
                                Act::Menu => {
                                    menu = !menu;
                                    dirty_layout = true;
                                }
                                Act::Protocol => {
                                    proto = match proto {
                                        Proto::Kitty => Proto::Lib(ProtocolType::Sixel),
                                        Proto::Lib(ProtocolType::Sixel) => Proto::Lib(ProtocolType::Halfblocks),
                                        Proto::Lib(ProtocolType::Halfblocks) => Proto::Lib(ProtocolType::Iterm2),
                                        _ => Proto::Kitty,
                                    };
                                    kitty_delete(&mut stdout);
                                    terminal.clear()?;
                                    if menu {
                                        menu = false;
                                    }
                                    dirty_layout = true;
                                }
                                Act::Refresh => client.request_full(),
                                Act::Close => break Ok(()),
                                a => {
                                    if let Some((mods, sym)) = bar::keysyms(a) {
                                        client.tap(mods, sym);
                                    }
                                    menu = false;
                                    dirty_layout = true;
                                }
                            }
                            last_gen = u64::MAX;
                        }
                        continue;
                    }
                    if hover.take().is_some() {
                        dirty_layout = true;
                    }
                    if let Some(g) = &geom {
                        let (fw, fh) = (g.font.0 as f64, g.font.1 as f64);
                        // position inside the picture, in terminal pixels
                        let (px, py) = if pixel_mouse {
                            (pcol.saturating_sub(g.area.x * g.font.0) as f64, prow.saturating_sub(g.area.y * g.font.1) as f64)
                        } else {
                            ((m.column.saturating_sub(g.area.x) as f64 + 0.5) * fw, (m.row.saturating_sub(g.area.y) as f64 + 0.5) * fh)
                        };
                        let (fbw, fbh) = {
                            let f = client.fb.lock().unwrap();
                            (f.w, f.h)
                        };
                        // kitty stretches the picture to whole cells: use the real per-axis scale
                        let (sx, sy) = if proto == Proto::Kitty {
                            ((g.cols as f64 * fw) / fbw.max(1) as f64, (g.rows as f64 * fh) / fbh.max(1) as f64)
                        } else {
                            (g.scale, g.scale)
                        };
                        let x = (px / sx).clamp(0.0, fbw.saturating_sub(1) as f64) as u16;
                        let y = (py / sy).clamp(0.0, fbh.saturating_sub(1) as f64) as u16;
                        let bit = |b: MouseButton| match b {
                            MouseButton::Left => 1u8,
                            MouseButton::Middle => 2,
                            MouseButton::Right => 4,
                        };
                        let cur = client.buttons();
                        match m.kind {
                            MouseEventKind::Down(b) => client.pointer_event(x, y, cur | bit(b)),
                            MouseEventKind::Up(b) => client.pointer_event(x, y, cur & !bit(b)),
                            MouseEventKind::Drag(_) | MouseEventKind::Moved => {
                                client.pointer_event(x, y, cur);
                                // the pointer is drawn by us: redraw even if the guest screen did not change
                                dirty_layout = true;
                            }
                            MouseEventKind::ScrollUp => {
                                client.pointer_event(x, y, cur | 8);
                                client.pointer_event(x, y, cur);
                            }
                            MouseEventKind::ScrollDown => {
                                client.pointer_event(x, y, cur | 16);
                                client.pointer_event(x, y, cur);
                            }
                            _ => {}
                        }
                    }
                }
                Event::Resize(_, _) => {
                    terminal.autoresize()?;
                    dirty_layout = true;
                    last_gen = u64::MAX;
                    if proto == Proto::Kitty {
                        kitty_delete(&mut stdout);
                    }
                    terminal.clear()?;
                }
                _ => {}
            }
        }

        // ---------------------------------------------------------------- output
        let (gen, closed, bell) = {
            let mut f = client.fb.lock().unwrap();
            let b = std::mem::take(&mut f.bell);
            (f.gen, f.closed.clone(), b)
        };
        if bell {
            let _ = write!(stdout, "\x07");
        }
        if let Some(reason) = closed {
            break Err(anyhow::anyhow!("console closed: {reason}"));
        }
        if (gen == last_gen && !dirty_layout) || last_draw.elapsed() < interval {
            continue;
        }
        last_gen = gen;
        last_draw = Instant::now();
        let (w, h, rgb) = {
            let f = client.fb.lock().unwrap();
            snapshot_rgb(&f, client.pointer)
        };
        let Some(img) = RgbImage::from_raw(w as u32, h as u32, rgb) else { continue };
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height.saturating_sub(1));
        let font = picker.font_size();
        let g = geometry(area, (font.width, font.height), (w, h));
        let proto_name = match proto {
            Proto::Kitty => "kitty".to_string(),
            Proto::Lib(p) => format!("{p:?}").to_lowercase(),
        };
        buttons = bar_buttons(&bar::BAR, 0, size.height.saturating_sub(1), size.width);
        let bar_end = buttons.last().map(|b| b.x + b.w).unwrap_or(0);
        let status = format!(" Ctrl+] {} │ {title} │ {w}x{h} │ {proto_name} ", crate::i18n::tr("menu"));
        menu_rect = menu.then(|| centered(34, bar::MENU_TERM.len() as u16 + 2, area));
        rows = menu_rect.map(menu_rows).unwrap_or_default();
        if let Proto::Lib(p) = proto {
            picker.set_protocol_type(p);
            lib_proto = picker.new_protocol(DynamicImage::ImageRgb8(img.clone()), Size::new(area.width, area.height), Resize::Fit(None)).ok();
        }
        let (buttons_ref, rows_ref) = (&buttons, &rows);
        terminal.draw(|f| {
            if let (Proto::Lib(_), Some(p)) = (proto, &lib_proto) {
                f.render_widget(Image::new(p), area);
            }
            let sr = Rect::new(0, size.height.saturating_sub(1), size.width, 1);
            f.render_widget(Paragraph::new(Span::styled(" ".to_string(), Style::default().bg(Color::Blue))), sr);
            if bar_end < size.width {
                f.render_widget(
                    Paragraph::new(Span::styled(status.clone(), Style::default().bg(Color::Blue).fg(Color::White))),
                    Rect::new(bar_end, sr.y, size.width - bar_end, 1),
                );
            }
            for b in buttons_ref {
                let sel = hover == Some(b.act);
                let st = if sel {
                    Style::default().bg(Color::White).fg(Color::Blue).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().bg(Color::Blue).fg(Color::Cyan).add_modifier(Modifier::BOLD)
                };
                f.render_widget(Paragraph::new(Span::styled(b.act.text(), st)), Rect::new(b.x, b.y, b.w, b.h));
            }
            if let Some(r) = menu_rect {
                crate::ui::clear(f, r);
                let lines: Vec<Line> = rows_ref
                    .iter()
                    .enumerate()
                    .map(|(i, b)| {
                        let st = if i == sel {
                            Style::default().bg(Color::White).fg(Color::Blue).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        Line::from(Span::styled(b.act.text(), st))
                    })
                    .collect();
                f.render_widget(Paragraph::new(lines).block(block("Keys", true)), r);
            }
        })?;
        if proto == Proto::Kitty && !menu {
            let next = if kitty_shown { 1 - kitty_slot } else { kitty_slot };
            kitty_frame(&mut stdout, &g, &img, KITTY_IDS[next])?;
            if kitty_shown && next != kitty_slot {
                kitty_delete_id(&mut stdout, KITTY_IDS[kitty_slot]);
            }
            kitty_slot = next;
            kitty_shown = true;
        }
        geom = Some(g);
        dirty_layout = false;
    };

    if proto == Proto::Kitty {
        kitty_delete(&mut stdout);
    }
    if enhanced {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    if pixel_mouse {
        let _ = write!(stdout, "\x1b[?1016l");
    }
    execute!(stdout, DisableMouseCapture)?;
    terminal.clear()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_fits_aspect() {
        let g = geometry(Rect::new(0, 0, 100, 30), (10, 20), (1024, 768));
        assert!(g.cols <= 100 && g.rows <= 30);
        // 4:3 image in a 1000x600 px box is height bound: 600/768
        assert!((g.scale - 600.0 / 768.0).abs() < 1e-6);
    }

    #[test]
    fn kitty_escape_is_chunked() {
        let g = geometry(Rect::new(0, 0, 80, 24), (10, 20), (640, 480));
        let img = RgbImage::from_fn(640, 480, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 7]));
        let mut out = Vec::new();
        kitty_frame(&mut out, &g, &img, KITTY_IDS[0]).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b_Ga=T,f=24,o=z"));
        assert!(s.ends_with("\x1b\\"));
        assert!(s.contains("m=0;"));
    }
}
