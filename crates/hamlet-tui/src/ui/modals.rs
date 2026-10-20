//! Intervention dialogs and modal popups (SPEC §12.2).

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::model::{AppState, MailFocus, ModalState, SteerFocus};

/// Helper to render a centered rectangle with percentages.
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

pub fn render_modals(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    match &state.modal {
        ModalState::None => {}
        ModalState::Help => render_help_modal(frame, area),
        ModalState::Steer {
            uid_input,
            directive_input,
            focus,
            submitting,
            error_msg,
        } => render_steer_modal(
            frame,
            area,
            uid_input,
            directive_input,
            *focus,
            *submitting,
            error_msg.as_deref(),
        ),
        ModalState::DecreeReview {
            decree_id,
            detail,
            scroll: _,
        } => render_decree_modal(frame, area, decree_id, detail.as_ref()),
        ModalState::EscalationInbox {
            selected,
            notices,
            resolving,
            resolution_input,
            submitting,
            status_msg,
        } => render_escalation_modal(
            frame,
            area,
            *selected,
            notices,
            *resolving,
            resolution_input,
            *submitting,
            status_msg.as_deref(),
        ),
        ModalState::MailCompose {
            to_input,
            subject_input,
            body_input,
            focus,
            submitting,
            status_msg,
        } => render_mail_compose_modal(
            frame,
            area,
            to_input,
            subject_input,
            body_input,
            *focus,
            *submitting,
            status_msg.as_deref(),
        ),
        ModalState::Detail {
            title,
            content,
            scroll: _,
        } => render_detail_modal(frame, area, title, content),
    }
}

