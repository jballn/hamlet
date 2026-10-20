//! Unit tests for pure view-model mapping in `hamlet-tui` (SPEC §12.2).

use hamlet_core::api::*;
use hamlet_core::sanitize::sanitize_name;
use hamlet_tui::demo::{demo_event_stream, demo_village_data};
use hamlet_tui::model::*;

#[test]
fn test_room_spec2_topology_and_navigation() {
    assert_eq!(Room::ALL.len(), 6);

    // Verify 1:1 mapping to SPEC §2 normative topology
    assert_eq!(Room::Guild.tree_path(), "guild/");
    assert_eq!(Room::Tavern.tree_path(), "tavern/");
    assert_eq!(Room::Courier.tree_path(), "courier/");
    assert_eq!(Room::Market.tree_path(), "market/");
    assert_eq!(Room::Keep.tree_path(), "keep/");
    assert_eq!(Room::Manor.tree_path(), "keep/census/manor/");

    // Verify cyclic navigation
    assert_eq!(Room::Guild.next(), Room::Tavern);
    assert_eq!(Room::Tavern.next(), Room::Courier);
    assert_eq!(Room::Courier.next(), Room::Market);
    assert_eq!(Room::Market.next(), Room::Keep);
    assert_eq!(Room::Keep.next(), Room::Manor);
    assert_eq!(Room::Manor.next(), Room::Guild);

    assert_eq!(Room::Guild.prev(), Room::Manor);
    assert_eq!(Room::Manor.prev(), Room::Keep);

    // Parse from string / number
    assert_eq!(Room::from_name("1"), Some(Room::Guild));
    assert_eq!(Room::from_name("guild"), Some(Room::Guild));
    assert_eq!(Room::from_name("6"), Some(Room::Manor));
    assert_eq!(Room::from_name("manor"), Some(Room::Manor));
    assert_eq!(Room::from_name("invalid"), None);
}

#[test]
fn test_room_ws_topic_filtering() {
    assert!(Room::Guild.matches_topic(WsTopic::Commission));
    assert!(!Room::Guild.matches_topic(WsTopic::Mail));

    assert!(Room::Courier.matches_topic(WsTopic::Mail));
    assert!(Room::Courier.matches_topic(WsTopic::Freight));
    assert!(!Room::Courier.matches_topic(WsTopic::Decree));

    assert!(Room::Manor.matches_topic(WsTopic::Decree));
    assert!(!Room::Manor.matches_topic(WsTopic::Mail));

    assert!(Room::Keep.matches_topic(WsTopic::Census));
    assert!(Room::Keep.matches_topic(WsTopic::Telemetry));
}

#[test]
fn test_ticker_state_and_event_mapping() {
    let mut ticker = TickerState::new(3);

    let ev1 = WsEvent::new(
        WsTopic::Mail,
        "mail_sent",
        1,
        1000,
        serde_json::json!({ "to": 1001, "subject": "Hello" }),
    );
    let ev2 = WsEvent::new(
        WsTopic::Commission,
        "commission_created",
        2,
        2000,
        serde_json::json!({ "slug": "alpha" }),
    );
    let ev3 = WsEvent::new(
        WsTopic::Decree,
        "decree_executed",
        3,
        3000,
        serde_json::json!({ "seq": 1 }),
    );
    let ev4 = WsEvent::new(
        WsTopic::Census,
        "agent_steered",
        4,
        4000,
        serde_json::json!({ "uid": 1002, "correlation": "c1" }),
    );

    ticker.push(TickerEvent::from(ev1));
    ticker.push(TickerEvent::from(ev2));
    ticker.push(TickerEvent::from(ev3));
    assert_eq!(ticker.events.len(), 3);
    assert_eq!(ticker.events[0].seq, 1);

    // Push 4th: capacity 3 causes oldest to drop
    ticker.push(TickerEvent::from(ev4));
    assert_eq!(ticker.events.len(), 3);
    assert_eq!(ticker.events[0].seq, 2);
    assert_eq!(ticker.events[2].seq, 4);

    // Unfiltered visible events
    ticker.filter_by_room = false;
    assert_eq!(ticker.visible_events(Room::Guild).len(), 3);

    // Filtered by room
    ticker.filter_by_room = true;
    let guild_events = ticker.visible_events(Room::Guild);
    assert_eq!(guild_events.len(), 1);
    assert_eq!(guild_events[0].event_kind, "commission_created");

    let keep_events = ticker.visible_events(Room::Keep);
    assert_eq!(keep_events.len(), 1);
    assert_eq!(keep_events[0].event_kind, "agent_steered");
}

