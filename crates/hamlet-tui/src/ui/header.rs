//! Header widget displaying village status and room tabs.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};

use crate::model::{AppState, ConnectionStatus, Room};

pub fn render_header(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(3)])
        .split(area);

    // 1. Top status line
    let conn_span = match &state.connection {
        ConnectionStatus::Connected => Span::styled(
            " ● CONNECTED ",
            Style::default().fg(Color::Black).bg(Color::Green),
        ),
        ConnectionStatus::Reconnecting {
            attempt,
            next_retry_ms,
            ..
        } => Span::styled(
            format!(" ⟳ RECONNECTING #{attempt} ({}s) ", next_retry_ms / 1000),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
        ConnectionStatus::Disconnected(err) => Span::styled(
            format!(" ✖ DISCONNECTED ({err}) "),
            Style::default().fg(Color::White).bg(Color::Red),
        ),
    };

    let daemon_info = if let Some(st) = &state.data.status {
        format!(
            "PID: {} | Uptime: {}s | Active: {} | Decrees: {} | Mail: {}",
            st.pid, st.uptime_secs, st.active_agents, st.queued_decrees, st.queued_mail
        )
    } else if state.demo_mode {
        "DEMO MODE — Scripted Local API Fixture Stream".into()
    } else {
        "Connecting to daemon...".into()
    };

    let title_line = Line::from(vec![
        Span::styled(
            " HAMLET ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("│ "),
        Span::styled(daemon_info, Style::default().fg(Color::DarkGray)),
        Span::raw(" │ "),
        conn_span,
    ]);

    frame.render_widget(Paragraph::new(title_line), chunks[0]);

    // 2. Room Navigation Tabs
    let tab_titles: Vec<Line> = Room::ALL
        .iter()
        .map(|r| {
            let num = r.index() + 1;
            let name = r.short_name();
            let path = r.tree_path();
            if *r == state.current_room {
                Line::from(vec![
                    Span::styled(
                        format!("{num}:{name} "),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("[{path}]"), Style::default().fg(Color::Cyan)),
                ])
            } else {
                Line::from(vec![
                    Span::styled(format!("{num}:{name} "), Style::default().fg(Color::Gray)),
                    Span::styled(format!("[{path}]"), Style::default().fg(Color::DarkGray)),
                ])
            }
        })
        .collect();

    let tabs = Tabs::new(tab_titles)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .select(state.current_room.index())
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::UNDERLINED),
        )
        .divider(" │ ");

    frame.render_widget(tabs, chunks[1]);
}
