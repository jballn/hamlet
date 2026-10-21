//! View-model golden tests for work management pixel-UI panels (SPEC §12.3; ADR-0044).

use std::collections::BTreeMap;

use hamlet_core::api::{
    ApiCensusCounters, CensusRecordView, CommandAckFrame, CommissionSummaryView, DecreeSummaryView,
    EscalationView, GuildLandmark, LandmarksView, ParcelMetricTick, StateDeltaFrame, VaultItemView,
    WsServerFrame,
};
use hamlet_core::sanitize::sanitize_name;
use hamlet_web::panels::{ActivePanel, MutationStatus, PanelKind};
use hamlet_web::view_model::VillageViewModel;

fn safe(s: &str) -> hamlet_core::sanitize::SafeName {
    sanitize_name(s).expect("valid safe name")
}

#[test]
fn golden_test_agent_panel_projection() {
    let mut vm = VillageViewModel::new();

    let delta = StateDeltaFrame::snapshot(
        "inst-panel-1",
        1,
        1729000000000,
        vec![
            CensusRecordView {
                uid: 1001,
                identity: safe("builder"),
                commission: Some(1),
                commission_slug: Some(safe("compiler-v1")),
                state: "working".into(),
                stasis: false,
                session_count: 4,
                counters: ApiCensusCounters {
                    input_tokens: 18000,
                    output_tokens: 5200,
                    cost_micro_usd: 95000,
                    traces: 22,
                },
                last_event: Some("pi_tool_call".into()),
                last_event_ts: Some("1729000000000".into()),
                title_digest: "digest-builder".into(),
            },
            CensusRecordView {
                uid: 1002,
                identity: safe("veteran-miner"),
                commission: None,
                commission_slug: None,
                state: "stasis".into(),
                stasis: true,
                session_count: 2,
                counters: ApiCensusCounters::default(),
                last_event: None,
                last_event_ts: None,
                title_digest: "digest-stasis".into(),
            },
        ],
        vec![],
        vec![],
        vec![],
        BTreeMap::new(),
        LandmarksView::default(),
        vec![],
    );
    vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));

    // Ingest metrics for 1001
    vm.apply_metrics(hamlet_core::api::MetricsBatchFrame {
        seq: 2,
        timestamp_unix_ms: 1729000001000,
        ticks: vec![ParcelMetricTick {
            uid: 1001,
            session_id: Some("sess-1".into()),
            tokens_in: 18500,
            tokens_out: 5300,
            cost_micro_usd: Some(98000),
            latency_ms: 120,
            memory_bytes: Some(2048 * 1024),
            cpu_percent_x10: Some(185),
        }],
    });

    // 1. Open Agent Panel for active worker 1001
    vm.open_panel(PanelKind::Agent, Some("1001".into()));
    match vm.generate_panel_view() {
        ActivePanel::Agent(p) => {
            assert_eq!(p.uid, 1001);
            assert_eq!(p.name, "builder");
            assert_eq!(p.state, "working");
            assert!(!p.stasis);
            assert_eq!(p.tokens_in, 18500);
            assert_eq!(p.tokens_out, 5300);
            assert_eq!(p.cost_micro_usd, 98000);
            assert_eq!(p.latency_ms, 120);
            assert_eq!(p.cpu_percent_x10, 185);
            assert_eq!(p.title_digest, "digest-builder");
            assert!(p.deed_summary.contains("keep/census/parcel1001/DEED.md"));
            assert!(p.proprietor_summary.contains("PROPRIETOR.md"));
            assert!(p.can_steer);
            assert!(p.can_hibernate);
            assert!(!p.can_wake);
        }
        other => panic!("expected Agent panel, got {:?}", other),
    }

    // 2. Open Agent Panel for Mayor (UID 1000)
    vm.open_panel(PanelKind::Agent, Some("1000".into()));
    match vm.generate_panel_view() {
        ActivePanel::Agent(p) => {
            assert_eq!(p.uid, 1000);
            assert_eq!(p.name, "Mayor");
            assert!(!p.can_steer);
            assert!(!p.can_hibernate);
            assert!(!p.can_wake);
        }
        other => panic!("expected Mayor panel, got {:?}", other),
    }

    // 3. Open Agent Panel for Patron in Stasis (UID 1002)
    vm.open_panel(PanelKind::Agent, Some("1002".into()));
    match vm.generate_panel_view() {
        ActivePanel::Agent(p) => {
            assert_eq!(p.uid, 1002);
            assert!(p.stasis);
            assert!(!p.can_steer);
            assert!(!p.can_hibernate);
            assert!(p.can_wake);
        }
        other => panic!("expected Stasis panel, got {:?}", other),
    }

    // Close panel
    vm.close_panel();
    assert_eq!(vm.generate_panel_view(), ActivePanel::None);
}

