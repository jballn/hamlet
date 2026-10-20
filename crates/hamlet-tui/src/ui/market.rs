//! Market view (SPEC §2 `market/`).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};

use crate::model::AppState;

pub fn render_market(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // MUD description
            Constraint::Min(8),    // Content panes
        ])
        .split(area);

    let market = &state.data.landmarks.market;
    let mb_size = (market.pkgcache_bytes as f64) / (1024.0 * 1024.0);

    // 1. MUD Description Box
    let desc_text = vec![
        Line::from(vec![
            Span::styled(
                "═══ The Open Bazaar ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("[market/] ", Style::default().fg(Color::Cyan)),
            Span::raw("— Skills: "),
            Span::styled(
                format!("{}", market.skills.len()),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ PkgCache Entries: "),
            Span::styled(
                format!("{}", market.pkgcache_entries),
                Style::default().fg(Color::White),
            ),
            Span::raw(" │ Cache Size: "),
            Span::styled(
                format!("{mb_size:.2} MB"),
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

    // 2. Main Content: Split Left (Skills Catalog) and Right (Package Cache Mirror Telemetry)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    // Skills Table
    let sel_idx = state.current_sel().list_index;
    let header_cells = ["Skill Name", "Type", "Scope"].iter().map(|h| {
        Span::styled(
            *h,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    });
    let header_row = Row::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<Row> = market
        .skills
        .iter()
        .enumerate()
        .map(|(i, s)| {
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
                Span::raw(s.clone()),
                Span::styled("SKILL.md", Style::default().fg(Color::Green)),
                Span::raw("agent-tool"),
            ])
            .style(style)
        })
        .collect();

    let skills_table = Table::new(
        rows,
        [
            Constraint::Length(25),
            Constraint::Length(15),
            Constraint::Length(15),
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Modular Skills Repository (market/skills/) ")
            .border_style(Style::default().fg(Color::White)),
    );

    frame.render_widget(skills_table, body_chunks[0]);

    // Package Cache Telemetry
    let mut cache_lines = Vec::new();
    cache_lines.push(Line::from(Span::styled(
        "Host Package Cache [Mount: /var/pkgcache/]:",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    cache_lines.push(Line::raw(""));

    cache_lines.push(Line::from(vec![
        Span::styled("Cached Package Entries: ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{}", market.pkgcache_entries)),
    ]));
    cache_lines.push(Line::from(vec![
        Span::styled("Total Mirror Volume:    ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{} bytes ({mb_size:.2} MB)", market.pkgcache_bytes)),
    ]));
    cache_lines.push(Line::from(vec![
        Span::styled("Mirror Mount Mode:      ", Style::default().fg(Color::Cyan)),
        Span::raw("Read-Only Bind Mount (SPEC §4)"),
    ]));
    cache_lines.push(Line::raw(""));
    cache_lines.push(Line::from(Span::styled(
        "Packages are shared across agent parcels without redundant downloads, \
         guaranteeing deterministic builds and offline resilience.",
        Style::default().fg(Color::Gray),
    )));

    let cache_box = Paragraph::new(cache_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Package Cache Telemetry (market/pkgcache/) ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(cache_box, body_chunks[1]);
}
