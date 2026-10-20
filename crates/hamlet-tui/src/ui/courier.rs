//! Courier Post view (SPEC §2 `courier/`).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};

use crate::model::AppState;

pub fn render_courier(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // MUD description
            Constraint::Min(8),    // Content panes
        ])
        .split(area);

    let courier = &state.data.landmarks.courier;

    // 1. MUD Description Box
    let desc_text = vec![
        Line::from(vec![
            Span::styled(
                "═══ Courier Post ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("[courier/] ", Style::default().fg(Color::Cyan)),
            Span::raw("— Spools: "),
            Span::styled(
                format!("{}", courier.spools.len()),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Freight Sandboxes: "),
            Span::styled(
                format!("{}", courier.freight_sandboxes),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Dispatch (Out/In/Dead): "),
            Span::styled(
                format!(
                    "{}/{}/{}",
                    courier.dispatch_outbox, courier.dispatch_inbox, courier.dispatch_dead
                ),
                Style::default().fg(Color::White),
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

    // 2. Main Content: Split Left (Mail spools) and Right (Messages & Dispatch)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    // Mail Spools Table
    let sel_idx = state.current_sel().list_index;
    let header_cells = ["UID", "Target", "Unread", "Read", "Status"]
        .iter()
        .map(|h| {
            Span::styled(
                *h,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header_row = Row::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<Row> = courier
        .spools
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let is_sel = i == sel_idx;
            let target_desc = if s.uid == 1000 { "Mayor" } else { "Agent" };

            let status_span = if s.unread > 0 {
                Span::styled(
                    format!("{} pending", s.unread),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled("idle", Style::default().fg(Color::DarkGray))
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
                Span::raw(format!("{}", s.uid)),
                Span::raw(target_desc),
                Span::raw(format!("{}", s.unread)),
                Span::raw(format!("{}", s.read)),
                status_span,
            ])
            .style(style)
        })
        .collect();

    let spools_table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(15),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(14),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Resident Mail Spools (courier/mail/) ")
            .border_style(Style::default().fg(Color::White)),
    );

    frame.render_widget(spools_table, body_chunks[0]);

    // Right Column: Split Top (Mayor/Recent Messages) and Bottom (Freight & Dispatch)
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(body_chunks[1]);

    // Mail Messages
    let mut mail_lines = Vec::new();
    mail_lines.push(Line::from(Span::styled(
        "Recent Mail In/Outbox [Press 'm' to Compose]:",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    mail_lines.push(Line::raw(""));

    if state.data.mail.is_empty() {
        mail_lines.push(Line::from(Span::styled(
            "(No mail messages for UID 1000)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for m in &state.data.mail {
            let read_indicator = if m.read { " " } else { "●" };
            let style = if m.read {
                Style::default().fg(Color::Gray)
            } else {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            };
            mail_lines.push(Line::from(vec![
                Span::styled(format!(" [{read_indicator}] "), style),
                Span::styled(
                    format!("From: {:<10} ", m.from),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(format!("Subj: {}", m.subject)),
            ]));
        }
    }

    let mail_box = Paragraph::new(mail_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Messages (Mayor UID 1000) ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(mail_box, right_chunks[0]);

    // Freight & Dispatch
    let mut infra_lines = Vec::new();
    infra_lines.push(Line::from(vec![
        Span::styled(
            "Freight Sandboxes (/var/freight/): ",
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(format!("{} active sandboxes", courier.freight_sandboxes)),
    ]));
    infra_lines.push(Line::from(vec![
        Span::styled(
            "Dispatch Webhook Outbox: ",
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(format!("{} pending requests", courier.dispatch_outbox)),
    ]));
    infra_lines.push(Line::from(vec![
        Span::styled(
            "Dispatch Webhook Inbox: ",
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(format!("{} received responses", courier.dispatch_inbox)),
    ]));
    infra_lines.push(Line::from(vec![
        Span::styled(
            "Dispatch Dead-Letter Queue: ",
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(format!("{} dropped packets", courier.dispatch_dead)),
    ]));

    let infra_box = Paragraph::new(infra_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Freight & Dispatch Subsystems ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(infra_box, right_chunks[1]);
}