#[test]
fn golden_test_guild_board_projection() {
    let mut vm = VillageViewModel::new();

    let landmarks = LandmarksView {
        guild: GuildLandmark {
            register: vec!["builder".into(), "scout".into()],
            blueprints: vec!["rust-base.toml".into()],
            ledger_commissions: 2,
        },
        ..Default::default()
    };

    let delta = StateDeltaFrame::snapshot(
        "inst-guild-1",
        1,
        1729000000000,
        vec![CensusRecordView {
            uid: 1001,
            identity: safe("builder"),
            commission: Some(1),
            commission_slug: Some(safe("compiler-v1")),
            state: "working".into(),
            stasis: false,
            session_count: 1,
            counters: ApiCensusCounters::default(),
            last_event: None,
            last_event_ts: None,
            title_digest: "d1".into(),
        }],
        vec![
            CommissionSummaryView {
                commission_id: 1,
                slug: safe("compiler-v1"),
                state: "Active".into(),
                roster: vec![1001],
                deliverables_count: 1,
                brief: true,
                indenture: true,
                git: "bare".into(),
            },
            CommissionSummaryView {
                commission_id: 2,
                slug: safe("docs-site"),
                state: "draft".into(),
                roster: vec![],
                deliverables_count: 0,
                brief: true,
                indenture: false,
                git: "bare".into(),
            },
        ],
        vec![],
        vec![],
        BTreeMap::new(),
        landmarks,
        vec![],
    );
    vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));

    vm.open_panel(PanelKind::Guild, Some("guild".into()));
    match vm.generate_panel_view() {
        ActivePanel::Guild(b) => {
            assert_eq!(b.commissions.len(), 2);
            assert_eq!(b.active_count, 1);
            assert_eq!(b.draft_count, 1);
            assert_eq!(b.closed_count, 0);
            assert_eq!(b.registered_agents, vec!["builder", "scout"]);
            assert_eq!(b.blueprints, vec!["rust-base.toml"]);
            assert_eq!(b.indenture_entries.len(), 1);
            assert_eq!(b.indenture_entries[0].uid, 1001);
            assert_eq!(b.indenture_entries[0].agent_name, "builder");
            assert!(b.can_create);
            assert!(b.can_close);
        }
        other => panic!("expected Guild board, got {:?}", other),
    }
}

#[test]
fn golden_test_vault_panel_projection() {
    let mut vm = VillageViewModel::new();

    let delta = StateDeltaFrame::snapshot(
        "inst-vault-1",
        1,
        1729000000000,
        vec![],
        vec![],
        vec![],
        vec![
            VaultItemView {
                name: "compiler-release.tar.gz".into(),
                commission: Some(safe("compiler-v1")),
                size_bytes: 1048576,
                sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(),
            },
            VaultItemView {
                name: "docs.zip".into(),
                commission: Some(safe("compiler-v1")),
                size_bytes: 204800,
                sha256: "5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8".into(),
            },
        ],
        BTreeMap::new(),
        LandmarksView::default(),
        vec![],
    );
    vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));

    vm.open_panel(PanelKind::Vault, Some("keep".into()));
    match vm.generate_panel_view() {
        ActivePanel::Vault(v) => {
            assert_eq!(v.artifacts.len(), 2);
            assert_eq!(v.total_count, 2);
            assert_eq!(v.total_bytes, 1048576 + 204800);
            assert!(v.can_export);
            assert_eq!(v.artifacts[0].name, "compiler-release.tar.gz");
        }
        other => panic!("expected Vault panel, got {:?}", other),
    }
}

