//! Mock fixtures and scripted event streams for demo and headless test mode.

use hamlet_core::api::*;
use hamlet_core::sanitize::sanitize_name;

use crate::model::VillageData;

fn safe(s: &str) -> hamlet_core::sanitize::SafeName {
    sanitize_name(s).expect("valid safe name")
}

/// Generates a realistic mock village dataset adhering strictly to SPEC §2.
#[must_use]
pub fn demo_village_data() -> VillageData {
    let status = DaemonStatusResponse {
        version: "0.0.0".into(),
        pid: 4200,
        uptime_secs: 1845,
        village_root: "/home/dock/Development/hamlet/.hamlet".into(),
        active_agents: 2,
        commissions_count: 2,
        queued_decrees: 1,
        queued_mail: 2,
    };

    let landmarks = LandmarksView {
        guild: GuildLandmark {
            register: vec!["builder".into(), "scout".into(), "tester".into()],
            blueprints: vec!["base-v1".into(), "python-data".into(), "rust-dev".into()],
            ledger_commissions: 2,
        },
        tavern: TavernLandmark {
            gossip: vec!["initial-brief.md".into(), "user-feedback.md".into()],
            patrons: vec!["code-oracle".into(), "veteran-miner".into()],
        },
        courier: CourierLandmark {
            spools: vec![
                MailSpoolView {
                    uid: 1000,
                    unread: 1,
                    read: 3,
                },
                MailSpoolView {
                    uid: 1001,
                    unread: 0,
                    read: 5,
                },
                MailSpoolView {
                    uid: 1002,
                    unread: 1,
                    read: 2,
                },
            ],
            freight_sandboxes: 2,
            dispatch_outbox: 1,
            dispatch_inbox: 4,
            dispatch_dead: 0,
        },
        market: MarketLandmark {
            skills: vec![
                "cargo-test".into(),
                "git-rebase".into(),
                "ripgrep-search".into(),
            ],
            pkgcache_entries: 42,
            pkgcache_bytes: 84_521_000,
        },
        keep: KeepLandmark {
            vault_items: 2,
            archives: vec![
                "commission1-convergence.json".into(),
                "commission2-summary.json".into(),
            ],
            census_parcels: vec![1001, 1002, 1003],
        },
        manor: ManorLandmark {
            mayor_md: true,
            mayor_session: Some("idle".into()),
            mayor_incarnation: Some(4),
            decrees_rejected: 0,
            escalations_open: 1,
            escalations_resolved: 1,
        },
    };

    let census = vec![
        CensusRecordView {
            uid: 1001,
            identity: safe("scout"),
            commission: Some(1),
            commission_slug: Some(safe("compiler-v1")),
            state: "live".into(),
            stasis: false,
            session_count: 3,
            counters: ApiCensusCounters {
                input_tokens: 45_000,
                output_tokens: 12_000,
                cost_micro_usd: 32_000,
                traces: 14,
            },
            last_event: Some("session_settled".into()),
            last_event_ts: Some("2026-10-19T14:00:00Z".into()),
            title_digest: "a1b2c3d4".into(),
        },
        CensusRecordView {
            uid: 1002,
            identity: safe("builder"),
            commission: Some(1),
            commission_slug: Some(safe("compiler-v1")),
            state: "busy".into(),
            stasis: false,
            session_count: 5,
            counters: ApiCensusCounters {
                input_tokens: 120_000,
                output_tokens: 38_000,
                cost_micro_usd: 95_000,
                traces: 28,
            },
            last_event: Some("git_commit".into()),
            last_event_ts: Some("2026-10-19T14:02:15Z".into()),
            title_digest: "e5f60718".into(),
        },
        CensusRecordView {
            uid: 1003,
            identity: safe("tester"),
            commission: None,
            commission_slug: None,
            state: "stasis".into(),
            stasis: true,
            session_count: 1,
            counters: ApiCensusCounters {
                input_tokens: 8_000,
                output_tokens: 2_000,
                cost_micro_usd: 5_000,
                traces: 4,
            },
            last_event: Some("hibernated".into()),
            last_event_ts: Some("2026-10-19T13:45:00Z".into()),
            title_digest: "293a4b5c".into(),
        },
    ];

    let comm1_slug = safe("compiler-v1");
    let comm2_slug = safe("web-dashboard");

    let comm1_summary = CommissionSummaryView {
        commission_id: 1,
        slug: comm1_slug.clone(),
        state: "Active".into(),
        roster: vec![1001, 1002],
        deliverables_count: 2,
        brief: true,
        indenture: true,
        git: "bare".into(),
    };

    let comm2_summary = CommissionSummaryView {
        commission_id: 2,
        slug: comm2_slug.clone(),
        state: "Draft".into(),
        roster: vec![],
        deliverables_count: 0,
        brief: true,
        indenture: false,
        git: "-".into(),
    };

    let mut commission_details = std::collections::BTreeMap::new();
    commission_details.insert(
        comm1_slug.to_string(),
        CommissionDetailView {
            summary: comm1_summary.clone(),
            brief: Some("Build the normative compiler pipeline for the village core.".into()),
            git_head: Some("refs/heads/main".into()),
            main_oid: Some("8a3f912b0c14".into()),
            indenture_tasks: vec![
                ApiIndentureTask {
                    uid: 1001,
                    task: "lexer and ast definitions".into(),
                    done: true,
                    identity: Some(safe("scout")),
                    status: "Done".into(),
                },
                ApiIndentureTask {
                    uid: 1002,
                    task: "codegen pass over llvm".into(),
                    done: false,
                    identity: Some(safe("builder")),
                    status: "Busy".into(),
                },
            ],
            deliverables: vec!["hamlet-core-v0.1.0.tar.gz".into()],
            convergence: None,
        },
    );

    let decrees = vec![
        DecreeSummaryView {
            decree_id: "d-1-0001".into(),
            seq: 1,
            action: "provision_agent".into(),
            status: "done".into(),
            reason: None,
        },
        DecreeSummaryView {
            decree_id: "d-2-0002".into(),
            seq: 2,
            action: "decompose_commission".into(),
            status: "done".into(),
            reason: None,
        },
        DecreeSummaryView {
            decree_id: "d-3-0003".into(),
            seq: 3,
            action: "escalate_to_human".into(),
            status: "pending".into(),
            reason: Some("Resource bound exceeded on parcel1002".into()),
        },
    ];

    let mut decree_details = std::collections::BTreeMap::new();
    decree_details.insert(
        "d-3-0003".into(),
        DecreeDetailView {
            summary: DecreeSummaryView {
                decree_id: "d-3-0003".into(),
                seq: 3,
                action: "escalate_to_human".into(),
                status: "pending".into(),
                reason: Some("Resource bound exceeded on parcel1002".into()),
            },
            raw_json: serde_json::to_string_pretty(&serde_json::json!({
                "action": "escalate_to_human",
                "decree_id": "d-3-0003",
                "evidence_refs": ["keep/census/parcel1002/TITLE.md"],
                "summary": "parcel1002 memory limit 512MB reached",
                "seq": 3,
            }))
            .unwrap_or_default(),
            ack_verdict: None,
            ack_reason: None,
        },
    );

    let escalations = vec![
        EscalationView {
            id: "d-3-0003.json".into(),
            decree_id: "d-3-0003".into(),
            summary: "parcel1002 cpu limit exceeded during codegen pass".into(),
            evidence_refs: vec![
                "keep/census/parcel1002/TITLE.md".into(),
                "keep/archives/telemetry.sled".into(),
            ],
            recorded_unix_ms: 1729000000000,
            resolved: false,
            resolution: None,
        },
        EscalationView {
            id: "d-1-0001.json".into(),
            decree_id: "d-1-0001".into(),
            summary: "untrusted tool installation requested by scout".into(),
            evidence_refs: vec!["guild/register/scout".into()],
            recorded_unix_ms: 1728990000000,
            resolved: true,
            resolution: Some("Approved by operator: standard dev tooling allowed.".into()),
        },
    ];

    let mail = vec![
        MailMessageSummary {
            file_name: "0001.mail".into(),
            from: "mayor".into(),
            to: 1000,
            subject: "Welcome to hamlet village orchestration".into(),
            read: true,
        },
        MailMessageSummary {
            file_name: "0002.mail".into(),
            from: "scout".into(),
            to: 1000,
            subject: "Commission compiler-v1 AST draft ready".into(),
            read: false,
        },
    ];

    let vault = vec![
        VaultItemView {
            name: "hamlet-core-v0.1.0.tar.gz".into(),
            commission: Some(comm1_slug.clone()),
            size_bytes: 1_048_576,
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        },
        VaultItemView {
            name: "docs-spec-v1.pdf".into(),
            commission: Some(comm1_slug),
            size_bytes: 524_288,
            sha256: "a1b2c3d4e5f60718293a4b5c6d7e8f90123456789abcdef0123456789abcdef0".into(),
        },
    ];

    let metrics = MetricsSummaryView {
        total_input_tokens: 173_000,
        total_output_tokens: 52_000,
        total_cost_micro_usd: 132_000,
        active_sessions: 2,
        agents: vec![
            AgentMetricsItem {
                uid: 1001,
                identity: safe("scout"),
                commission: Some(1),
                input_tokens: 45_000,
                output_tokens: 12_000,
                cost_micro_usd: 32_000,
                crashes: 0,
                uptime_secs: 1200,
            },
            AgentMetricsItem {
                uid: 1002,
                identity: safe("builder"),
                commission: Some(1),
                input_tokens: 120_000,
                output_tokens: 38_000,
                cost_micro_usd: 95_000,
                crashes: 1,
                uptime_secs: 645,
            },
        ],
    };

    VillageData {
        status: Some(status),
        landmarks,
        census,
        commissions: vec![comm1_summary, comm2_summary],
        commission_details,
        decrees,
        decree_details,
        escalations,
        mail,
        vault,
        metrics: Some(metrics),
    }
}

