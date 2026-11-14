mod api;
mod app;
mod backend;
mod config;
mod ctx;
mod exec;
mod i18n;
mod loader;
mod login;
mod metrics;
mod modules;
#[allow(dead_code)] // full copy of the pxvdi-sdk module
mod punycode;
mod raidhost;
mod rescue;
mod sshconn;
mod ui;
mod util;
mod vnc;

use anyhow::Result;
use app::{App, RemoteTarget, APP_VERSION};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::Terminal;
use std::io::{self, Write};
use std::time::Duration;

pub type Term = Terminal<backend::ConsoleBackend>;

/// Where audit messages go (SSH mode: the journal of the connected node).
static AUDIT_EXEC: std::sync::OnceLock<exec::Exec> = std::sync::OnceLock::new();

/// Audit trail of write operations in the journal (SYSLOG_IDENTIFIER=pxvirt-console).
pub fn audit(user: &str, node: &str, what: &str) {
    let msg = format!("user={user} node={node} action={what}");
    let e = AUDIT_EXEC.get().cloned().unwrap_or(exec::Exec::Local);
    let _ = e
        .command("logger", &["-t", "pxvirt-console", "--", &msg])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut c| c.wait());
}

struct Args {
    remote: Option<RemoteTarget>,
    ssh: Option<String>,
    lang: Option<String>,
    vnc_test: Option<String>,
    dump: Option<String>,
}

fn usage() -> ! {
    println!(
        "pxvirt-console {APP_VERSION}\n\n\
Usage: pxvirt-console [options]\n\n\
  --remote HOST[:PORT]   manage a remote PVE host through its API (default port 8006)\n\
  --ssh [USER@]HOST[:PORT]  manage a node over ssh: API through a port forward plus host\n\
                         commands; as root you are logged in as root@pam automatically\n\
  --lang en|zh|auto      interface language (tty1 is always English)\n\
  --vnc-test SOCKET      debug: open the graphical viewer on a raw VNC unix socket\n\
  --version              print version\n"
    );
    std::process::exit(0)
}

fn parse_args() -> Args {
    let mut a = Args { remote: None, ssh: None, lang: None, vnc_test: None, dump: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--remote" | "-r" => {
                let v = it.next().unwrap_or_else(|| usage());
                let (host, port) = match v.rsplit_once(':') {
                    Some((h, p)) if !h.contains(':') || h.ends_with(']') => {
                        (h.trim_matches(|c| c == '[' || c == ']').to_string(), p.parse().unwrap_or(8006))
                    }
                    _ => (v.trim_matches(|c| c == '[' || c == ']').to_string(), 8006),
                };
                a.remote = Some(RemoteTarget { host, port });
            }
            "--ssh" | "-s" => a.ssh = Some(it.next().unwrap_or_else(|| usage())),
            "--lang" => a.lang = it.next(),
            "--vnc-test" => a.vnc_test = it.next(),
            "--dump-screens" => a.dump = it.next(),
            "--vnc-shot" => {
                // debug: connect, wait for the first frame, write a PPM image
                let sock = it.next().unwrap_or_else(|| usage());
                let out = it.next().unwrap_or_else(|| usage());
                match vnc::shot(&sock, &out) {
                    Ok(s) => println!("{s}"),
                    Err(e) => {
                        eprintln!("vnc-shot: {e:#}");
                        std::process::exit(1);
                    }
                }
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("pxvirt-console {APP_VERSION}");
                std::process::exit(0);
            }
            _ => usage(),
        }
    }
    a
}

