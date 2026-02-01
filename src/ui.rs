use crate::app::{App, Screen};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_content(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);

    // Message popup
    if let Some((msg, success)) = &app.message {
        draw_message(f, msg, *success);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let title = format!(
        "Proxmox VE Console v{}  |  {}  |  PVE {}",
        crate::app::VERSION, app.hostname, app.pve_version
    );
    let header = Paragraph::new(title)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hint = match &app.screen {
        Screen::Login => "Enter password  Esc Exit",
        Screen::Main => "↑↓ Navigate  Enter Select  Q Logout",
        Screen::ResetPassword => "Tab/↑↓ Switch field  Enter Submit  Esc Back",
        Screen::Network => "P Ping  R Reset Network  Esc Back",
        Screen::NetworkPing => "Enter Ping  Esc Back",
        Screen::NetworkReset | Screen::NetworkSelectIface | Screen::NetworkConfigIP => "Esc Back",
        Screen::RestartService => "↑↓ Navigate  Enter Select  Esc Back",
        Screen::ServiceConfirm(_) => "Y Confirm  N/Esc Cancel",
    };
    let footer = Paragraph::new(hint).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, area);
}

fn draw_content(f: &mut Frame, app: &App, area: Rect) {
    // Login screen is fullscreen
    if matches!(app.screen, Screen::Login) {
        draw_login(f, app, area);
        return;
    }

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    draw_main_menu(f, app, columns[0]);

    match &app.screen {
        Screen::Login => {} // handled above
        Screen::Main => {
            let block = Block::default().borders(Borders::NONE);
            f.render_widget(block, columns[1]);
        }
        Screen::ResetPassword => draw_password_form(f, app, columns[1]),
        Screen::Network | Screen::NetworkPing | Screen::NetworkReset 
        | Screen::NetworkSelectIface | Screen::NetworkConfigIP => {
            draw_network(f, app, columns[1]);
        }
        Screen::RestartService | Screen::ServiceConfirm(_) => draw_service_list(f, app, columns[1]),
    }
}

fn draw_login(f: &mut Frame, app: &App, area: Rect) {
    // Center the login box
    let login_area = centered_rect(50, 9, area);
    f.render_widget(Clear, login_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Login")
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(block, login_area);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(login_area);

    let title = Paragraph::new("Proxmox VE Console")
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(title, inner[0]);

    if app.login_checking {
        // Show verifying status
        let status = Paragraph::new("Verifying...")
            .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
            .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(status, inner[2]);
    } else {
        let password_display = "*".repeat(app.login_password.len());
        let password_line = Paragraph::new(format!("Password: {}_", password_display))
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(password_line, inner[2]);
    }
}

fn draw_main_menu(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = App::main_menu_items()
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == app.main_index && matches!(app.screen, Screen::Main) {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = if i == app.main_index && matches!(app.screen, Screen::Main) {
                "> "
            } else {
                "  "
            };
            ListItem::new(Line::from(Span::styled(format!("{}{}", prefix, item), style)))
        })
        .collect();

    let menu = List::new(items).block(Block::default().borders(Borders::RIGHT).title("Menu"));
    f.render_widget(menu, area);
}

