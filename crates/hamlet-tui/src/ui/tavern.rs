//! Tavern view (SPEC §2 `tavern/`).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};

use crate::model::AppState;

pub fn render_tavern(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
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
                "═══ Boar's Tusk Tavern ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("[tavern/] ", Style::default().fg(Color::Cyan)),
            Span::raw("— Patrons in Stasis: "),
            Span::styled(
                format!("{}", state.data.landmarks.tavern.patrons.len()),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Gossip Documents: "),
            Span::styled(
                format!("{}", state.data.landmarks.tavern.gossip.len()),
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

    // 2. Main Content: Split Left (Patrons in Stasis) and Right (Gossip Feed)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    // Patrons Table (hibernated patrons from census or landmark)
    let sel_idx = state.current_sel().list_index;
    let patrons = &state.data.landmarks.tavern.patrons;

    let header_cells = ["Patron Name", "Status", "Home Backup"].iter().map(|h| {
        Span::styled(
            *h,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    });
    let header_row = Row::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<Row> = patrons
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let is_sel = i == sel_idx;
            let style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            Row::new(vec![
                Span::raw(p.clone()),
                Span::styled("hibernated (stasis)", Style::default().fg(Color::Magenta)),
                Span::raw(format!("home.{}.bak", p)),
            ])
            .style(style)
        })
        .collect();

    let patrons_table = Table::new(
        rows,
        [
            Constraint::Length(20),
            Constraint::Length(22),
            Constraint::Length(20),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Patrons in Stasis (tavern/patrons/) ")
            .border_style(Style::default().fg(Color::White)),
    );

    frame.render_widget(patrons_table, body_chunks[0]);

    // Gossip Feed (read-only player / user context)
    let gossip_files = &state.data.landmarks.tavern.gossip;
    let mut gossip_lines = Vec::new();
    gossip_lines.push(Line::from(Span::styled(
        "Context Feed [Mount: /var/context/]:",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    gossip_lines.push(Line::raw(""));

    if gossip_files.is_empty() {
        gossip_lines.push(Line::from(Span::styled(
            "(No gossip files in tavern/gossip/)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for file in gossip_files {
            gossip_lines.push(Line::from(vec![
                Span::styled(" • ", Style::default().fg(Color::Cyan)),
                Span::styled(file.clone(), Style::default().fg(Color::White)),
                Span::styled(
                    " (read-only user input)",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }

    let gossip_box = Paragraph::new(gossip_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Gossip Feed (tavern/gossip/) ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(gossip_box, body_chunks[1]);
}
