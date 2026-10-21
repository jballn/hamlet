//! Adversarial frontend view-model test suite (SPEC §12.3; ADR-0044; CONVENTIONS §2, §3, §9).
//!
//! Covers:
//! 1. State fidelity: scripted village activity through 5 checkpoints
//!    (initial -> commission -> work -> submit/promote -> hibernate)
//!    asserting that WS state deltas reconstruct a view model matching CLI golden vectors.
//! 2. Honesty rules: client-side view-model rendering of owner-only private data is impossible.
//! 3. Optimistic UI rollback on command rejection.
//! 4. Repro QA09-03: View-model ignores sequence-number gaps without triggering resync.

use std::collections::BTreeMap;

use hamlet_core::api::*;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::slots::MAYOR_UID;
use hamlet_web::panels::{ActivePanel, MutationStatus, PanelKind};
use hamlet_web::view_model::VillageViewModel;

fn safe(s: &str) -> hamlet_core::sanitize::SafeName {
    sanitize_name(s).expect("valid safe name")
}

#[test]
fn test_adversarial_view_model_state_fidelity_five_checkpoints() {
    let mut vm = VillageViewModel::new();
    let instance_id = "daemon-fidelity-inst-1";

    // -----------------------------------------------------------------------
    // Checkpoint 1: Initial empty village
    // -----------------------------------------------------------------------
    let mayor_record = CensusRecordView {
        uid: MAYOR_UID,
        identity: safe("mayor"),
        commission: None,
        commission_slug: None,
        state: "running".into(),
        stasis: false,
        session_count: 1,
        counters: ApiCensusCounters::default(),
        last_event: None,
        last_event_ts: None,
        title_digest: "digest-mayor".into(),
    };
    let snap1 = StateDeltaFrame::snapshot(
        instance_id,
        1,
        1_700_000_000,
        vec![mayor_record.clone()],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
        LandmarksView::default(),
        Vec::new(),
    );
    assert!(vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(snap1))));

    assert_eq!(vm.last_seq, 1);
    assert_eq!(vm.census.len(), 1);
    assert!(vm.census.contains_key(&MAYOR_UID));
    assert_eq!(vm.commissions.len(), 0);
    assert_eq!(vm.vault.len(), 0);
    assert_eq!(vm.decrees.len(), 0);

    let scene1 = vm.generate_scene();
    assert_eq!(scene1.sprites.len(), 1);
    assert_eq!(scene1.sprites[0].uid, MAYOR_UID);
    assert_eq!(scene1.hud.active_agents, 0); // running != active lease
    assert_eq!(scene1.hud.active_commissions, 0);

    // -----------------------------------------------------------------------
    // Checkpoint 2: Commission registered (watchtower)
    // -----------------------------------------------------------------------
    let comm1 = CommissionSummaryView {
        commission_id: 1,
        slug: safe("watchtower"),
        state: "draft".into(),
        roster: Vec::new(),
        deliverables_count: 0,
        brief: true,
        indenture: false,
        git: "commit-init".into(),
    };
    let mut delta2 = StateDeltaFrame::diff(instance_id, 2, 1_700_000_100);
    delta2.commissions = Some(vec![comm1.clone()]);
    assert!(vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta2))));

    assert_eq!(vm.last_seq, 2);
    assert_eq!(vm.commissions.len(), 1);
    assert_eq!(vm.commissions[0].slug.as_str(), "watchtower");
    assert_eq!(vm.commissions[0].state, "draft");

    vm.open_panel(PanelKind::Guild, None);
    if let ActivePanel::Guild(board) = vm.generate_panel_view() {
        assert_eq!(board.commissions.len(), 1);
        assert_eq!(board.active_count, 0); // draft is not active
        assert_eq!(board.draft_count, 1);
        assert_eq!(board.commissions[0].slug.as_str(), "watchtower");
    } else {
        panic!("expected GuildBoardView panel");
    }

    // -----------------------------------------------------------------------
    // Checkpoint 3: Worker assigned to parcel 1001 (working state)
    // -----------------------------------------------------------------------
    let worker_record = CensusRecordView {
        uid: 1001,
        identity: safe("builder"),
        commission: Some(1),
        commission_slug: Some(safe("watchtower")),
        state: "working".into(),
        stasis: false,
        session_count: 3,
        counters: ApiCensusCounters {
            input_tokens: 500,
            output_tokens: 250,
            cost_micro_usd: 12500,
            traces: 2,
        },
        last_event: Some("pi_tool_call".into()),
        last_event_ts: Some("1700000200".into()),
        title_digest: "digest-builder".into(),
    };
    let mut comm1_active = comm1.clone();
    comm1_active.state = "Active".into();
    comm1_active.roster = vec![1001];

    let mut delta3 = StateDeltaFrame::diff(instance_id, 3, 1_700_000_200);
    delta3.census = Some(vec![mayor_record.clone(), worker_record.clone()]);
    delta3.commissions = Some(vec![comm1_active]);
    assert!(vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta3))));

    assert_eq!(vm.last_seq, 3);
    assert_eq!(vm.census.len(), 2);
    assert_eq!(vm.census.get(&1001).unwrap().state, "working");

    let scene3 = vm.generate_scene();
    assert_eq!(scene3.sprites.len(), 2);
    let worker_sprite = scene3
        .sprites
        .iter()
        .find(|s| s.uid == 1001)
        .expect("worker sprite");
    assert_eq!(worker_sprite.anim_state, "working");

    vm.open_panel(PanelKind::Agent, Some("1001".into()));
    if let ActivePanel::Agent(agent_view) = vm.generate_panel_view() {
        assert_eq!(agent_view.uid, 1001);
        assert_eq!(agent_view.name, "builder");
        assert_eq!(agent_view.state, "working");
    } else {
        panic!("expected AgentPanelView");
    }

    // -----------------------------------------------------------------------
    // Checkpoint 4: Deliverable staged in freight & promoted into Keep Vault
    // -----------------------------------------------------------------------
    let vault_item = VaultItemView {
        name: "beacon-release.tar".into(),
        commission: Some(safe("watchtower")),
        sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(),
        size_bytes: 4096,
    };
    let mut delta4 = StateDeltaFrame::diff(instance_id, 4, 1_700_000_300);
    delta4.vault = Some(vec![vault_item.clone()]);
    assert!(vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta4))));

    assert_eq!(vm.last_seq, 4);
    assert_eq!(vm.vault.len(), 1);
    assert_eq!(vm.vault[0].name, "beacon-release.tar");
    assert_eq!(vm.vault[0].size_bytes, 4096);

    vm.open_panel(PanelKind::Vault, None);
    if let ActivePanel::Vault(vault_panel) = vm.generate_panel_view() {
        assert_eq!(vault_panel.artifacts.len(), 1);
        assert_eq!(vault_panel.artifacts[0].name, "beacon-release.tar");
        assert_eq!(vault_panel.total_count, 1);
        assert_eq!(vault_panel.total_bytes, 4096);
    } else {
        panic!("expected VaultPanelView");
    }

    // -----------------------------------------------------------------------
    // Checkpoint 5: Worker hibernated into Tavern stasis
    // -----------------------------------------------------------------------
    let mut worker_hibernated = worker_record.clone();
    worker_hibernated.state = "stasis".into();
    worker_hibernated.stasis = true;

    let mut delta5 = StateDeltaFrame::diff(instance_id, 5, 1_700_000_400);
    delta5.census = Some(vec![mayor_record, worker_hibernated]);
    assert!(vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta5))));

    assert_eq!(vm.last_seq, 5);
    assert_eq!(vm.census.get(&1001).unwrap().state, "stasis");

    let scene5 = vm.generate_scene();
    let hibernated_sprite = scene5
        .sprites
        .iter()
        .find(|s| s.uid == 1001)
        .expect("worker sprite");
    assert_eq!(hibernated_sprite.anim_state, "sleeping");
}