#[test]
fn test_app_state_modal_transitions() {
    let mut state = AppState::new(Some(Room::Keep), true);
    assert_eq!(state.current_room, Room::Keep);
    assert_eq!(state.modal, ModalState::None);

    state.open_modal(ModalState::Help);
    assert_eq!(state.modal, ModalState::Help);

    state.close_modal();
    assert_eq!(state.modal, ModalState::None);

    state.open_modal(ModalState::Steer {
        uid_input: "1001".into(),
        directive_input: "optimize code".into(),
        focus: SteerFocus::Directive,
        submitting: false,
        error_msg: None,
    });
    assert!(matches!(state.modal, ModalState::Steer { .. }));

    state.close_modal();
    assert_eq!(state.modal, ModalState::None);
}

#[test]
fn test_demo_village_data_fidelity() {
    let data = demo_village_data();

    // Verify daemon status
    assert!(data.status.is_some());
    let st = data.status.unwrap();
    assert_eq!(st.pid, 4200);
    assert_eq!(st.active_agents, 2);

    // Verify landmarks
    assert_eq!(
        data.landmarks.guild.register,
        vec!["builder", "scout", "tester"]
    );
    assert_eq!(
        data.landmarks.tavern.patrons,
        vec!["code-oracle", "veteran-miner"]
    );
    assert_eq!(data.landmarks.courier.spools.len(), 3);
    assert_eq!(data.landmarks.market.skills.len(), 3);
    assert_eq!(data.landmarks.keep.census_parcels, vec![1001, 1002, 1003]);
    assert_eq!(data.landmarks.manor.mayor_incarnation, Some(4));

    // Verify commissions and indenture
    assert_eq!(data.commissions.len(), 2);
    let comm1 = &data.commissions[0];
    assert_eq!(comm1.slug.as_str(), "compiler-v1");
    let detail = data.commission_details.get("compiler-v1").unwrap();
    assert_eq!(detail.indenture_tasks.len(), 2);
    assert!(detail.indenture_tasks[0].done);
    assert!(!detail.indenture_tasks[1].done);

    // Verify demo events
    let events = demo_event_stream();
    assert_eq!(events.len(), 7);
}

#[tokio::test]
async fn test_app_interactive_key_handling() {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use hamlet_tui::app::{App, TuiOptions};

    let mut app = App::new(TuiOptions {
        dir: None,
        endpoint: None,
        token: None,
        initial_room: Some("guild".into()),
        demo: true,
    })
    .expect("demo app init");

    assert_eq!(app.state.current_room, Room::Guild);

    let key = |c: KeyCode| KeyEvent {
        code: c,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    };

    // 1. Cycle rooms with Tab
    app.handle_key(key(KeyCode::Tab)).await;
    assert_eq!(app.state.current_room, Room::Tavern);

    // 2. Direct jump to Room 6 (Manor)
    app.handle_key(key(KeyCode::Char('6'))).await;
    assert_eq!(app.state.current_room, Room::Manor);

    // 3. Open Help modal with '?'
    app.handle_key(key(KeyCode::Char('?'))).await;
    assert_eq!(app.state.modal, ModalState::Help);

    // 4. Close modal with Esc
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.state.modal, ModalState::None);

    // 5. Open Steer modal with 's'
    app.handle_key(key(KeyCode::Char('s'))).await;
    assert!(matches!(app.state.modal, ModalState::Steer { .. }));
    app.handle_key(key(KeyCode::Esc)).await;

    // 6. Open Escalation modal with 'e'
    app.handle_key(key(KeyCode::Char('e'))).await;
    assert!(matches!(
        app.state.modal,
        ModalState::EscalationInbox { .. }
    ));
    app.handle_key(key(KeyCode::Esc)).await;

    // 7. Toggle filter with 'f'
    assert!(!app.state.ticker.filter_by_room);
    app.handle_key(key(KeyCode::Char('f'))).await;
    assert!(app.state.ticker.filter_by_room);

    // 8. Quit with 'q'
    assert!(!app.state.should_quit);
    app.handle_key(key(KeyCode::Char('q'))).await;
    assert!(app.state.should_quit);
}

