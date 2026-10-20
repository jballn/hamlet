//! Manor view (SPEC §2 `keep/census/manor/`).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};

use crate::model::AppState;

pub fn render_manor(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // MUD description
            Constraint::Min(8),    // Content panes
        ])
        .split(area);

    let manor = &state.data.landmarks.manor;

    // 1. MUD Description Box
    let desc_text = vec![
        Line::from(vec![
            Span::styled(
                "═══ Mayor's Manor ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("[keep/census/manor/] ", Style::default().fg(Color::Cyan)),
            Span::raw("— Wake Incarnation: "),
            Span::styled(
                format!("{}", manor.mayor_incarnation.unwrap_or(0)),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Decrees: "),
            Span::styled(
                format!("{}", state.data.decrees.len()),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Escalations (Open/Resolved): "),
            Span::styled(
                format!("{}/{}", manor.escalations_open, manor.escalations_resolved),
                Style::default().fg(if manor.escalations_open > 0 {
                    Color::Yellow
                } else {
                    Color::Green
                }),
            ),
        ]),
        Line::from(Span::styled(
            state.current_room.mud_description(),
            Style::default().fg(Color::Gray),
        )),
    ];

    let desc_box = Paragraph::new(desc_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(desc_box, chunks[0]);

    // 2. Main Content: Split Left (Mayor Study & Decrees) and Right (Escalations Inbox)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    // Left Column: Split Top (Mayor Study) and Bottom (Decree Queue)
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(6)])
        .split(body_chunks[0]);

    // Mayor Study
    let mut mayor_lines = Vec::new();
    mayor_lines.push(Line::from(vec![
        Span::styled("Trunk Agent:          ", Style::default().fg(Color::Cyan)),
        Span::styled(
            "UID 1000 ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("(Static System Lock)", Style::default().fg(Color::DarkGray)),
    ]));
    mayor_lines.push(Line::from(vec![
        Span::styled("System Prompt:        ", Style::default().fg(Color::Cyan)),
        Span::raw(if manor.mayor_md {
            "MAYOR.md Present"
        } else {
            "Missing MAYOR.md"
        }),
    ]));
    mayor_lines.push(Line::from(vec![
        Span::styled("Supervised Status:    ", Style::default().fg(Color::Cyan)),
        Span::styled(
            manor.mayor_session.as_deref().unwrap_or("sleeping"),
            Style::default().fg(Color::Green),
        ),
    ]));
    mayor_lines.push(Line::from(vec![
        Span::styled("Consecutive Wakes:    ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{}", manor.mayor_incarnation.unwrap_or(0))),
    ]));
    mayor_lines.push(Line::from(vec![
        Span::styled("Quarantined Decrees:  ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{} (decrees/.rejected/)", manor.decrees_rejected)),
    ]));

    let mayor_box = Paragraph::new(mayor_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" The Mayor (UID 1000 Control Loop) ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(mayor_box, left_chunks[0]);

    // Decree Queue Table
    let sel_idx = state.current_sel().list_index;
    let header_cells = ["Seq", "ID", "Action", "Status"].iter().map(|h| {
        Span::styled(
            *h,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    });
    let header_row = Row::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<Row> = state
        .data
        .decrees
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let is_sel = i == sel_idx;
            let status_color = match d.status.as_str() {
                "done" | "delivered" | "acknowledged" => Color::Green,
                "queued" | "pending" => Color::Yellow,
                "rejected" | "failed" => Color::Red,
                _ => Color::White,
            };

            let style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            Row::new(vec![
                Span::raw(format!("{}", d.seq)),
                Span::raw(d.decree_id.clone()),
                Span::raw(d.action.clone()),
                Span::styled(d.status.clone(), Style::default().fg(status_color)),
            ])
            .style(style)
        })
        .collect();

    let decree_table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(14),
            Constraint::Length(22),
            Constraint::Length(12),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Mayor Decrees (Press 'd' to Review) ")
            .border_style(Style::default().fg(Color::White)),
    );

    frame.render_widget(decree_table, left_chunks[1]);

    // Right Column: Escalation Inbox
    let mut esc_lines = Vec::new();
    esc_lines.push(Line::from(Span::styled(
        "Escalation Inbox [Press 'e' to Open / Resolve]:",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    esc_lines.push(Line::raw(""));

    if state.data.escalations.is_empty() {
        esc_lines.push(Line::from(Span::styled(
            "(No escalation notices pending)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, esc) in state.data.escalations.iter().enumerate() {
            let (status_icon, status_color) = if esc.resolved {
                ("[RESOLVED]", Color::Green)
            } else {
                ("[OPEN]", Color::Red)
            };

            esc_lines.push(Line::from(vec![
                Span::styled(format!("{}. ", i + 1), Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{status_icon} "),
                    Style::default()
                        .fg(status_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("Decree {}: ", esc.decree_id),
                    Style::default().fg(Color::White),
                ),
                Span::raw(esc.summary.clone()),
            ]));

            if let Some(res) = &esc.resolution {
                esc_lines.push(Line::from(vec![
                    Span::raw("    Verdict: "),
                    Span::styled(res.as_str(), Style::default().fg(Color::DarkGray)),
                ]));
            }
            esc_lines.push(Line::raw(""));
        }
    }

    let esc_box = Paragraph::new(esc_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Human Escalation Notices (manor/escalations/) ")
            .border_style(Style::default().fg(if manor.escalations_open > 0 {
                Color::Yellow
            } else {
                Color::DarkGray
            })),
    );
    frame.render_widget(esc_box, body_chunks[1]);
}
