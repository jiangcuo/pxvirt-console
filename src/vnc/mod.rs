//! Graphical VM console.
//!
//! Connection: `qm vncproxy` (root, local or ssh to the guest's node) or the PVE API websocket.
//! Display: SDL2/KMSDRM on the physical console, otherwise inside the terminal
//! (Kitty / Sixel / iTerm2 / half blocks).

pub mod bar;
pub mod keys;
pub mod rfb;
#[cfg(feature = "sdl")]
pub mod sdl;
pub mod term;
pub mod transport;

use anyhow::{anyhow, bail, Result};
use crossterm::{
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{style::Style, text::Span, widgets::Paragraph};
use std::sync::mpsc;
use std::time::Duration;

use crate::app::App;
use crate::config::Config;
use crate::ctx::VncRequest;
use crate::Term;
use rfb::Client;
use transport::Conn;

#[derive(Debug, PartialEq)]
enum Display {
    Sdl,
    Terminal,
}

fn choose(cfg: &Config) -> Display {
    match cfg.console_display.as_str() {
        "terminal" => Display::Terminal,
        "sdl" => Display::Sdl,
        _ => {
            #[cfg(feature = "sdl")]
            {
                if sdl::on_physical_console() {
                    return Display::Sdl;
                }
            }
            Display::Terminal
        }
    }
}

fn connecting(terminal: &mut Term, msg: &str) {
    let _ = terminal.draw(|f| {
        let a = f.area();
        f.render_widget(Paragraph::new(Span::styled(format!(" {msg}"), Style::default())), a);
    });
}

/// Handshake with a timeout so a hanging proxy cannot freeze the console.
fn handshake(conn: Conn) -> Result<(Client, Option<transport::ChildGuard>, String)> {
    let Conn { reader, writer, password, child, via } = conn;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(Client::connect(reader, writer, password.as_deref()));
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(c)) => Ok((c, child, via)),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("timeout while connecting to the VNC server ({via})"),
    }
}

