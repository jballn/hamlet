//! Headless view-model golden tests and scripted frame sequence verification (SPEC §12.3; ADR-0044).

use std::collections::BTreeMap;

use hamlet_core::api::{
    ApiCensusCounters, BubbleFrame, BubbleKind, BubbleTarget, CensusRecordView,
    CommissionSummaryView, DecreeSummaryView, LandmarksView, MetricsBatchFrame, ParcelMetricTick,
    StateDeltaFrame, VaultItemView, WsServerFrame,
};
use hamlet_core::sanitize::sanitize_name;
use hamlet_web::view_model::VillageViewModel;

fn safe(s: &str) -> hamlet_core::sanitize::SafeName {
    sanitize_name(s).expect("valid safe name")
}

#[test]
fn golden_test_initial_snapshot_full_village_topology() {
    let mut vm = VillageViewModel::new();

    let mut mail_counts = BTreeMap::new();
    mail_counts.insert(1000, 3);
    mail_counts.insert(1001, 1);

    let delta = StateDeltaFrame::snapshot(
        "inst-golden-1",
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
                session_count: 3,
                counters: ApiCensusCounters {
                    input_tokens: 12000,
                    output_tokens: 4500,
                    cost_micro_usd: 85000,
                    traces: 14,
                },
                last_event: Some("pi_tool_call".into()),
                last_event_ts: Some("1729000000000".into()),
                title_digest: "digest-1".into(),
            },
            CensusRecordView {
                uid: 1002,
                identity: safe("reviewer"),
                commission: Some(1),
                commission_slug: Some(safe("compiler-v1")),
                state: "idle".into(),
                stasis: false,
                session_count: 1,
                counters: ApiCensusCounters {
                    input_tokens: 4000,
                    output_tokens: 800,
                    cost_micro_usd: 12000,
                    traces: 4,
                },
                last_event: None,
                last_event_ts: None,
                title_digest: "digest-2".into(),
            },
            CensusRecordView {
                uid: 1003,
                identity: safe("miner"),
                commission: None,
                commission_slug: None,
                state: "stasis".into(),
                stasis: true,
                session_count: 5,
                counters: ApiCensusCounters::default(),
                last_event: None,
                last_event_ts: None,
                title_digest: "digest-3".into(),
            },
        ],
        vec![CommissionSummaryView {
            commission_id: 1,
            slug: safe("compiler-v1"),
            state: "Active".into(),
            roster: vec![1001, 1002],
            deliverables_count: 1,
            brief: true,
            indenture: true,
            git: "commit-abc".into(),
        }],
        vec![DecreeSummaryView {
            decree_id: "d-1-0001".into(),
            seq: 1,
            action: "provision_agent".into(),
            status: "pending".into(),
            reason: Some("provision agent builder".into()),
        }],
        vec![VaultItemView {
            name: "lexer.tar.gz".into(),
            commission: Some(safe("compiler-v1")),
            sha256: "deadbeef01234567".into(),
            size_bytes: 4096,
        }],
        mail_counts,
        LandmarksView::default(),
        vec![],
    );

    let changed = vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));
    assert!(changed);

    let scene = vm.generate_scene();

    // Verify 6 SPEC §2 Landmarks
    assert_eq!(scene.landmarks.len(), 6);
    let expected_paths = [
        ("guild", "guild/"),
        ("courier", "courier/"),
        ("keep", "keep/"),
        ("tavern", "tavern/"),
        ("market", "market/"),
        ("manor", "keep/census/manor/"),
    ];
    for (id, tree_path) in expected_paths {
        let lm = scene.landmarks.iter().find(|l| l.id == id).expect(id);
        assert_eq!(lm.tree_path, tree_path);
    }

    // Verify census workshop structures (SPEC §2 keep/census/parcel<uid>/)
    assert_eq!(scene.workshops.len(), 3);
    let ws1001 = scene.workshops.iter().find(|w| w.uid == 1001).unwrap();
    assert_eq!(ws1001.parcel_slug, "parcel1001");
    assert_eq!(ws1001.tree_path, "keep/census/parcel1001/");
    assert_eq!(ws1001.agent_name, "builder");

    // Verify sprite count derives from state-delta ONLY (1 Mayor + 3 agents = 4 sprites)
    assert_eq!(scene.sprites.len(), 4);
    let mayor = scene.sprites.iter().find(|s| s.is_mayor).unwrap();
    assert_eq!(mayor.uid, 1000);
    assert_eq!(mayor.x, 27.0);
    assert_eq!(mayor.y, 19.0);

    let builder_sp = scene.sprites.iter().find(|s| s.uid == 1001).unwrap();
    assert_eq!(builder_sp.name, "builder");
    assert_eq!(builder_sp.anim_state, "working");
    assert!(builder_sp.tool_swing);

    let miner_sp = scene.sprites.iter().find(|s| s.uid == 1003).unwrap();
    assert_eq!(miner_sp.anim_state, "sleeping");
    assert!(miner_sp.zzz);

    // Verify HUD
    assert_eq!(scene.hud.seq, 1);
    assert_eq!(scene.hud.active_agents, 2); // 1001 and 1002 (1003 is stasis)
    assert_eq!(scene.hud.active_commissions, 1);
    assert_eq!(scene.hud.pending_decrees, 1);
    assert_eq!(scene.hud.unread_mail, 4);

    // Golden JSON serialization check
    let json = serde_json::to_string_pretty(&scene).unwrap();
    assert!(json.contains("\"daemon_instance_id\": \"inst-golden-1\""));
    assert!(json.contains("\"active_agents\": 2"));
    assert!(json.contains("\"parcel1001\""));
}

