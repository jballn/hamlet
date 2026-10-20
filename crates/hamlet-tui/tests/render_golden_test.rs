//! Headless smoke rendering and golden frame tests for `hamlet-tui` (SPEC §12.2).

use hamlet_tui::demo::{demo_event_stream, demo_village_data};
use hamlet_tui::model::*;
use hamlet_tui::ui::render_app;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn render_state_to_text(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("init test terminal");
    terminal
        .draw(|f| render_app(f, state))
        .expect("render app frame");

    let mut out = String::new();
    let buf = terminal.backend().buffer();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = buf.cell((x, y)).expect("valid cell");
            out.push_str(cell.symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn test_render_all_six_rooms_headlessly() {
    let mut state = AppState::new(Some(Room::Guild), true);
    state.data = demo_village_data();
    for ev in demo_event_stream() {
        state.ticker.push(TickerEvent::from(ev));
    }

    // 1. Room 1: Guild Hall
    state.set_room(Room::Guild);
    let guild_frame = render_state_to_text(&state, 120, 35);
    assert!(guild_frame.contains("Guild Hall"));
    assert!(guild_frame.contains("[guild/]"));
    assert!(guild_frame.contains("Commissions Ledger Board"));
    assert!(guild_frame.contains("compiler-v1"));
    assert!(guild_frame.contains("Active"));
    assert!(guild_frame.contains("Indenture Task Breakdown"));
    assert!(guild_frame.contains("lexer and ast definitions"));
    assert!(guild_frame.contains("Registered Agents (guild/register/):"));
    assert!(guild_frame.contains("Blueprints (guild/blueprints/):"));
    assert!(guild_frame.contains("Live WS Event Feed"));

    // 2. Room 2: Tavern
    state.set_room(Room::Tavern);
    let tavern_frame = render_state_to_text(&state, 120, 35);
    assert!(tavern_frame.contains("Boar's Tusk Tavern"));
    assert!(tavern_frame.contains("[tavern/]"));
    assert!(tavern_frame.contains("Patrons in Stasis (tavern/patrons/)"));
    assert!(tavern_frame.contains("code-oracle"));
    assert!(tavern_frame.contains("veteran-miner"));
    assert!(tavern_frame.contains("Gossip Feed (tavern/gossip/)"));
    assert!(tavern_frame.contains("initial-brief.md"));

    // 3. Room 3: Courier
    state.set_room(Room::Courier);
    let courier_frame = render_state_to_text(&state, 120, 35);
    assert!(courier_frame.contains("Courier Post"));
    assert!(courier_frame.contains("[courier/]"));
    assert!(courier_frame.contains("Resident Mail Spools (courier/mail/)"));
    assert!(courier_frame.contains("Mayor"));
    assert!(courier_frame.contains("Agent"));
    assert!(courier_frame.contains("Messages (Mayor UID 1000)"));
    assert!(courier_frame.contains("Welcome to hamlet village"));
    assert!(courier_frame.contains("Freight & Dispatch Subsystems"));
    assert!(courier_frame.contains("Freight Sandboxes (/var/freight/):"));

    // 4. Room 4: Market
    state.set_room(Room::Market);
    let market_frame = render_state_to_text(&state, 120, 35);
    assert!(market_frame.contains("The Open Bazaar"));
    assert!(market_frame.contains("[market/]"));
    assert!(market_frame.contains("Modular Skills Repository (market/skills/)"));
    assert!(market_frame.contains("cargo-test"));
    assert!(market_frame.contains("git-rebase"));
    assert!(market_frame.contains("Package Cache Telemetry (market/pkgcache/)"));
    assert!(market_frame.contains("Total Mirror Volume:"));

    // 5. Room 5: Keep
    state.set_room(Room::Keep);
    let keep_frame = render_state_to_text(&state, 120, 35);
    assert!(keep_frame.contains("Stone Keep"));
    assert!(keep_frame.contains("[keep/]"));
    assert!(keep_frame.contains("Active Working Parcels (keep/census/)"));
    assert!(keep_frame.contains("scout"));
    assert!(keep_frame.contains("builder"));
    assert!(keep_frame.contains("Deliverable Vault"));
    assert!(keep_frame.contains("hamlet-core-v0.1.0.tar.gz"));
    assert!(keep_frame.contains("Historical Archives"));

    // 6. Room 6: Manor
    state.set_room(Room::Manor);
    let manor_frame = render_state_to_text(&state, 120, 35);
    assert!(manor_frame.contains("Mayor's Manor"));
    assert!(manor_frame.contains("[keep/census/manor/]"));
    assert!(manor_frame.contains("The Mayor (UID 1000 Control Loop)"));
    assert!(manor_frame.contains("MAYOR.md Present"));
    assert!(manor_frame.contains("Mayor Decrees (Press 'd' to Review)"));
    assert!(manor_frame.contains("provision_agent"));
    assert!(manor_frame.contains("decompose_commission"));
    assert!(manor_frame.contains("Human Escalation Notices (manor/escalations/)"));
    assert!(manor_frame.contains("parcel1002 cpu limit exceeded"));
}

#[test]
fn test_render_modals_headlessly() {
    let mut state = AppState::new(Some(Room::Guild), true);
    state.data = demo_village_data();

    // 1. Help modal
    state.open_modal(ModalState::Help);
    let help_frame = render_state_to_text(&state, 120, 35);
    assert!(help_frame.contains("HAMLET MUD-STYLE TERMINAL INTERFACE"));
    assert!(help_frame.contains("ROOM GRAPH (SPEC §2 Normative Topology)"));
    assert!(help_frame.contains("NAVIGATION & HOTKEYS"));

    // 2. Steer dialog
    state.open_modal(ModalState::Steer {
        uid_input: "1001".into(),
        directive_input: "re-run tests with verbose output".into(),
        focus: SteerFocus::Directive,
        submitting: false,
        error_msg: None,
    });
    let steer_frame = render_state_to_text(&state, 120, 35);
    assert!(steer_frame.contains("Compose Steer Directive"));
    assert!(steer_frame.contains("Target Agent UID"));
    assert!(steer_frame.contains("1001"));
    assert!(steer_frame.contains("re-run tests with verbose output"));

    // 3. Decree review dialog
    state.open_modal(ModalState::DecreeReview {
        decree_id: "d-3-0003".into(),
        detail: state.data.decree_details.get("d-3-0003").cloned(),
        scroll: 0,
    });
    let decree_frame = render_state_to_text(&state, 120, 35);
    assert!(decree_frame.contains("Mayor Decree Review"));
    assert!(decree_frame.contains("d-3-0003"));
    assert!(decree_frame.contains("escalate_to_human"));

    // 4. Escalation inbox dialog
    state.open_modal(ModalState::EscalationInbox {
        selected: 0,
        notices: state.data.escalations.clone(),
        resolving: false,
        resolution_input: String::new(),
        submitting: false,
        status_msg: None,
    });
    let esc_frame = render_state_to_text(&state, 120, 35);
    assert!(esc_frame.contains("Escalation Inbox"));
    assert!(esc_frame.contains("d-3-0003.json"));
    assert!(esc_frame.contains("parcel1002 cpu limit exceeded"));

    // 5. Mail compose dialog
    state.open_modal(ModalState::MailCompose {
        to_input: "1002".into(),
        subject_input: "Urgent fix required".into(),
        body_input: "Please check parser error handling.".into(),
        focus: MailFocus::Body,
        submitting: false,
        status_msg: None,
    });
    let mail_frame = render_state_to_text(&state, 120, 35);
    assert!(mail_frame.contains("Compose Mail"));
    assert!(mail_frame.contains("1002"));
    assert!(mail_frame.contains("Urgent fix required"));
    assert!(mail_frame.contains("Please check parser error handling."));

    // 6. Detail modal
    state.open_modal(ModalState::Detail {
        title: "Census Lease: UID 1001".into(),
        content: "Identity: scout\nState: live\nCommission: compiler-v1".into(),
        scroll: 0,
    });
    let detail_frame = render_state_to_text(&state, 120, 35);
    assert!(detail_frame.contains("Census Lease: UID 1001"));
    assert!(detail_frame.contains("Identity: scout"));
}