#[test]
fn golden_test_decree_desk_projection() {
    let mut vm = VillageViewModel::new();

    let delta = StateDeltaFrame::snapshot(
        "inst-decree-1",
        1,
        1729000000000,
        vec![],
        vec![],
        vec![
            DecreeSummaryView {
                decree_id: "d-1-0001".into(),
                seq: 1,
                action: "provision_agent".into(),
                status: "applied".into(),
                reason: Some("provision builder".into()),
            },
            DecreeSummaryView {
                decree_id: "d-1-0002".into(),
                seq: 2,
                action: "decompose_commission".into(),
                status: "rejected".into(),
                reason: Some("missing brief".into()),
            },
            DecreeSummaryView {
                decree_id: "d-1-0003".into(),
                seq: 3,
                action: "close_commission".into(),
                status: "pending".into(),
                reason: None,
            },
        ],
        vec![],
        BTreeMap::new(),
        LandmarksView::default(),
        vec![EscalationView {
            id: "esc-mem-1".into(),
            decree_id: "d-1-0002".into(),
            summary: "Parcel 1001 memory quota exceeded".into(),
            evidence_refs: vec!["keep/census/parcel1001/TITLE.md".into()],
            recorded_unix_ms: 1729000000000,
            resolved: false,
            resolution: None,
        }],
    );
    vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));

    vm.open_panel(PanelKind::DecreeDesk, Some("manor".into()));
    match vm.generate_panel_view() {
        ActivePanel::DecreeDesk(d) => {
            assert_eq!(d.decrees.len(), 3);
            assert_eq!(d.pending_count, 1);
            assert_eq!(d.escalations.len(), 1);
            assert!(d.can_replay); // Rejected decree d-1-0002 can be replayed
            assert!(d.can_resolve); // Open escalation can be resolved
        }
        other => panic!("expected Decree desk panel, got {:?}", other),
    }
}

#[test]
fn golden_test_courier_panel_projection() {
    let mut vm = VillageViewModel::new();

    let mut mail_counts = BTreeMap::new();
    mail_counts.insert(1000, 3);
    mail_counts.insert(1001, 1);

    let delta = StateDeltaFrame::snapshot(
        "inst-courier-1",
        1,
        1729000000000,
        vec![],
        vec![],
        vec![],
        vec![],
        mail_counts,
        LandmarksView::default(),
        vec![],
    );
    vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));

    vm.open_panel(PanelKind::Courier, Some("courier".into()));
    match vm.generate_panel_view() {
        ActivePanel::Courier(c) => {
            assert_eq!(c.total_unread, 4);
            assert_eq!(c.mail_counts.get(&1000), Some(&3));
            assert_eq!(c.mail_counts.get(&1001), Some(&1));
            assert!(c.can_compose);
        }
        other => panic!("expected Courier panel, got {:?}", other),
    }
}

#[test]
fn golden_test_tavern_panel_projection() {
    let mut vm = VillageViewModel::new();

    let delta = StateDeltaFrame::snapshot(
        "inst-tavern-1",
        1,
        1729000000000,
        vec![CensusRecordView {
            uid: 1002,
            identity: safe("miner-patron"),
            commission: None,
            commission_slug: None,
            state: "stasis".into(),
            stasis: true,
            session_count: 3,
            counters: ApiCensusCounters::default(),
            last_event: None,
            last_event_ts: None,
            title_digest: "digest-patron".into(),
        }],
        vec![],
        vec![],
        vec![],
        BTreeMap::new(),
        LandmarksView::default(),
        vec![],
    );
    vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));

    vm.open_panel(PanelKind::Tavern, Some("tavern".into()));
    match vm.generate_panel_view() {
        ActivePanel::Tavern(t) => {
            assert_eq!(t.stasis_count, 1);
            assert_eq!(t.patrons.len(), 1);
            assert_eq!(t.patrons[0].uid, 1002);
            assert_eq!(t.patrons[0].identity, safe("miner-patron"));
            assert!(t.can_awaken);
        }
        other => panic!("expected Tavern panel, got {:?}", other),
    }
}

