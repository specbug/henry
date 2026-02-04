//! UI rendering for the TUI.

use crate::app::{App, AppState, LogLevel};
use henry_health::{format_bytes, HealthStatus};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Row, Table, Wrap},
    Frame,
};

/// Render the UI.
pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Header
            Constraint::Min(0),    // Main content
            Constraint::Length(3), // Command input
        ])
        .split(frame.area());

    render_header(frame, app, chunks[0]);
    render_main(frame, app, chunks[1]);
    render_command_bar(frame, app, chunks[2]);

    if app.state == AppState::Help {
        render_help_popup(frame, app);
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let (cpu, mem) = if let Some(ref metrics) = app.system_metrics {
        (
            format!("CPU: {:.0}%", metrics.cpu_percent),
            format!("MEM: {}", format_bytes(metrics.memory_used)),
        )
    } else {
        ("CPU: --".to_string(), "MEM: --".to_string())
    };

    let status_icon = if app.modules.iter().all(|m| !m.enabled || m.status == HealthStatus::Healthy)
    {
        Span::styled("◉", Style::default().fg(Color::Green))
    } else if app.modules.iter().any(|m| m.enabled && m.status == HealthStatus::Unhealthy) {
        Span::styled("◉", Style::default().fg(Color::Red))
    } else {
        Span::styled("◉", Style::default().fg(Color::Yellow))
    };

    let header = Line::from(vec![
        Span::styled(
            format!(" Henry v{}", app.version),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(
            area.width
                .saturating_sub(app.version.len() as u16 + cpu.len() as u16 + mem.len() as u16 + 20)
                as usize,
        )),
        Span::styled(format!("▲ {} ", cpu), Style::default().fg(Color::Cyan)),
        Span::styled(format!(" {} ", mem), Style::default().fg(Color::Magenta)),
        Span::raw(" "),
        status_icon,
        Span::raw(" "),
    ]);

    frame.render_widget(
        Paragraph::new(header).style(Style::default().bg(Color::DarkGray)),
        area,
    );
}

fn render_main(frame: &mut Frame, app: &App, area: Rect) {
    match app.state {
        AppState::Dashboard => render_dashboard(frame, app, area),
        AppState::Modules => render_modules(frame, app, area),
        AppState::Containers => render_containers(frame, app, area),
        AppState::Logs => render_logs(frame, app, area),
        AppState::Help => render_dashboard(frame, app, area), // Help is overlay
    }
}

fn render_dashboard(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    // Modules panel
    render_modules_summary(frame, app, left_chunks[0]);

    // System metrics panel
    render_system_metrics(frame, app, left_chunks[1]);

    // Activity log panel
    render_activity_log(frame, app, chunks[1]);
}

