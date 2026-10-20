//! Live WebSocket event ticker widget with per-room filtering (SPEC §12.2).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::model::AppState;

pub fn render_ticker(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let filter_badge = if state.ticker.filter_by_room {
        format!(
            "[FILTER: {} ONLY (press 'f' to view all)]",
            state.current_room.short_name()
        )
    } else {
        "[FILTER: ALL EVENTS (press 'f' to filter by room)]".into()
    };

    let title_line = Line::from(vec![
        Span::styled(
            " ● Live WS Event Feed ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(filter_badge, Style::default().fg(Color::DarkGray)),
    ]);

    let visible_events = state.ticker.visible_events(state.current_room);
    let mut lines = Vec::new();

    if visible_events.is_empty() {
        lines.push(Line::from(Span::styled(
            "Waiting for WebSocket events...",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Take the latest events fitting in the area
        let take_count = (area.height.saturating_sub(2)) as usize;
        let start_idx = visible_events.len().saturating_sub(take_count);
        for event in &visible_events[start_idx..] {
            let topic_color = match event.topic {
                hamlet_core::api::WsTopic::Commission => Color::Cyan,
                hamlet_core::api::WsTopic::Mail => Color::Yellow,
                hamlet_core::api::WsTopic::Decree => Color::Magenta,
                hamlet_core::api::WsTopic::Census => Color::Green,
                hamlet_core::api::WsTopic::Telemetry => Color::Blue,
                _ => Color::White,
            };

            lines.push(Line::from(vec![
                Span::styled(
                    format!("[#{}] ", event.seq),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("[{}:{}] ", event.topic, event.event_kind),
                    Style::default().fg(topic_color),
                ),
                Span::raw(event.summary.clone()),
            ]));
        }
    }

    let ticker_widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title_line)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    frame.render_widget(ticker_widget, area);
}
