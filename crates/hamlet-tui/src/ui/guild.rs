//! Guild Hall view (SPEC §2 `guild/`).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};

use crate::model::AppState;

pub fn render_guild(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // MUD description
            Constraint::Min(8),    // Content panes
        ])
        .split(area);

    // 1. MUD Description Box
    let desc_text = vec![
        Line::from(vec![
            Span::styled(
                "═══ Guild Hall ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("[guild/] ", Style::default().fg(Color::Cyan)),
            Span::raw("— Register: "),
            Span::styled(
                format!("{}", state.data.landmarks.guild.register.len()),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Blueprints: "),
            Span::styled(
                format!("{}", state.data.landmarks.guild.blueprints.len()),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Commissions: "),
            Span::styled(
                format!("{}", state.data.landmarks.guild.ledger_commissions),
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

    // 2. Main Content: Split Left (Commissions board) and Right (Indenture & Blueprints)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(chunks[1]);

    // Commissions Table
    let sel_idx = state.current_sel().list_index;
    let header_cells = ["ID", "Slug", "State", "Roster", "Brief", "Git"]
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

    let rows: Vec<Row> = state
        .data
        .commissions
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let is_sel = i == sel_idx;
            let roster_str = c
                .roster
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let brief_str = if c.brief { "yes" } else { "no" };

            let style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            Row::new(vec![
                Span::raw(format!("{}", c.commission_id)),
                Span::raw(c.slug.to_string()),
                Span::raw(c.state.clone()),
                Span::raw(if roster_str.is_empty() {
                    "-".into()
                } else {
                    roster_str
                }),
                Span::raw(brief_str),
                Span::raw(c.git.clone()),
            ])
            .style(style)
        })
        .collect();

    let comm_table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(6),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Commissions Ledger Board (guild/ledger/) ")
            .border_style(Style::default().fg(Color::White)),
    );

    frame.render_widget(comm_table, body_chunks[0]);

    // Right Column: Split Top (Indenture tasks) and Bottom (Registry & Blueprints)
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(body_chunks[1]);

    // Indenture tasks for selected commission
    let selected_comm = state.data.commissions.get(sel_idx);
    let mut indenture_lines = Vec::new();
    if let Some(comm) = selected_comm {
        if let Some(detail) = state.data.commission_details.get(comm.slug.as_str()) {
            if let Some(b) = &detail.brief {
                indenture_lines.push(Line::from(vec![
                    Span::styled("Brief: ", Style::default().fg(Color::Cyan)),
                    Span::styled(b.as_str(), Style::default().fg(Color::Gray)),
                ]));
                indenture_lines.push(Line::raw(""));
            }

            indenture_lines.push(Line::from(Span::styled(
                "INDENTURE.md Contracts:",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));

            for task in &detail.indenture_tasks {
                let status_color = if task.done {
                    Color::Green
                } else if task.status == "Busy" {
                    Color::Yellow
                } else {
                    Color::White
                };
                let id_str = task
                    .identity
                    .as_ref()
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| task.uid.to_string());
                indenture_lines.push(Line::from(vec![
                    Span::styled(
                        format!(" [{}] ", if task.done { "✓" } else { " " }),
                        Style::default().fg(status_color),
                    ),
                    Span::styled(format!("{id_str}: "), Style::default().fg(Color::Cyan)),
                    Span::raw(task.task.clone()),
                    Span::styled(
                        format!(" ({})", task.status),
                        Style::default().fg(status_color),
                    ),
                ]));
            }
        } else {
            indenture_lines.push(Line::from(Span::styled(
                format!("Press Enter to inspect INDENTURE.md for '{}'", comm.slug),
                Style::default().fg(Color::DarkGray),
            )));
        }
    } else {
        indenture_lines.push(Line::from(Span::styled(
            "No commission selected.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let indenture_box = Paragraph::new(indenture_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Indenture Task Breakdown ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(indenture_box, right_chunks[0]);

    // Registry & Blueprints summary
    let reg_blue_lines = vec![
        Line::from(vec![
            Span::styled(
                "Registered Agents (guild/register/): ",
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(state.data.landmarks.guild.register.join(", ")),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                "Blueprints (guild/blueprints/): ",
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(state.data.landmarks.guild.blueprints.join(", ")),
        ]),
    ];

    let reg_box = Paragraph::new(reg_blue_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Registry & Blueprints ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(reg_box, right_chunks[1]);
}