/// Generates a realistic sequence of simulated WebSocket event frames.
#[must_use]
pub fn demo_event_stream() -> Vec<WsEvent> {
    vec![
        WsEvent::new(
            WsTopic::Commission,
            "commission_created",
            101,
            1729000100000,
            serde_json::json!({ "commission_id": 1, "slug": "compiler-v1" }),
        ),
        WsEvent::new(
            WsTopic::Census,
            "agent_woken",
            102,
            1729000105000,
            serde_json::json!({ "uid": 1001, "identity": "scout", "commission": "compiler-v1" }),
        ),
        WsEvent::new(
            WsTopic::Mail,
            "mail_sent",
            103,
            1729000110000,
            serde_json::json!({ "from": "operator", "to": 1001, "subject": "Commence scouting" }),
        ),
        WsEvent::new(
            WsTopic::Decree,
            "decree_executed",
            104,
            1729000120000,
            serde_json::json!({ "seq": 1, "action": "provision_agent" }),
        ),
        WsEvent::new(
            WsTopic::Census,
            "agent_steered",
            105,
            1729000130000,
            serde_json::json!({ "uid": 1002, "correlation": "c-1002-1" }),
        ),
        WsEvent::new(
            WsTopic::Census,
            "agent_hibernated",
            106,
            1729000140000,
            serde_json::json!({ "uid": 1003, "patron": "tester" }),
        ),
        WsEvent::new(
            WsTopic::Decree,
            "escalation_resolved",
            107,
            1729000150000,
            serde_json::json!({ "id": "d-1-0001.json", "mayor_notified": true }),
        ),
    ]
}
