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
    execute,
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
        execute!(std::io::stdout(), LeaveAlternateScreen)?;
        let r = sdl::run(client, title);
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen)?;
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
    let title = format!("VM {} {}", req.vmid, req.name);
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