fn render_modules_summary(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .modules
        .iter()
        .map(|m| {
            let icon = App::module_status_icon(&m.status, m.enabled);
            let color = App::module_status_color(&m.status, m.enabled);
            let status = if m.enabled {
                m.status.to_string()
            } else {
                "Disabled".to_string()
            };
            let extra = m.message.as_deref().unwrap_or("");

            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", icon), Style::default().fg(color)),
                Span::styled(
                    format!("{:<12}", m.name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{:<10}", status), Style::default().fg(color)),
                Span::styled(extra, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Modules ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );

    frame.render_widget(list, area);
}

fn render_system_metrics(frame: &mut Frame, app: &App, area: Rect) {
    let text = if let Some(ref metrics) = app.system_metrics {
        vec![
            Line::from(vec![
                Span::raw(" CPU:    "),
                Span::styled(
                    format!("{:.1}%", metrics.cpu_percent),
                    Style::default().fg(if metrics.cpu_percent > 80.0 {
                        Color::Red
                    } else if metrics.cpu_percent > 50.0 {
                        Color::Yellow
                    } else {
                        Color::Green
                    }),
                ),
            ]),
            Line::from(vec![
                Span::raw(" Memory: "),
                Span::styled(
                    format!(
                        "{} / {} ({:.0}%)",
                        format_bytes(metrics.memory_used),
                        format_bytes(metrics.memory_total),
                        metrics.memory_percent
                    ),
                    Style::default().fg(if metrics.memory_percent > 80.0 {
                        Color::Red
                    } else if metrics.memory_percent > 60.0 {
                        Color::Yellow
                    } else {
                        Color::Green
                    }),
                ),
            ]),
            Line::raw(""),
            Line::styled(" Disks:", Style::default().add_modifier(Modifier::BOLD)),
        ]
        .into_iter()
        .chain(metrics.disks.iter().take(3).map(|d| {
            Line::from(vec![
                Span::raw(format!("   {} ", d.mount_point)),
                Span::styled(
                    format!("{:.0}%", d.percent),
                    Style::default().fg(if d.percent > 90.0 {
                        Color::Red
                    } else if d.percent > 70.0 {
                        Color::Yellow
                    } else {
                        Color::Green
                    }),
                ),
            ])
        }))
        .collect()
    } else {
        vec![Line::raw(" Loading metrics...")]
    };

    let paragraph = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" System ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );

    frame.render_widget(paragraph, area);
}

fn render_activity_log(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .activity_log
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .map(|entry| {
            let (icon, color) = match entry.level {
                LogLevel::Info => ("•", Color::Blue),
                LogLevel::Warn => ("⚠", Color::Yellow),
                LogLevel::Error => ("✗", Color::Red),
                LogLevel::Debug => ("·", Color::DarkGray),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    entry.timestamp.format(" %H:%M:%S ").to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(format!("{} ", icon), Style::default().fg(color)),
                Span::raw(&entry.message),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Activity ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );

    frame.render_widget(list, area);
}

fn render_modules(frame: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .modules
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let icon = App::module_status_icon(&m.status, m.enabled);
            let _color = App::module_status_color(&m.status, m.enabled);
            let style = if i == app.selected_module {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };

            Row::new(vec![
                format!(" {} {}", icon, m.name),
                if m.enabled { "Enabled" } else { "Disabled" }.to_string(),
                m.status.to_string(),
                m.message.clone().unwrap_or_default(),
                m.last_check.format("%H:%M:%S").to_string(),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["Module", "State", "Status", "Message", "Last Check"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Modules ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );

    frame.render_widget(table, area);
}

fn render_containers(frame: &mut Frame, _app: &App, area: Rect) {
    // Placeholder for container view
    let paragraph = Paragraph::new(vec![
        Line::raw(""),
        Line::styled(
            " Container management coming in Phase 3",
            Style::default().fg(Color::DarkGray),
        ),
        Line::raw(""),
        Line::raw(" This view will show:"),
        Line::raw("   • Running containers"),
        Line::raw("   • Resource usage per container"),
        Line::raw("   • Quick actions (start/stop/restart)"),
        Line::raw("   • Container logs"),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Containers ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );

    frame.render_widget(paragraph, area);
}

fn render_logs(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .activity_log
        .iter()
        .rev()
        .map(|entry| {
            let (icon, color) = match entry.level {
                LogLevel::Info => ("INFO ", Color::Blue),
                LogLevel::Warn => ("WARN ", Color::Yellow),
                LogLevel::Error => ("ERROR", Color::Red),
                LogLevel::Debug => ("DEBUG", Color::DarkGray),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    entry.timestamp.format(" %Y-%m-%d %H:%M:%S ").to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(icon, Style::default().fg(color)),
                Span::raw(" "),
                Span::raw(&entry.message),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Logs ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );

    frame.render_widget(list, area);
}

fn render_command_bar(frame: &mut Frame, app: &App, area: Rect) {
    let (prefix, style) = if app.command_focused {
        ("> ", Style::default().fg(Color::Cyan))
    } else {
        ("", Style::default().fg(Color::DarkGray))
    };

    let input = if app.command_focused {
        format!("{}{}", prefix, app.command_input)
    } else {
        String::new()
    };

    let help_line = Line::from(vec![
        Span::styled(" [Tab]", Style::default().fg(Color::Yellow)),
        Span::raw(" switch  "),
        Span::styled("[F1]", Style::default().fg(Color::Yellow)),
        Span::raw(" help  "),
        Span::styled("[F5]", Style::default().fg(Color::Yellow)),
        Span::raw(" refresh  "),
        Span::styled("[:]", Style::default().fg(Color::Yellow)),
        Span::raw(" command  "),
        Span::styled("[q]", Style::default().fg(Color::Yellow)),
        Span::raw(" quit"),
    ]);

    let paragraph = Paragraph::new(vec![
        Line::from(input).style(style),
        Line::raw(""),
        help_line,
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Commands ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );

    frame.render_widget(paragraph, area);

    // Show cursor when command input is focused
    if app.command_focused {
        frame.set_cursor_position((area.x + 3 + app.command_input.len() as u16, area.y + 1));
    }
}

fn render_help_popup(frame: &mut Frame, _app: &App) {
    let area = centered_rect(60, 70, frame.area());
    frame.render_widget(Clear, area);

    let help_text = vec![
        Line::styled(
            " Keyboard Shortcuts",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(" Navigation:"),
        Line::raw("   Tab         Switch between views"),
        Line::raw("   1-4         Jump to view (1=Dashboard, 2=Modules, etc)"),
        Line::raw("   j/k, ↑/↓    Navigate lists"),
        Line::raw(""),
        Line::raw(" Actions:"),
        Line::raw("   F1          Show this help"),
        Line::raw("   F5          Refresh metrics"),
        Line::raw("   : or /      Enter command mode"),
        Line::raw("   q           Quit"),
        Line::raw(""),
        Line::raw(" Commands:"),
        Line::raw("   :help       Show help"),
        Line::raw("   :refresh    Refresh metrics"),
        Line::raw("   :modules    Show modules view"),
        Line::raw("   :quit       Quit Henry"),
        Line::raw(""),
        Line::styled(
            " Press any key to close",
            Style::default().fg(Color::DarkGray),
        ),
    ];

    let paragraph = Paragraph::new(help_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help ")
                .title_style(Style::default().add_modifier(Modifier::BOLD))
                .style(Style::default().bg(Color::Black)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
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