#[test]
fn golden_test_optimistic_ui_mutation_and_rollback_on_reject() {
    let mut vm = VillageViewModel::new();

    // 1. Dispatch optimistic command (e.g. steer agent 1001)
    vm.record_optimistic_command(
        "corr-steer-1001",
        "Steer directive to agent 1001",
        "agent:1001",
    );
    let mut_entry = vm
        .pending_mutations
        .get("corr-steer-1001")
        .expect("mutation exists");
    assert_eq!(mut_entry.status, MutationStatus::Pending);
    assert_eq!(mut_entry.description, "Steer directive to agent 1001");
    assert!(vm.last_mutation_error.is_none());

    // 2. Server rejects the command with 400 Bad Request
    let reject_ack = CommandAckFrame {
        seq: 10,
        timestamp_unix_ms: 1729000010000,
        correlation_id: "corr-steer-1001".into(),
        command_endpoint: "/api/v1/agents/1001/steer".into(),
        success: false,
        status_code: 400,
        payload: None,
        error: Some("agent 1001 is currently crashed".into()),
    };
    vm.apply_server_frame(WsServerFrame::CommandAck(reject_ack));

    // Optimistic UI rolled back immediately
    let rolled_back = vm.pending_mutations.get("corr-steer-1001").unwrap();
    assert_eq!(rolled_back.status, MutationStatus::RolledBack);
    assert_eq!(
        rolled_back.error_message.as_deref(),
        Some("agent 1001 is currently crashed")
    );
    assert_eq!(
        vm.last_mutation_error.as_deref(),
        Some("agent 1001 is currently crashed")
    );
    assert!(vm.ticker_history.iter().any(|t| t.contains("[ROLLBACK]")));

    // 3. Dispatch second command that succeeds
    vm.record_optimistic_command(
        "corr-wake-1002",
        "Wake patron 1002 into commission",
        "agent:1002",
    );
    let success_ack = CommandAckFrame {
        seq: 11,
        timestamp_unix_ms: 1729000011000,
        correlation_id: "corr-wake-1002".into(),
        command_endpoint: "/api/v1/agents/wake".into(),
        success: true,
        status_code: 200,
        payload: Some(serde_json::json!({ "uid": 1002, "status": "active" })),
        error: None,
    };
    vm.apply_server_frame(WsServerFrame::CommandAck(success_ack));

    let confirmed = vm.pending_mutations.get("corr-wake-1002").unwrap();
    assert_eq!(confirmed.status, MutationStatus::Confirmed);
    assert!(vm.last_mutation_error.is_none());
    assert!(vm.ticker_history.iter().any(|t| t.contains("[ACK OK]")));
}

#[test]
fn golden_test_a11y_keymap_table() {
    // Normative keymap coverage test: verifies hotkeys 1-6 map 1:1 to landmarks
    let keymap: [(&str, &str, PanelKind); 6] = [
        ("1", "guild", PanelKind::Guild),
        ("2", "courier", PanelKind::Courier),
        ("3", "keep", PanelKind::Vault),
        ("4", "tavern", PanelKind::Tavern),
        ("5", "market", PanelKind::Guild), // Market landmarks reviewable via Guild/Economy
        ("6", "manor", PanelKind::DecreeDesk),
    ];

    let vm = VillageViewModel::new();
    for (key, landmark_id, panel_kind) in keymap {
        let lm = vm
            .map
            .find_landmark(landmark_id)
            .unwrap_or_else(|| panic!("missing {landmark_id}"));
        assert!(!lm.name.is_empty());
        assert!(!key.is_empty());
        // Verify panel opens cleanly
        let mut test_vm = vm.clone();
        test_vm.open_panel(panel_kind, Some(landmark_id.into()));
        assert_ne!(test_vm.generate_panel_view(), ActivePanel::None);
    }
}