fn draw_password_form(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Reset Password");
    f.render_widget(block, area);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    let fields = [
        ("Username", &app.username, 0),
        ("New Password", &"*".repeat(app.password.len()), 1),
        ("Confirm Password", &"*".repeat(app.password_confirm.len()), 2),
    ];

    for (label, value, idx) in fields {
        let style = if app.password_field == idx {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        let prefix = if app.password_field == idx { "> " } else { "  " };
        let text = format!("{}{}: {}", prefix, label, value);
        let p = Paragraph::new(text).style(style);
        f.render_widget(p, inner[idx]);
    }
}

fn draw_network(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Network");
    f.render_widget(block, area);

    // Split into upper (config) and lower (ping) sections
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(5), // Config section
            Constraint::Length(1), // Spacer
            Constraint::Min(0),    // Ping section
        ])
        .split(area);

    // Upper section: Current network config
    let config_block = Block::default()
        .borders(Borders::ALL)
        .title("Current Config");
    f.render_widget(config_block, sections[0]);

    let config_inner = Layout::default()
        .margin(1)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(sections[0]);

    let info = &app.network_info;
    let line1 = format!("Interface: {}     IP: {}/{}", info.iface, info.ip, 
        netmask_to_cidr(&info.netmask));
    let line2 = format!("Gateway: {}", info.gateway);

    f.render_widget(
        Paragraph::new(line1).style(Style::default().fg(Color::White)),
        config_inner[0],
    );
    f.render_widget(
        Paragraph::new(line2).style(Style::default().fg(Color::White)),
        config_inner[1],
    );

    // Lower section: Ping
    let ping_block = Block::default().borders(Borders::ALL).title("Ping Test");
    f.render_widget(ping_block, sections[2]);

    let ping_inner = Layout::default()
        .margin(1)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(sections[2]);

    // Ping input
    let ping_line = if app.ping_running {
        format!("Target: {} [running...]", app.ping_target)
    } else {
        format!("Target: {}_", app.ping_target)
    };
    let style = if matches!(app.screen, Screen::NetworkPing) {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    f.render_widget(Paragraph::new(ping_line).style(style), ping_inner[0]);

    // Ping result
    if let Some(result) = &app.ping_result {
        let result_area = ping_inner[1];
        let lines: Vec<Line> = result
            .lines()
            .take(result_area.height as usize)
            .map(|l| Line::from(l))
            .collect();
        f.render_widget(
            Paragraph::new(lines).style(Style::default().fg(Color::Cyan)),
            result_area,
        );
    }

    // Network reset overlays
    match &app.screen {
        Screen::NetworkReset => {
            draw_network_reset_confirm(f, app);
        }
        Screen::NetworkSelectIface => {
            draw_network_select_iface(f, app);
        }
        Screen::NetworkConfigIP => {
            draw_network_config_ip(f, app);
        }
        _ => {}
    }
}

fn draw_network_reset_confirm(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 8, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Reset Network")
        .style(Style::default().fg(Color::Red));
    f.render_widget(block, area);

    let inner = Layout::default()
        .margin(1)
        .constraints([Constraint::Length(2), Constraint::Length(2)])
        .split(area);

    // Show progress if running
    if let Some(status) = &app.network_reset_status {
        let progress = Paragraph::new(status.as_str())
            .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
            .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(progress, inner[0]);
        
        let hint = Paragraph::new("Please wait...")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(hint, inner[1]);
    } else {
        let warning = Paragraph::new("WARNING: This will CLEAR all network config!\nBackup will be created. Continue?")
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(warning, inner[0]);

        let hint = Paragraph::new("[Y] Yes, reset  [N/Esc] Cancel")
            .style(Style::default().fg(Color::White));
        f.render_widget(hint, inner[1]);
    }
}