#[test]
fn golden_test_state_driven_animations_and_steer_attention_mark() {
    let mut vm = VillageViewModel::new();

    let delta = StateDeltaFrame::snapshot(
        "inst-anim",
        1,
        1729000000000,
        vec![
            CensusRecordView {
                uid: 1001,
                identity: safe("worker-working"),
                commission: Some(1),
                commission_slug: Some(safe("demo")),
                state: "working".into(),
                stasis: false,
                session_count: 1,
                counters: ApiCensusCounters::default(),
                last_event: None,
                last_event_ts: None,
                title_digest: "d1".into(),
            },
            CensusRecordView {
                uid: 1002,
                identity: safe("worker-idle"),
                commission: Some(1),
                commission_slug: Some(safe("demo")),
                state: "idle".into(),
                stasis: false,
                session_count: 1,
                counters: ApiCensusCounters::default(),
                last_event: None,
                last_event_ts: None,
                title_digest: "d2".into(),
            },
            CensusRecordView {
                uid: 1003,
                identity: safe("worker-crashed"),
                commission: None,
                commission_slug: None,
                state: "crashed".into(),
                stasis: false,
                session_count: 1,
                counters: ApiCensusCounters::default(),
                last_event: None,
                last_event_ts: None,
                title_digest: "d3".into(),
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

    // Initially: working, idle, crashed
    let s0 = vm.generate_scene();
    let sp1 = s0.sprites.iter().find(|s| s.uid == 1001).unwrap();
    assert_eq!(sp1.anim_state, "working");
    assert!(sp1.tool_swing);

    let sp2 = s0.sprites.iter().find(|s| s.uid == 1002).unwrap();
    assert_eq!(sp2.anim_state, "idle");
    assert!(sp2.loiter);

    let sp3 = s0.sprites.iter().find(|s| s.uid == 1003).unwrap();
    assert_eq!(sp3.anim_state, "crashed");
    assert!(sp3.fallen);

    // Steer operator injection on worker 1002
    let steer_frame = BubbleFrame::steer(2, 1729000001000, 1002, "Interrupt and recheck bounds");
    vm.apply_server_frame(WsServerFrame::Bubble(steer_frame));

    let s1 = vm.generate_scene();
    let sp2_steered = s1.sprites.iter().find(|s| s.uid == 1002).unwrap();
    assert_eq!(sp2_steered.anim_state, "steered");
    assert!(sp2_steered.attention_mark);

    // Advance clock 2000ms: steer attention mark still active (duration 4000ms)
    vm.tick(2000);
    let s2 = vm.generate_scene();
    assert!(
        s2.sprites
            .iter()
            .find(|s| s.uid == 1002)
            .unwrap()
            .attention_mark
    );

    // Advance clock 2500ms more: steer attention mark expires, reverts to idle
    vm.tick(2500);
    let s3 = vm.generate_scene();
    let sp2_restored = s3.sprites.iter().find(|s| s.uid == 1002).unwrap();
    assert_eq!(sp2_restored.anim_state, "idle");
    assert!(!sp2_restored.attention_mark);
}

#[test]
fn golden_test_chat_bubbles_queueing_timeout_and_honesty_rule() {
    let mut vm = VillageViewModel::new();

    // Enqueue 3 bubbles targeting agent 1001
    let b1 = BubbleFrame::steer(1, 100, 1001, "First task directive");
    let b2 = BubbleFrame::mail_snippet(
        2,
        200,
        1000,
        1001,
        "Confidential Project Brief",
        "This is a very long text snippet that deliberately exceeds the maximum allowed mail snippet length of sixty four characters and should be truncated safely.",
    );
    let b3 = BubbleFrame {
        seq: 3,
        timestamp_unix_ms: 300,
        target: BubbleTarget::Agent(1001),
        kind: BubbleKind::AgentSpeech {
            uid: 1001,
            text: "Understood, proceeding.".into(),
        },
        text: "Understood, proceeding.".into(),
        duration_ms: 2000,
    };

    vm.apply_server_frame(WsServerFrame::Bubble(b1));
    vm.apply_server_frame(WsServerFrame::Bubble(b2));
    vm.apply_server_frame(WsServerFrame::Bubble(b3));

    // Queue check: Only b1 should be in active_bubbles
    let s1 = vm.generate_scene();
    assert_eq!(s1.bubbles.len(), 1);
    assert_eq!(s1.bubbles[0].kind, "steer");
    assert!(s1.bubbles[0].text.contains("First task directive"));

    // Tick to expire b1 (3000ms)
    vm.tick(3000);
    let s2 = vm.generate_scene();
    assert_eq!(s2.bubbles.len(), 1);
    assert_eq!(s2.bubbles[0].kind, "mail");
    assert!(s2.bubbles[0].is_mail);
    // Verify client-side honesty rule enforcement: subject + truncated snippet only
    assert!(s2.bubbles[0]
        .text
        .starts_with("[Mail] Confidential Project Brief:"));
    assert!(s2.bubbles[0].text.contains("..."));
    assert!(s2.bubbles[0].text.len() < 120);

    // Tick to expire b2 (4000ms)
    vm.tick(4000);
    let s3 = vm.generate_scene();
    assert_eq!(s3.bubbles.len(), 1);
    assert_eq!(s3.bubbles[0].kind, "speech");
    assert_eq!(s3.bubbles[0].text, "Understood, proceeding.");

    // Tick to expire b3 (2000ms)
    vm.tick(2000);
    let s4 = vm.generate_scene();
    assert_eq!(s4.bubbles.len(), 0);
}

#[test]
fn golden_test_metrics_sparklines_and_village_ticker() {
    let mut vm = VillageViewModel::new();

    // Send 3 batches of metrics
    for seq in 1..=3 {
        let batch = MetricsBatchFrame {
            seq,
            timestamp_unix_ms: 1729000000000 + seq * 1000,
            ticks: vec![
                ParcelMetricTick {
                    uid: 1001,
                    session_id: Some("sess-1".into()),
                    tokens_in: 500 * seq,
                    tokens_out: 100 * seq,
                    cost_micro_usd: Some(2500 * seq),
                    latency_ms: 150,
                    memory_bytes: Some(1024 * 1024),
                    cpu_percent_x10: Some(250),
                },
                ParcelMetricTick {
                    uid: 1002,
                    session_id: Some("sess-2".into()),
                    tokens_in: 300 * seq,
                    tokens_out: 50 * seq,
                    cost_micro_usd: Some(1200 * seq),
                    latency_ms: 100,
                    memory_bytes: Some(512 * 1024),
                    cpu_percent_x10: Some(120),
                },
            ],
        };
        vm.apply_server_frame(WsServerFrame::Metrics(batch));
    }

    // Verify sparklines in scene graph
    let mut delta = StateDeltaFrame::snapshot(
        "inst-m",
        4,
        1729000004000,
        vec![CensusRecordView {
            uid: 1001,
            identity: safe("builder"),
            commission: None,
            commission_slug: None,
            state: "live".into(),
            stasis: false,
            session_count: 1,
            counters: ApiCensusCounters::default(),
            last_event: None,
            last_event_ts: None,
            title_digest: "d".into(),
        }],
        vec![],
        vec![],
        vec![],
        BTreeMap::new(),
        LandmarksView::default(),
        vec![],
    );
    delta.is_snapshot = false; // Don't flush metrics
    vm.apply_state_delta(delta);

    let scene = vm.generate_scene();
    let sp1001 = scene.sprites.iter().find(|s| s.uid == 1001).unwrap();
    // 3 ticks accumulated
    assert_eq!(sp1001.sparkline.len(), 3);
    assert_eq!(sp1001.sparkline, vec![600, 1200, 1800]);

    // Verify village ticker
    assert!(!scene.hud.ticker.is_empty());
    assert!(scene.hud.ticker[0].contains("[METRICS]"));
    assert!(scene.hud.ticker[0].contains("2 parcels reporting"));
}

#[test]
fn golden_test_frame_budget_degradation_guard_prioritizes_escalation() {
    let mut vm = VillageViewModel::new();

    // Enqueue 4 bubbles across different targets
    vm.apply_server_frame(WsServerFrame::Bubble(BubbleFrame {
        seq: 1,
        timestamp_unix_ms: 100,
        target: BubbleTarget::Agent(1001),
        kind: BubbleKind::AgentSpeech {
            uid: 1001,
            text: "Just chatting".into(),
        },
        text: "Just chatting".into(),
        duration_ms: 3000,
    }));
    vm.apply_server_frame(WsServerFrame::Bubble(BubbleFrame::mail_snippet(
        2,
        100,
        1000,
        1002,
        "Mail subject",
        "Mail snippet",
    )));
    vm.apply_server_frame(WsServerFrame::Bubble(BubbleFrame::steer(
        3,
        100,
        1003,
        "Steer directive",
    )));
    vm.apply_server_frame(WsServerFrame::Bubble(BubbleFrame::escalation(
        4,
        100,
        "esc-oom",
        "Out of memory in parcel 1001",
    )));

    // In normal mode: 4 bubbles present
    assert_eq!(vm.generate_scene().bubbles.len(), 4);

    // Activate degradation mode (client blitting dropped below 50fps)
    vm.set_degraded_mode(true);
    let scene = vm.generate_scene();
    assert_eq!(scene.bubbles.len(), 2);
    assert!(scene.hud.frame_budget_degraded);

    // High-priority bubbles (escalation and steer) take priority over speech and mail
    let kinds: Vec<String> = scene.bubbles.iter().map(|b| b.kind.clone()).collect();
    assert!(kinds.contains(&"escalation".to_string()));
    assert!(kinds.contains(&"steer".to_string()));

    // Bubbles decay at 2x rate under degraded mode
    vm.tick(1500); // 1500ms * 2 = 3000ms effective decay: speech and steer (3000ms) expire
    let post_tick = vm.generate_scene();
    assert_eq!(post_tick.bubbles.len(), 2); // mail (4000ms) and escalation (5000ms) remain

    vm.tick(600); // 600ms * 2 = 1200ms effective decay: mail expires, escalation remains (800ms)
    let post_tick2 = vm.generate_scene();
    assert_eq!(post_tick2.bubbles.len(), 1);
    assert_eq!(post_tick2.bubbles[0].kind, "escalation");

    vm.tick(500); // 500ms * 2 = 1000ms effective decay: escalation expires
    let post_tick3 = vm.generate_scene();
    assert_eq!(post_tick3.bubbles.len(), 0);
}

#[test]
fn golden_test_resync_and_restart_clears_all_caches() {
    let mut vm = VillageViewModel::new();

    let delta = StateDeltaFrame::snapshot(
        "inst-alpha",
        1,
        1729000000000,
        vec![CensusRecordView {
            uid: 1001,
            identity: safe("builder"),
            commission: None,
            commission_slug: None,
            state: "live".into(),
            stasis: false,
            session_count: 1,
            counters: ApiCensusCounters::default(),
            last_event: None,
            last_event_ts: None,
            title_digest: "d".into(),
        }],
        vec![],
        vec![],
        vec![],
        BTreeMap::new(),
        LandmarksView::default(),
        vec![],
    );
    vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta)));
    assert_eq!(vm.generate_scene().sprites.len(), 2); // mayor + builder

    // New daemon instance id in snapshot triggers cache invalidation
    let delta2 = StateDeltaFrame::snapshot(
        "inst-beta",
        1,
        1729000010000,
        vec![],
        vec![],
        vec![],
        vec![],
        BTreeMap::new(),
        LandmarksView::default(),
        vec![],
    );
    vm.apply_server_frame(WsServerFrame::StateDelta(Box::new(delta2)));
    let scene2 = vm.generate_scene();
    assert_eq!(scene2.hud.daemon_instance_id, "inst-beta");
    // Builder was flushed; only Mayor remains
    assert_eq!(scene2.sprites.len(), 1);
    assert!(scene2.sprites[0].is_mayor);
}