#[test]
fn test_adversarial_view_model_honesty_isolation() {
    let mut vm = VillageViewModel::new();

    // Courier panel: contains only mail counts, zero mail bodies
    vm.mail_counts.insert(1001, 5);
    vm.open_panel(PanelKind::Courier, None);
    if let ActivePanel::Courier(courier) = vm.generate_panel_view() {
        assert_eq!(courier.total_unread, 5);
        assert_eq!(courier.mail_counts.get(&1001), Some(&5));
    } else {
        panic!("expected CourierPanelView");
    }

    // Agent panel: displays identity and lease, zero secret env vars
    let rec = CensusRecordView {
        uid: 1001,
        identity: safe("scout"),
        commission: None,
        commission_slug: None,
        state: "idle".into(),
        stasis: false,
        session_count: 1,
        counters: ApiCensusCounters::default(),
        last_event: None,
        last_event_ts: None,
        title_digest: "digest-scout".into(),
    };
    vm.census.insert(1001, rec);
    vm.open_panel(PanelKind::Agent, Some("1001".into()));

    let panel_view = vm.generate_panel_view();
    let panel_json = serde_json::to_string(&panel_view).unwrap();
    assert!(!panel_json.contains("HAMLET_ANTHROPIC_KEY"));
    assert!(!panel_json.contains("sk-ant-"));
    assert!(!panel_json.contains("secret"));

    // Bubble honesty: raw mail snippets clamped to 64 chars
    let long_snippet = "This is a super long private mail snippet that should be bounded strictly on client display";
    let bubble = BubbleFrame::mail_snippet(10, 1000, 1000, 1001, "Test", long_snippet);
    vm.apply_server_frame(WsServerFrame::Bubble(bubble));

    let scene = vm.generate_scene();
    for b in scene.bubbles {
        assert!(b.text.len() <= MAX_BUBBLE_TEXT_LEN);
    }
}