#[test]
fn test_reconnect_exponential_backoff_schedule() {
    use hamlet_tui::app::{App, TuiOptions};

    let mut app = App::new(TuiOptions {
        demo: true,
        ..Default::default()
    })
    .unwrap();

    // Verify exponential backoff growth and ceiling
    // attempt 1: 500 * 2^1 = 1000ms
    app.schedule_reconnect("test error 1".into());
    if let ConnectionStatus::Reconnecting {
        attempt,
        next_retry_ms,
        ..
    } = app.state.connection
    {
        assert_eq!(attempt, 1);
        assert_eq!(next_retry_ms, 1000);
    } else {
        panic!("expected Reconnecting state");
    }

    // attempt 2: 500 * 2^2 = 2000ms
    app.schedule_reconnect("test error 2".into());
    if let ConnectionStatus::Reconnecting {
        attempt,
        next_retry_ms,
        ..
    } = app.state.connection
    {
        assert_eq!(attempt, 2);
        assert_eq!(next_retry_ms, 2000);
    }

    // attempt 3: 500 * 2^3 = 4000ms
    app.schedule_reconnect("test error 3".into());
    if let ConnectionStatus::Reconnecting {
        attempt,
        next_retry_ms,
        ..
    } = app.state.connection
    {
        assert_eq!(attempt, 3);
        assert_eq!(next_retry_ms, 4000);
    }

    // High attempt capped at 10,000ms
    for i in 4..10 {
        app.schedule_reconnect(format!("test error {i}"));
    }
    if let ConnectionStatus::Reconnecting {
        attempt,
        next_retry_ms,
        ..
    } = app.state.connection
    {
        assert_eq!(attempt, 9);
        assert_eq!(next_retry_ms, 10_000);
    }
}

#[tokio::test]
async fn test_keyboard_operability_complete_keymap() {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use hamlet_tui::app::{App, TuiOptions};

    let mut app = App::new(TuiOptions {
        demo: true,
        ..Default::default()
    })
    .unwrap();

    let key = |code: KeyCode| KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    };

    // 1. Direct jumps 1-6
    for (ch, expected) in [
        ('1', Room::Guild),
        ('2', Room::Tavern),
        ('3', Room::Courier),
        ('4', Room::Market),
        ('5', Room::Keep),
        ('6', Room::Manor),
    ] {
        app.handle_key(key(KeyCode::Char(ch))).await;
        assert_eq!(app.state.current_room, expected);
    }

    // 2. Bracket room cycling '[' and ']'
    app.state.set_room(Room::Guild);
    app.handle_key(key(KeyCode::Char(']'))).await;
    assert_eq!(app.state.current_room, Room::Tavern);
    app.handle_key(key(KeyCode::Char('['))).await;
    assert_eq!(app.state.current_room, Room::Guild);

    // 3. Modals: 'd' for decree review, 'm' for mail compose
    app.handle_key(key(KeyCode::Char('d'))).await;
    assert!(matches!(app.state.modal, ModalState::DecreeReview { .. }));
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.state.modal, ModalState::None);

    app.handle_key(key(KeyCode::Char('m'))).await;
    assert!(matches!(app.state.modal, ModalState::MailCompose { .. }));
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.state.modal, ModalState::None);
}

#[test]
fn test_reconnect_fresh_state_clears_ghosts() {
    let mut state = AppState::new(Some(Room::Guild), true);
    state.data = demo_village_data();
    assert_eq!(state.data.commissions.len(), 2);

    // Simulate reconnect receiving fresh state with only 1 commission and new census
    let fresh_data = VillageData {
        commissions: vec![CommissionSummaryView {
            commission_id: 42,
            slug: sanitize_name("fresh-comm").unwrap(),
            state: "active".into(),
            roster: vec![1005],
            deliverables_count: 1,
            brief: true,
            indenture: true,
            git: "bare".into(),
        }],
        ..Default::default()
    };

    // State replacement leaves zero old/ghost commissions
    state.data = fresh_data;
    assert_eq!(state.data.commissions.len(), 1);
    assert_eq!(state.data.commissions[0].slug.as_str(), "fresh-comm");
    assert!(!state
        .data
        .commissions
        .iter()
        .any(|c| c.slug.as_str() == "compiler-v1"));
}