fn view(terminal: &mut Term, cfg: &Config, client: &mut Client, title: &str) -> Result<()> {
    let display = choose(cfg);
    #[cfg(feature = "sdl")]
    if display == Display::Sdl {
        disable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
        let r = sdl::run(client, title);
        enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
        terminal.clear()?;
        match r {
            Ok(()) => return Ok(()),
            Err(e) if format!("{e}").starts_with("console closed") => return Err(e),
            Err(e) => {
                // SDL not usable (no DRM master, no GPU...): fall back to the terminal renderer
                connecting(terminal, &format!("SDL unavailable ({e:#}), using the terminal renderer..."));
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
    let _ = (&display, disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen);
    term::run(terminal, client, title, cfg.console_fps)
}

pub fn run_for_guest(terminal: &mut Term, app: &mut App, req: VncRequest) -> Result<()> {
    let ctx = &app.ctx;
    if req.vmid == 0 {
        bail!("invalid VM id");
    }
    let title = format!("VM {} {}", req.vmid, crate::util::guest_name(&req.name));
    connecting(terminal, &format!("Connecting to the console of {title} on {}...", req.node));
    let conn = match (ctx.session.is_root() || ctx.session.rescue, ctx.exec_for(&req.node)) {
        (true, Some(exec)) if !ctx.remote => transport::qm_vncproxy(&exec, req.vmid)?,
        _ => {
            if ctx.session.rescue {
                return Err(anyhow!("graphical console in rescue mode needs local/ssh access to {}", req.node));
            }
            transport::api_websocket(&ctx.api, &req.node, req.vmid)?
        }
    };
    crate::audit(&ctx.session.user, &req.node, &format!("graphical console {} via {}", req.vmid, conn.via));
    let cfg = ctx.cfg.clone();
    let (mut client, _guard, _via) = handshake(conn)?;
    let r = view(terminal, &cfg, &mut client, &title);
    app.note_activity();
    r
}

/// Debug: `pxvirt-console --vnc-test /path/to/socket` (QEMU `-vnc unix:/path`).
pub fn run_test(terminal: &mut Term, cfg: &Config, sock: &str) -> Result<()> {
    connecting(terminal, &format!("Connecting to {sock}..."));
    let conn = if sock.starts_with("qm:") {
        transport::qm_vncproxy(&crate::exec::Exec::Local, sock[3..].parse().unwrap_or(0))?
    } else {
        transport::unix_socket(sock)?
    };
    let (mut client, _guard, via) = handshake(conn)?;
    let name = client.fb.lock().unwrap().name.clone();
    view(terminal, cfg, &mut client, &format!("{name} ({via})"))
}

/// Image support of the terminal for console previews (queried once at startup).
static PICKER: std::sync::OnceLock<Option<ratatui_image::picker::Picker>> = std::sync::OnceLock::new();

/// Query the terminal's graphics support; must run before the event loop reads stdin.
/// No previews on the system console (tty1 / serial).
pub fn init_picker() {
    let p = if crate::util::on_system_console() {
        None
    } else {
        ratatui_image::picker::Picker::from_query_stdio().ok().filter(|p| !matches!(p.protocol_type(), ratatui_image::picker::ProtocolType::Halfblocks))
    };
    let _ = PICKER.set(p);
}

/// The terminal can show console previews (Kitty / Sixel / iTerm2 graphics).
pub fn picker() -> Option<ratatui_image::picker::Picker> {
    PICKER.get().cloned().flatten()
}

/// Latest frame of a running preview connection.
#[derive(Default)]
pub struct PreviewState {
    pub img: Option<(std::time::Instant, image::RgbImage)>,
    pub err: String,
    /// last time the preview was drawn; the connection closes itself 10 s after that
    pub shown: Option<std::time::Instant>,
    /// closed because nobody looked at it (not an error)
    pub idle: bool,
}

/// A VNC connection kept open while a guest is selected: the frame is copied out about once a
/// second when it changed. Dropping the stream closes the connection.
pub struct PreviewStream {
    pub key: String,
    pub state: std::sync::Arc<std::sync::Mutex<PreviewState>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PreviewStream {
    pub fn start(key: String, api: std::sync::Arc<crate::api::Api>, exec: Option<crate::exec::Exec>, node: String, vmid: u32, max_w: u32) -> Self {
        use std::sync::atomic::Ordering;
        let state = std::sync::Arc::new(std::sync::Mutex::new(PreviewState::default()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (st, sp) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            let conn = match exec {
                Some(e) => transport::qm_vncproxy(&e, vmid),
                None => transport::api_websocket(&api, &node, vmid),
            };
            let res = conn.and_then(handshake);
            let (client, _guard, _via) = match res {
                Ok(c) => c,
                Err(e) => {
                    st.lock().unwrap().err = format!("{e:#}");
                    return;
                }
            };
            let mut last = 0;
            while !sp.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(if last == 0 { 300 } else { 1000 }));
                if st.lock().unwrap().shown.map(|t| t.elapsed() > Duration::from_secs(10)).unwrap_or(false) {
                    st.lock().unwrap().idle = true;
                    return;
                }
                let (gen, closed) = {
                    let f = client.fb.lock().unwrap();
                    (f.gen, f.closed.clone())
                };
                if let Some(c) = closed {
                    st.lock().unwrap().err = c;
                    return;
                }
                if gen == last || gen == 0 {
                    continue;
                }
                last = gen;
                let (w, h, rgb) = {
                    let f = client.fb.lock().unwrap();
                    rfb::snapshot_rgb(&f, (u16::MAX, u16::MAX))
                };
                if w == 0 || h == 0 {
                    continue;
                }
                if let Some(img) = image::RgbImage::from_raw(w as u32, h as u32, rgb) {
                    let tw = max_w.min(w as u32);
                    let th = (h as u32 * tw / w as u32).max(1);
                    let small = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
                    let mut s = st.lock().unwrap();
                    s.img = Some((std::time::Instant::now(), small));
                    s.err.clear();
                }
            }
        });
        Self { key, state, stop }
    }
}

impl Drop for PreviewStream {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Debug: grab one frame from a VNC unix socket (or `qm:<vmid>`) into a PPM file.
pub fn shot(sock: &str, out: &str) -> Result<String> {
    let conn = if let Some(id) = sock.strip_prefix("qm:") {
        transport::qm_vncproxy(&crate::exec::Exec::Local, id.parse().unwrap_or(0))?
    } else {
        let mut c = transport::unix_socket(sock)?;
        c.password = std::env::var("PXVIRT_VNC_PASSWORD").ok();
        c
    };
    let (client, _guard, via) = handshake(conn)?;
    let start = std::time::Instant::now();
    let mut last = 0;
    // wait until the screen has been updated a few times
    while start.elapsed() < Duration::from_secs(8) {
        let g = client.fb.lock().unwrap().gen;
        if g >= 3 && g == last {
            break;
        }
        last = g;
        std::thread::sleep(Duration::from_millis(700));
    }
    let f = client.fb.lock().unwrap();
    let (w, h, rgb) = rfb::snapshot_rgb(&f, (0, 0));
    let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
    data.extend_from_slice(&rgb);
    std::fs::write(out, data)?;
    Ok(format!("{w}x{h} name={:?} via={via} updates={}", f.name, f.gen))
}

#[cfg(test)]
mod thumb_tests {
    /// Real VM: `THUMB_SSH=root@node THUMB_VMID=102 cargo test thumb_live -- --ignored --nocapture`
    /// (only reads the screen: no keyboard or mouse input is sent)
    #[test]
    #[ignore]
    fn thumb_live() {
        let (Ok(t), Ok(id)) = (std::env::var("THUMB_SSH"), std::env::var("THUMB_VMID")) else { return };
        let api = std::sync::Arc::new(crate::api::Api::pvesh());
        let start = std::time::Instant::now();
        let st = super::PreviewStream::start("k".into(), api, Some(crate::exec::Exec::Ssh(t)), String::new(), id.parse().unwrap(), 800);
        let mut first = None;
        let mut frames = 0;
        let mut last = None;
        while start.elapsed() < std::time::Duration::from_secs(20) {
            st.state.lock().unwrap().shown = Some(std::time::Instant::now());
            std::thread::sleep(std::time::Duration::from_millis(100));
            let s = st.state.lock().unwrap();
            assert!(s.err.is_empty(), "{}", s.err);
            if let Some((t, img)) = &s.img {
                first.get_or_insert(start.elapsed());
                if last != Some(*t) {
                    frames += 1;
                    last = Some(*t);
                    img.save("/tmp/pxvirt-thumb.png").unwrap();
                }
            }
        }
        println!("first frame after {:.1}s, {frames} frame updates in 20 s", first.expect("a frame").as_secs_f64());
    }
}