#[test]
fn test_adversarial_view_model_optimistic_ui_rollback_on_command_rejection() {
    let mut vm = VillageViewModel::new();

    // 1. Client registers optimistic mutation
    vm.record_optimistic_command("corr-comm-fail", "Create watchtower", "watchtower");
    assert_eq!(
        vm.pending_mutations.get("corr-comm-fail").unwrap().status,
        MutationStatus::Pending
    );

    // 2. Server returns CommandAck rejection
    let ack_err = CommandAckFrame {
        seq: 2,
        timestamp_unix_ms: 1_700_000_100,
        correlation_id: "corr-comm-fail".into(),
        command_endpoint: "/api/v1/commissions/new".into(),
        success: false,
        status_code: 400,
        payload: None,
        error: Some("Commission 'watchtower' already exists".into()),
    };
    assert!(vm.apply_server_frame(WsServerFrame::CommandAck(ack_err)));

    // 3. View model rolled back optimistic state
    let mutation = vm.pending_mutations.get("corr-comm-fail").unwrap();
    assert_eq!(mutation.status, MutationStatus::RolledBack);
    assert_eq!(
        mutation.error_message.as_deref(),
        Some("Commission 'watchtower' already exists")
    );
    assert_eq!(
        vm.last_mutation_error.as_deref(),
        Some("Commission 'watchtower' already exists")
    );

    // 4. Village ticker recorded rollback
    assert!(
        vm.ticker_history
            .iter()
            .any(|entry| entry.contains("[ROLLBACK]") && entry.contains("400")),
        "ticker must record rollback event"
    );
}

/// QA09-03 Repro: VillageViewModel accepts incoming frames with sequence gaps without detecting gap
/// or requesting snapshot resync.
/// SPEC Citation: Sprint 09 Step 5 Committed Suite 1: "sequence-number gaps trigger resync".
#[test]
fn test_repro_qa09_03_view_model_sequence_gap_ignored() {
    let mut vm = VillageViewModel::new();

    // Initial snapshot at seq == 1
    let snap = StateDeltaFrame::snapshot(
        "inst-1",
        1,
        1_700_000_000,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
        LandmarksView::default(),
        Vec::new(),
    );
    vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(snap)));
    assert_eq!(vm.last_seq, 1);

    // Frame seq jumps from 1 to 10 (9 dropped diffs in transit)
    let mut delta_gap = StateDeltaFrame::diff("inst-1", 10, 1_700_000_100);
    delta_gap.decrees = Some(Vec::new());

    // Expectation: View model should flag sequence gap and trigger resync requirement.
    let applied = vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta_gap)));
    assert!(applied);
    assert_eq!(vm.last_seq, 10);
    assert!(
        vm.resync_needed(),
        "sequence gap (seq 1 -> 10) must flag resync requirement"
    );
    assert!(
        vm.ticker_history
            .iter()
            .any(|entry| entry.contains("[GAP]")),
        "ticker history must record detected sequence gap"
    );

    // Resync via snapshot clears resync_needed flag
    let snap2 = StateDeltaFrame::snapshot(
        "inst-1",
        11,
        1_700_000_200,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
        LandmarksView::default(),
        Vec::new(),
    );
    assert!(vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(snap2))));
    assert!(
        !vm.resync_needed(),
        "snapshot resync must clear resync_needed"
    );
}
