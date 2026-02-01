mod app;
mod ui;

use app::{App, Screen};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, poll},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Write};
use std::process::Command;
use std::time::Duration;

fn main() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Clear terminal on startup
    terminal.clear()?;

    let res = run_app(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("Error: {err:?}");
    }

    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) -> Result<()> {
    let mut app = App::new();

    loop {
        // Poll async operations
        app.poll_login();
        app.poll_ping();
        app.poll_service_restart();
        app.poll_network_reset();

        terminal.draw(|f| ui::draw(f, &app))?;

        // Non-blocking event read
        if poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    // Q = logout (back to login screen)
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                        && matches!(app.screen, Screen::Main)
                    {
                        app.logout();
                        continue;
                    }

                    app.handle_key(key.code);

                    if app.should_exit {
                        return Ok(());
                    }

                    // Enter shell
                    if app.enter_shell {
                        app.enter_shell = false;
                        
                        // Exit TUI temporarily
                        disable_raw_mode()?;
                        execute!(io::stdout(), LeaveAlternateScreen)?;
                        
                        // Clear screen and show hint
                        print!("\x1B[2J\x1B[H");
                        println!("Entering shell... Type 'exit' to return to console.\n");
                        io::stdout().flush().ok();
                        
                        // Run bash as foreground process with inherited stdio
                        let _ = Command::new("/bin/bash")
                            .arg("-l")
                            .stdin(std::process::Stdio::inherit())
                            .stdout(std::process::Stdio::inherit())
                            .stderr(std::process::Stdio::inherit())
                            .status();
                        
                        // Re-enter TUI
                        enable_raw_mode()?;
                        execute!(io::stdout(), EnterAlternateScreen)?;
                        
                        // Force terminal clear and redraw
                        terminal.clear()?;
                    }
                }
            }
        }
    }
}