fn main() -> Result<()> {
    let args = parse_args();
    if !cfg!(target_os = "linux") && args.remote.is_none() && args.ssh.is_none() && args.vnc_test.is_none() {
        // no PVE on this machine: client modes only
        eprintln!("pxvirt-console manages PVE nodes remotely on this system:\n  pxvirt-console --ssh root@NODE\n  pxvirt-console --remote NODE");
        std::process::exit(2);
    }
    let mut cfg = config::Config::load();
    if let Some(l) = args.lang {
        cfg.lang = l;
    }
    if let Some(dir) = args.dump {
        // debug: render all views (local: needs root for the ticket; --ssh: root@ ssh)
        std::fs::create_dir_all(&dir)?;
        if let Some(spec) = &args.ssh {
            let c = sshconn::SshConn::connect(spec)?;
            let _ = AUDIT_EXEC.set(c.exec());
            let t = app::SshTarget { target: c.target.clone(), label: c.label(), node: c.node.clone(), api_port: c.api_port, user: c.user.clone() };
            let mut app = App::new(cfg, None, Some(t));
            app.ssh_auto_login()?;
            let r = app.debug_dump(&dir, &[(160, 40), (110, 32)]);
            c.close();
            return r;
        }
        let mut app = App::new(cfg, args.remote, None);
        for _ in 0..25 {
            app.tick();
            std::thread::sleep(Duration::from_millis(100));
        }
        for (w, h) in [(120u16, 35u16), (80, 25)] {
            std::fs::write(format!("{dir}/0_dashboard_{w}x{h}.txt"), app.debug_render(w, h))?;
            std::fs::write(format!("{dir}/0_dashboard_{w}x{h}.html"), app.debug_render_html(w, h))?;
        }
        app.on_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter));
        for (w, h) in [(120u16, 35u16), (80, 25)] {
            std::fs::write(format!("{dir}/0_login_{w}x{h}.txt"), app.debug_render(w, h))?;
        }
        app.debug_login_root()?;
        if std::env::var("PXVIRT_MOUSE_SELFTEST").is_ok() {
            let r = app.debug_mouse_selftest();
            std::fs::write(format!("{dir}/mouse_selftest.txt"), &r)?;
            print!("{r}");
            return Ok(());
        }
        app.debug_dump(&dir, &[(200, 50), (110, 32), (80, 25)])?;
        return Ok(());
    }

    if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
        eprintln!("pxvirt-console needs an interactive terminal (tty1, ssh -t or a terminal emulator).");
        std::process::exit(1);
    }
    // SSH mode: connect before the TUI so host-key / password prompts work normally
    let conn = match &args.ssh {
        Some(spec) => match sshconn::SshConn::connect(spec) {
            Ok(c) => {
                let _ = AUDIT_EXEC.set(c.exec());
                Some(c)
            }
            Err(e) => {
                eprintln!("pxvirt-console: {e:#}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(backend::ConsoleBackend::new(stdout))?;
    terminal.clear()?;
    // graphics support for console previews (before the event loop reads stdin)
    vnc::init_picker();

    let res = if let Some(sock) = args.vnc_test {
        i18n::init(&cfg.lang);
        vnc::run_test(&mut terminal, &cfg, &sock)
    } else {
        let ssh = conn.as_ref().map(|c| app::SshTarget {
            target: c.target.clone(),
            label: c.label(),
            node: c.node.clone(),
            api_port: c.api_port,
            user: c.user.clone(),
        });
        let mut app = App::new(cfg, args.remote, ssh);
        if conn.is_some() {
            if let Err(e) = app.ssh_auto_login() {
                app.ctx.toast_err(format!("auto login failed: {e:#}"));
            }
        }
        run_app(&mut terminal, &mut app)
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    if let Some(c) = &conn {
        c.close();
    }
    if let Err(err) = res {
        eprintln!("Error: {err:?}");
    }
    Ok(())
}

fn run_app(terminal: &mut Term, app: &mut App) -> Result<()> {
    loop {
        app.tick();
        terminal.draw(|f| app.draw(f))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Mouse(m) => app.on_mouse(m),
                Event::Resize(_, _) => terminal.autoresize()?,
                _ => {}
            }
        }
        if app.should_exit {
            return Ok(());
        }
        if let Some(s) = app.pending_suspend.take() {
            run_suspended(terminal, app, s)?;
        }
        if let Some(req) = app.pending_vnc.take() {
            if let Err(e) = vnc::run_for_guest(terminal, app, req) {
                app.ctx.toast_err(format!("console: {e:#}"));
            }
            // the viewer turns mouse reporting off when it closes
            execute!(io::stdout(), EnableMouseCapture)?;
            terminal.clear()?;
            app.note_activity();
        }
    }
}

/// Leave the TUI, run an interactive program (shell, guest console, apt), come back.
fn run_suspended(terminal: &mut Term, app: &mut App, s: ctx::Suspend) -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    print!("\x1B[2J\x1B[H");
    println!("{}", s.banner);
    io::stdout().flush().ok();

    let args: Vec<&str> = s.args.iter().map(|a| a.as_str()).collect();
    let mut cmd = s.exec.interactive(&s.prog, &args);
    for (k, v) in &s.env {
        cmd.env(k, v);
    }
    let status = cmd
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();
    if let Err(e) = &status {
        println!("\nfailed to start {}: {e}", s.prog);
    }
    if s.pause_after || status.is_err() {
        println!("\n[press Enter to return to the console]");
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    }

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;
    if s.logout_after {
        app.logout();
    } else {
        app.note_activity();
    }
    Ok(())
}
