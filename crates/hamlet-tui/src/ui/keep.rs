//! Keep view (SPEC §2 `keep/`).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};

use crate::model::AppState;

pub fn render_keep(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // MUD description
            Constraint::Min(8),    // Content panes
        ])
        .split(area);

    let keep = &state.data.landmarks.keep;

    // 1. MUD Description Box
    let desc_text = vec![
        Line::from(vec![
            Span::styled(
                "═══ Stone Keep ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("[keep/] ", Style::default().fg(Color::Cyan)),
            Span::raw("— Vault Items: "),
            Span::styled(
                format!("{}", keep.vault_items),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Archives: "),
            Span::styled(
                format!("{}", keep.archives.len()),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Active Parcels: "),
            Span::styled(
                format!("{}", state.data.census.len()),
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

    // 2. Main Content: Split Left (Census Parcels) and Right (Vault & Archives)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(chunks[1]);

    // Census Parcels Live Telemetry Table
    let sel_idx = state.current_sel().list_index;
    let header_cells = [
        "UID", "Identity", "State", "Comm", "Tokens", "Cost", "Event",
    ]
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
        .census
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let is_sel = i == sel_idx;
            let comm_str = c
                .commission_slug
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    c.commission
                        .map(|id| format!("#{id}"))
                        .unwrap_or_else(|| "-".into())
                });

            let state_color = match c.state.as_str() {
                "live" | "busy" => Color::Green,
                "idle" => Color::Cyan,
                "stasis" => Color::Magenta,
                "crashed" => Color::Red,
                _ => Color::White,
            };

            let tokens_str = format!(
                "{}/{}",
                c.counters.input_tokens / 1000,
                c.counters.output_tokens / 1000
            );
            let cost_str = format!("${:.2}", (c.counters.cost_micro_usd as f64) / 1_000_000.0);
            let last_ev = c.last_event.as_deref().unwrap_or("-");

            let style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            Row::new(vec![
                Span::raw(format!("{}", c.uid)),
                Span::raw(c.identity.to_string()),
                Span::styled(c.state.clone(), Style::default().fg(state_color)),
                Span::raw(comm_str),
                Span::raw(tokens_str),
                Span::raw(cost_str),
                Span::raw(last_ev),
            ])
            .style(style)
        })
        .collect();

    let census_table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(14),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Active Working Parcels (keep/census/) ")
            .border_style(Style::default().fg(Color::White)),
    );

    frame.render_widget(census_table, body_chunks[0]);

    // Right Column: Split Top (Vault) and Bottom (Archives)
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(body_chunks[1]);

    // Promoted Deliverables in Vault
    let mut vault_lines = Vec::new();
    vault_lines.push(Line::from(Span::styled(
        "Promoted Deliverables (keep/vault/):",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    vault_lines.push(Line::raw(""));

    if state.data.vault.is_empty() {
        vault_lines.push(Line::from(Span::styled(
            "(No promoted deliverables in vault)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for v in &state.data.vault {
            let comm = v
                .commission
                .as_ref()
                .map(|c| c.as_str())
                .unwrap_or("village");
            let size_kb = (v.size_bytes as f64) / 1024.0;
            let sha_short = if v.sha256.len() > 12 {
                &v.sha256[..12]
            } else {
                &v.sha256
            };
            vault_lines.push(Line::from(vec![
                Span::styled(" ★ ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    v.name.clone(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" [{comm}] {size_kb:.1} KB (sha: {sha_short})")),
            ]));
        }
    }

    let vault_box = Paragraph::new(vault_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Deliverable Vault ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(vault_box, right_chunks[0]);

    // Archives
    let mut archive_lines = Vec::new();
    archive_lines.push(Line::from(Span::styled(
        "Execution Archives (keep/archives/):",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    archive_lines.push(Line::raw(""));

    if keep.archives.is_empty() {
        archive_lines.push(Line::from(Span::styled(
            "(No historical archives)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for arch in &keep.archives {
            archive_lines.push(Line::from(vec![
                Span::styled(" • ", Style::default().fg(Color::DarkGray)),
                Span::raw(arch.clone()),
            ]));
        }
    }

    let arch_box = Paragraph::new(archive_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Historical Archives ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(arch_box, right_chunks[1]);
}