fn draw_network_select_iface(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 12, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Select Network Interface");
    f.render_widget(block, area);

    let inner = Layout::default()
        .margin(1)
        .constraints([Constraint::Min(0)])
        .split(area)[0];

    let items: Vec<ListItem> = app
        .network_ifaces
        .iter()
        .enumerate()
        .map(|(i, iface)| {
            let selected = i == app.network_index;
            let style = if selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let carrier = if iface.has_carrier { "[CONNECTED]" } else { "[NO CABLE]" };
            let prefix = if selected { "> " } else { "  " };
            let text = format!("{}{:<10} {}  {}", prefix, iface.name, iface.mac, carrier);
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);
}

fn draw_network_config_ip(f: &mut Frame, app: &App) {
    let area = centered_rect(50, 10, f.area());
    f.render_widget(Clear, area);

    let iface_name = app
        .network_ifaces
        .get(app.network_index)
        .map(|i| i.name.as_str())
        .unwrap_or("unknown");

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("Configure IP for {}", iface_name));
    f.render_widget(block, area);

    let inner = Layout::default()
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

    let fields = [
        ("IP Address", &app.network_info.ip, 0),
        ("Netmask", &app.network_info.netmask, 1),
        ("Gateway", &app.network_info.gateway, 2),
    ];

    for (label, value, idx) in fields {
        let style = if app.password_field == idx {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        let prefix = if app.password_field == idx { "> " } else { "  " };
        let cursor = if app.password_field == idx { "_" } else { "" };
        let text = format!("{}{}: {}{}", prefix, label, value, cursor);
        f.render_widget(Paragraph::new(text).style(style), inner[idx]);
    }
}

fn netmask_to_cidr(netmask: &str) -> u8 {
    netmask
        .split('.')
        .filter_map(|s| s.parse::<u8>().ok())
        .map(|b| b.count_ones() as u8)
        .sum()
}

fn draw_service_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Services");
    f.render_widget(block.clone(), area);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Min(0)])
        .split(area)[0];

    let mut items: Vec<ListItem> = app
        .services
        .iter()
        .enumerate()
        .map(|(i, svc)| {
            let selected = i == app.service_index && !matches!(app.screen, Screen::Main);
            let style = if selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let status = if svc.running { "[●]" } else { "[○]" };
            let prefix = if selected { "> " } else { "  " };
            let text = format!("{}{} {}", prefix, status, svc.display);
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();

    // Add "Restart All" option
    let restart_all_idx = app.services.len();
    let selected = app.service_index == restart_all_idx && !matches!(app.screen, Screen::Main);
    let style = if selected {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Red)
    };
    let prefix = if selected { "> " } else { "  " };
    items.push(ListItem::new(Line::from(Span::styled(
        format!("{}[*] Restart All Services", prefix),
        style,
    ))));

    let list = List::new(items);
    f.render_widget(list, inner);

    // Confirm dialog or progress
    if let Screen::ServiceConfirm(idx) = &app.screen {
        if let Some(status) = &app.service_restart_status {
            draw_progress_dialog(f, status);
        } else if *idx >= app.services.len() {
            draw_confirm_dialog(f, "Restart ALL services?");
        } else if let Some(svc) = app.services.get(*idx) {
            draw_confirm_dialog(f, &format!("Restart '{}'?", svc.name));
        }
    }
}

fn draw_progress_dialog(f: &mut Frame, msg: &str) {
    let area = centered_rect(40, 5, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Progress")
        .style(Style::default().fg(Color::Green));
    f.render_widget(block, area);

    let inner = Layout::default().margin(1).constraints([Constraint::Min(0)]).split(area)[0];
    let p = Paragraph::new(msg)
        .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(p, inner);
}

fn draw_confirm_dialog(f: &mut Frame, msg: &str) {
    let area = centered_rect(40, 7, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Confirm")
        .style(Style::default().bg(Color::DarkGray));
    f.render_widget(block, area);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Length(1), Constraint::Length(2)])
        .split(area);

    let text = Paragraph::new(msg).style(Style::default().fg(Color::White));
    f.render_widget(text, inner[0]);

    let hint = Paragraph::new("[Y] Yes  [N] No").style(Style::default().fg(Color::Yellow));
    f.render_widget(hint, inner[1]);
}

fn draw_message(f: &mut Frame, msg: &str, success: bool) {
    let area = centered_rect(50, 5, f.area());
    f.render_widget(Clear, area);
    let color = if success { Color::Green } else { Color::Red };
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().fg(color));
    f.render_widget(block, area);

    let inner = Layout::default()
        .margin(1)
        .constraints([Constraint::Min(0)])
        .split(area)[0];

    let text = Paragraph::new(msg).style(Style::default().fg(color));
    f.render_widget(text, inner);
}

fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height.min(100)) / 2),
            Constraint::Length(height),
            Constraint::Percentage((100 - height.min(100)) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
