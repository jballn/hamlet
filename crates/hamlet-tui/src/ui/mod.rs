//! UI assembly and layout for `hamlet-tui`.

pub mod courier;
pub mod guild;
pub mod header;
pub mod keep;
pub mod manor;
pub mod market;
pub mod modals;
pub mod tavern;
pub mod ticker;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::model::{AppState, Room};

/// Main render entrypoint for the TUI application frame.
pub fn render_app(frame: &mut ratatui::Frame, state: &AppState) {
    let size = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // Header + tabs
            Constraint::Min(10),   // Room content
            Constraint::Length(5), // Live WS Event Ticker
            Constraint::Length(1), // Footer key hints
        ])
        .split(size);

    // 1. Header & Room Tabs
    header::render_header(frame, chunks[0], state);

    // 2. Active Room View
    match state.current_room {
        Room::Guild => guild::render_guild(frame, chunks[1], state),
        Room::Tavern => tavern::render_tavern(frame, chunks[1], state),
        Room::Courier => courier::render_courier(frame, chunks[1], state),
        Room::Market => market::render_market(frame, chunks[1], state),
        Room::Keep => keep::render_keep(frame, chunks[1], state),
        Room::Manor => manor::render_manor(frame, chunks[1], state),
    }

    // 3. Live WebSocket Event Ticker
    ticker::render_ticker(frame, chunks[2], state);

    // 4. Footer & Keybinding hints
    render_footer(frame, chunks[3], state);

    // 5. Modal dialogs (if active)
    modals::render_modals(frame, size, state);
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    if let Some((msg, is_error)) = &state.status_message {
        let style = if *is_error {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        };
        let p = Paragraph::new(Line::from(vec![
            Span::styled(" ● ", style),
            Span::styled(msg.as_str(), style),
        ]));
        frame.render_widget(p, area);
        return;
    }

    let hints = Line::from(vec![
        Span::styled("Tab/1-6", Style::default().fg(Color::Yellow)),
        Span::raw(":Rooms "),
        Span::styled("j/k", Style::default().fg(Color::Yellow)),
        Span::raw(":Select "),
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::raw(":Detail "),
        Span::styled("s", Style::default().fg(Color::Cyan)),
        Span::raw(":Steer "),
        Span::styled("d", Style::default().fg(Color::Cyan)),
        Span::raw(":Decree "),
        Span::styled("e", Style::default().fg(Color::Cyan)),
        Span::raw(":Escalations "),
        Span::styled("m", Style::default().fg(Color::Cyan)),
        Span::raw(":Mail "),
        Span::styled("f", Style::default().fg(Color::DarkGray)),
        Span::raw(":Filter "),
        Span::styled("r", Style::default().fg(Color::DarkGray)),
        Span::raw(":Refresh "),
        Span::styled("?", Style::default().fg(Color::DarkGray)),
        Span::raw(":Help "),
        Span::styled("q", Style::default().fg(Color::Red)),
        Span::raw(":Quit"),
    ]);

    frame.render_widget(Paragraph::new(hints), area);
}