fn render_help_modal(frame: &mut ratatui::Frame, area: Rect) {
    let popup_area = centered_rect(65, 75, area);
    frame.render_widget(Clear, popup_area);

    let text = vec![
        Line::from(Span::styled(
            "HAMLET MUD-STYLE TERMINAL INTERFACE",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Rendering exclusively over localhost daemon API & WebSocket event stream.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "ROOM GRAPH (SPEC §2 Normative Topology):",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(
            "  1: Guild    [guild/]             — Register, blueprints, commission ledger board",
        ),
        Line::from(
            "  2: Tavern   [tavern/]            — Gossip feed (/var/context/), patron stasis",
        ),
        Line::from(
            "  3: Courier  [courier/]           — Mail spools (/var/mail/), freight, dispatch",
        ),
        Line::from("  4: Market   [market/]            — Skills catalog, package cache telemetry"),
        Line::from(
            "  5: Keep     [keep/]              — Vault deliverables, archives, census parcels",
        ),
        Line::from("  6: Manor    [keep/census/manor/] — The Mayor UID 1000, decrees, escalations"),
        Line::raw(""),
        Line::from(Span::styled(
            "NAVIGATION & HOTKEYS:",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tab / Shift-Tab / ] [  — Cycle between rooms forward / backward"),
        Line::from("  1 - 6                  — Direct jump to room"),
        Line::from("  j / k / Down / Up      — Select item in active table"),
        Line::from("  Enter                  — Inspect selected item details"),
        Line::from("  s                      — Open Agent Steer dialog (inject directive)"),
        Line::from("  d                      — Review selected Mayor Decree"),
        Line::from("  e                      — Open Escalation Inbox (resolve with verdict)"),
        Line::from("  m                      — Compose and send Mail message"),
        Line::from("  f                      — Toggle Live WS Event Ticker filter"),
        Line::from("  r                      — Refresh all state projections from daemon"),
        Line::from("  ?                      — Open this help dialog"),
        Line::from("  Esc                    — Close modal / cancel"),
        Line::from("  q / Ctrl-C             — Exit Hamlet TUI"),
        Line::raw(""),
        Line::from(Span::styled(
            "Press Esc or Enter to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help & Keybindings ")
        .border_style(Style::default().fg(Color::Yellow));

    let p = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    frame.render_widget(p, popup_area);
}

fn render_steer_modal(
    frame: &mut ratatui::Frame,
    area: Rect,
    uid_input: &str,
    directive_input: &str,
    focus: SteerFocus,
    submitting: bool,
    error_msg: Option<&str>,
) {
    let popup_area = centered_rect(50, 45, area);
    frame.render_widget(Clear, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // UID input
            Constraint::Length(4), // Directive input
            Constraint::Length(2), // Submit / status
            Constraint::Length(1), // Footer hint
        ])
        .split(popup_area);

    let uid_style = if focus == SteerFocus::Uid {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let uid_box = Paragraph::new(uid_input).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Target Agent UID ")
            .border_style(uid_style),
    );
    frame.render_widget(uid_box, chunks[0]);

    let dir_style = if focus == SteerFocus::Directive {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let dir_box = Paragraph::new(directive_input).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Steering Directive (out-of-band JSON-RPC) ")
            .border_style(dir_style),
    );
    frame.render_widget(dir_box, chunks[1]);

    let submit_text = if submitting {
        Span::styled(
            "Submitting directive...",
            Style::default().fg(Color::Yellow),
        )
    } else if let Some(err) = error_msg {
        Span::styled(format!("Error: {err}"), Style::default().fg(Color::Red))
    } else if focus == SteerFocus::Submit {
        Span::styled(
            "[ SUBMIT DIRECTIVE ]",
            Style::default().fg(Color::Black).bg(Color::Green),
        )
    } else {
        Span::styled("[ Submit Directive ]", Style::default().fg(Color::DarkGray))
    };

    let submit_p = Paragraph::new(Line::from(submit_text)).alignment(Alignment::Center);
    frame.render_widget(submit_p, chunks[2]);

    let hint_p = Paragraph::new(Line::from(Span::styled(
        "Tab: switch focus │ Enter: submit │ Esc: cancel",
        Style::default().fg(Color::DarkGray),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(hint_p, chunks[3]);

    let border_style = Style::default().fg(Color::Cyan);
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .title(" Compose Steer Directive (SPEC §8 cl. 2) ")
        .border_style(border_style);
    frame.render_widget(outer_block, popup_area);
}

fn render_decree_modal(
    frame: &mut ratatui::Frame,
    area: Rect,
    decree_id: &str,
    detail: Option<&hamlet_core::api::DecreeDetailView>,
) {
    let popup_area = centered_rect(60, 65, area);
    frame.render_widget(Clear, popup_area);

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Decree ID: ", Style::default().fg(Color::Cyan)),
        Span::styled(
            decree_id,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    if let Some(d) = detail {
        lines.push(Line::from(vec![
            Span::styled("Sequence:  ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("{}", d.summary.seq)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Action:    ", Style::default().fg(Color::Cyan)),
            Span::styled(
                d.summary.action.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Status:    ", Style::default().fg(Color::Cyan)),
            Span::styled(d.summary.status.clone(), Style::default().fg(Color::Green)),
        ]));

        if let Some(ack) = &d.ack_verdict {
            lines.push(Line::from(vec![
                Span::styled("Ack Verdict: ", Style::default().fg(Color::Cyan)),
                Span::styled(ack.trim(), Style::default().fg(Color::White)),
            ]));
        }

        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Decree Body (JSON):",
            Style::default().fg(Color::Yellow),
        )));
        for l in d.raw_json.lines() {
            lines.push(Line::from(Span::styled(
                l,
                Style::default().fg(Color::Gray),
            )));
        }
    } else {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Loading decree detail from daemon...",
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Press Esc to close",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Mayor Decree Review (SPEC §7) ")
        .border_style(Style::default().fg(Color::Yellow));

    let p = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(p, popup_area);
}

#[allow(clippy::too_many_arguments)]
fn render_escalation_modal(
    frame: &mut ratatui::Frame,
    area: Rect,
    selected: usize,
    notices: &[hamlet_core::api::EscalationView],
    resolving: bool,
    resolution_input: &str,
    submitting: bool,
    status_msg: Option<&str>,
) {
    let popup_area = centered_rect(70, 70, area);
    frame.render_widget(Clear, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(6), // List of notices
            Constraint::Min(6),    // Selected notice details
            Constraint::Length(4), // Resolution input
            Constraint::Length(1), // Footer hint
        ])
        .split(popup_area);

    // 1. Notice selection list
    let mut list_lines = Vec::new();
    if notices.is_empty() {
        list_lines.push(Line::from(Span::styled(
            "(No escalations in manor/escalations/)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, esc) in notices.iter().enumerate() {
            let is_sel = i == selected;
            let status = if esc.resolved { "[RESOLVED]" } else { "[OPEN]" };
            let style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            list_lines.push(Line::from(vec![
                Span::styled(format!("{}. {status} ", i + 1), style),
                Span::styled(format!("{}: ", esc.id), style),
                Span::styled(esc.summary.clone(), style),
            ]));
        }
    }
    let list_box = Paragraph::new(list_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Escalation Notices (j/k to select) ")
            .border_style(Style::default().fg(Color::White)),
    );
    frame.render_widget(list_box, chunks[0]);

    // 2. Selected notice details
    let mut detail_lines = Vec::new();
    if let Some(esc) = notices.get(selected) {
        detail_lines.push(Line::from(vec![
            Span::styled("Notice ID: ", Style::default().fg(Color::Cyan)),
            Span::styled(
                esc.id.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ Decree ID: ", Style::default().fg(Color::Cyan)),
            Span::styled(esc.decree_id.clone(), Style::default().fg(Color::Yellow)),
        ]));
        detail_lines.push(Line::from(vec![
            Span::styled("Summary:   ", Style::default().fg(Color::Cyan)),
            Span::raw(esc.summary.clone()),
        ]));
        detail_lines.push(Line::from(vec![
            Span::styled("Evidence:  ", Style::default().fg(Color::Cyan)),
            Span::raw(esc.evidence_refs.join(", ")),
        ]));
        if let Some(res) = &esc.resolution {
            detail_lines.push(Line::from(vec![
                Span::styled(
                    "Verdict:   ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(res.clone()),
            ]));
        }
    }
    let detail_box = Paragraph::new(detail_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Notice Details ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(detail_box, chunks[1]);

    // 3. Resolution input
    let is_already_resolved = notices.get(selected).map(|e| e.resolved).unwrap_or(false);
    let res_style = if resolving && !is_already_resolved {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let input_title = if is_already_resolved {
        " Operator Resolution [ALREADY RESOLVED] "
    } else if resolving {
        " Type Operator Verdict (Enter to Submit) "
    } else {
        " Operator Resolution (Press 'r' to edit verdict) "
    };

    let res_box = Paragraph::new(resolution_input).block(
        Block::default()
            .borders(Borders::ALL)
            .title(input_title)
            .border_style(res_style),
    );
    frame.render_widget(res_box, chunks[2]);

    // 4. Footer
    let footer_text = if submitting {
        "Submitting resolution to daemon..."
    } else if let Some(msg) = status_msg {
        msg
    } else if resolving {
        "Enter: submit resolution │ Esc: cancel editing"
    } else {
        "j/k: navigate │ r: resolve selected notice │ Esc: close"
    };

    let footer_p = Paragraph::new(Line::from(Span::styled(
        footer_text,
        Style::default().fg(Color::DarkGray),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(footer_p, chunks[3]);

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .title(" Escalation Inbox (manor/escalations/ - ADR-0043) ")
        .border_style(Style::default().fg(Color::Yellow));
    frame.render_widget(outer_block, popup_area);
}

#[allow(clippy::too_many_arguments)]
fn render_mail_compose_modal(
    frame: &mut ratatui::Frame,
    area: Rect,
    to_input: &str,
    subject_input: &str,
    body_input: &str,
    focus: MailFocus,
    submitting: bool,
    status_msg: Option<&str>,
) {
    let popup_area = centered_rect(55, 55, area);
    frame.render_widget(Clear, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // To UID
            Constraint::Length(3), // Subject
            Constraint::Min(5),    // Body
            Constraint::Length(2), // Submit
            Constraint::Length(1), // Footer hint
        ])
        .split(popup_area);

    let to_style = if focus == MailFocus::To {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let to_box = Paragraph::new(to_input).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Recipient UID (1000 = Mayor, 1001-2000 = Agents) ")
            .border_style(to_style),
    );
    frame.render_widget(to_box, chunks[0]);

    let subj_style = if focus == MailFocus::Subject {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let subj_box = Paragraph::new(subject_input).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Subject ")
            .border_style(subj_style),
    );
    frame.render_widget(subj_box, chunks[1]);

    let body_style = if focus == MailFocus::Body {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let body_box = Paragraph::new(body_input).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Mail Body (/var/mail/ spool delivery) ")
            .border_style(body_style),
    );
    frame.render_widget(body_box, chunks[2]);

    let submit_text = if submitting {
        Span::styled("Delivering mail...", Style::default().fg(Color::Yellow))
    } else if let Some(msg) = status_msg {
        Span::styled(msg, Style::default().fg(Color::Green))
    } else if focus == MailFocus::Submit {
        Span::styled(
            "[ DELIVER MAIL ]",
            Style::default().fg(Color::Black).bg(Color::Green),
        )
    } else {
        Span::styled("[ Deliver Mail ]", Style::default().fg(Color::DarkGray))
    };

    let submit_p = Paragraph::new(Line::from(submit_text)).alignment(Alignment::Center);
    frame.render_widget(submit_p, chunks[3]);

    let hint_p = Paragraph::new(Line::from(Span::styled(
        "Tab: switch field │ Enter on Submit: deliver │ Esc: cancel",
        Style::default().fg(Color::DarkGray),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(hint_p, chunks[4]);

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .title(" Compose Mail (courier/mail/) ")
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(outer_block, popup_area);
}

fn render_detail_modal(frame: &mut ratatui::Frame, area: Rect, title: &str, content: &str) {
    let popup_area = centered_rect(65, 70, area);
    frame.render_widget(Clear, popup_area);

    let lines: Vec<Line> = content.lines().map(Line::from).collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(Style::default().fg(Color::Cyan));

    let p = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(p, popup_area);
}
