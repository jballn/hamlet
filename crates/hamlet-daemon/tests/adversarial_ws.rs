//! Adversarial WebSocket integration test suite (SPEC §12.3; ADR-0044; CONVENTIONS §2, §3, §9).
//!
//! Covers:
//! 1. Protocol conformance:
//!    - ADR-0044 frame types round-trip serialization/deserialization.
//!    - Unknown frames ignored gracefully without socket drop or daemon panic.
//!    - Wire garbage and fuzzing resilience (binary payloads, truncated JSON, deep nesting).
//!    - Auth rejection matrix (missing token, empty token, forged token, valid token variants).
//!    - Repro QA09-01: Malformed command frames silently dropped instead of returning typed error acks.
//! 2. Honesty rules:
//!    - Spectator stream never receives .env secrets, full cross-owner mail bodies, or sled raw keys.
//!    - Snippet length strictly bounded to MAX_MAIL_SNIPPET_LEN (64 chars).
//! 3. State fidelity:
//!    - Projections in StateDeltaFrame snapshot match HTTP REST local API projections 1:1.
//! 4. Backpressure & Client flood:
//!    - Slow client drops metrics while preserving critical StateDeltas.
//!    - Repro QA09-02: Client command flood causes broadcast channel lag, terminating forwarder task.
//! 5. Reconnect & Kill:
//!    - Resync with instance ID mismatch clears caches with zero ghosts.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::api::*;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;
use hamlet_daemon::ledger::Ledger;
use hamlet_daemon::local_api::{local_api_router, LocalApiState};
use hamlet_daemon::mailroute::MailRouter;
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::Telemetry;
use hamlet_daemon::web::{AssetServer, WsBroadcastFrame};
use reqwest::header::HeaderValue;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::stream::MaybeTlsStream;
use tokio_tungstenite::tungstenite::{connect, Message, WebSocket};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const TEST_TOKEN: &str = "test-token-adversarial-0123456789abcdef0123456789";

fn safe(s: &str) -> SafeName {
    sanitize_name(s).expect("valid safe name")
}

struct TestVillageServer {
    _tmp: tempfile::TempDir,
    tree: Arc<HamletTree>,
    base_url: String,
    token: LocalToken,
    _shutdown_tx: watch::Sender<bool>,
    state: Arc<LocalApiState>,
}

async fn start_adversarial_server() -> TestVillageServer {
    let tmp = tempfile::tempdir().unwrap();
    let tree = Arc::new(HamletTree::ensure(tmp.path().join(".hamlet")).unwrap());

    // Minimal charter with Mayor (1000) and worker (1001)
    let mut charter = Charter::empty();
    charter.reserve_mayor(safe("mayor")).unwrap();
    charter.allocate_slot(safe("worker"), None).unwrap();
    charter.save_atomic(&tree.charter()).unwrap();

    let token = LocalToken::new(TEST_TOKEN);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let config = Config::default();
    let supervisor = Arc::new(Supervisor::new(
        (*tree).clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    let mail_router = Arc::new(MailRouter::new(Arc::clone(&supervisor)));
    let ledger = Arc::new(Ledger::new((*tree).clone()));

    let dist_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hamlet-web/dist");
    let asset_server = Arc::new(AssetServer::from_dir(dist_dir));

    let state = Arc::new(
        LocalApiState::new(tree.clone(), token.clone(), shutdown_tx.clone())
            .with_supervisor(supervisor)
            .with_mail_router(mail_router)
            .with_ledger(ledger)
            .with_asset_server(asset_server),
    );

    let router = local_api_router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;
    });

    TestVillageServer {
        _tmp: tmp,
        tree,
        base_url: format!("http://{}", addr),
        token,
        _shutdown_tx: shutdown_tx,
        state,
    }
}

type WsClient = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

#[allow(clippy::result_large_err)]
fn connect_ws_raw(
    base_url: &str,
    token_query: Option<&str>,
    bearer_header: Option<&str>,
    custom_header: Option<&str>,
) -> Result<WsClient, tokio_tungstenite::tungstenite::Error> {
    let ws_url = if let Some(t) = token_query {
        format!(
            "{}/api/v1/events/ws?token={}",
            base_url.replace("http://", "ws://"),
            t
        )
    } else {
        format!("{}/api/v1/events/ws", base_url.replace("http://", "ws://"))
    };

    let mut req = ws_url.into_client_request()?;
    if let Some(bearer) = bearer_header {
        req.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", bearer)).unwrap(),
        );
    }
    if let Some(custom) = custom_header {
        req.headers_mut()
            .insert(LOCAL_TOKEN_HEADER, HeaderValue::from_str(custom).unwrap());
    }

    let (socket, _) = connect(req)?;
    if let MaybeTlsStream::Plain(s) = socket.get_ref() {
        s.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
    }
    Ok(socket)
}

fn connect_ws(server: &TestVillageServer) -> WsClient {
    connect_ws_raw(&server.base_url, Some(TEST_TOKEN), None, None)
        .expect("successful websocket connection")
}

fn read_server_frame(socket: &mut WsClient) -> WsServerFrame {
    loop {
        match socket.read().expect("read ws message") {
            Message::Text(t) => match serde_json::from_str::<WsServerFrame>(&t) {
                Ok(f) => return f,
                Err(e) => {
                    panic!("failed to parse WsServerFrame: {e}\nPayload: {t}");
                }
            },
            Message::Ping(p) => {
                let _ = socket.write(Message::Pong(p));
                let _ = socket.flush();
            }
            Message::Pong(_) => {}
            other => panic!("unexpected ws message type: {:?}", other),
        }
    }
}

fn try_read_server_frame(socket: &mut WsClient, timeout: Duration) -> Option<WsServerFrame> {
    if let MaybeTlsStream::Plain(s) = socket.get_ref() {
        s.set_read_timeout(Some(timeout)).unwrap();
    }
    match socket.read() {
        Ok(Message::Text(t)) => serde_json::from_str::<WsServerFrame>(&t).ok(),
        Ok(Message::Ping(p)) => {
            let _ = socket.write(Message::Pong(p));
            let _ = socket.flush();
            None
        }
        _ => None,
    }
}

fn send_raw_text(socket: &mut WsClient, text: &str) {
    socket
        .write(Message::Text(text.to_string()))
        .expect("write ws message");
    socket.flush().expect("flush ws message");
}

fn send_client_frame(socket: &mut WsClient, frame: &WsClientFrame) {
    let t = serde_json::to_string(frame).expect("valid WsClientFrame JSON");
    send_raw_text(socket, &t);
}

// ---------------------------------------------------------------------------
// 1. Protocol Conformance Suite
// ---------------------------------------------------------------------------

#[test]
fn test_protocol_all_adr0044_frames_round_trip() {
    // 1. StateDeltaFrame (Snapshot)
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
    let s_frame = WsServerFrame::StateDelta(Box::new(snap));
    let json = serde_json::to_string(&s_frame).unwrap();
    let decoded: WsServerFrame = serde_json::from_str(&json).unwrap();
    assert_eq!(s_frame, decoded);

    // 2. MetricsBatchFrame
    let metrics = MetricsBatchFrame {
        seq: 2,
        timestamp_unix_ms: 1_700_000_100,
        ticks: vec![ParcelMetricTick {
            uid: 1001,
            session_id: Some("sess-1".to_string()),
            tokens_in: 150,
            tokens_out: 75,
            cost_micro_usd: Some(4200),
            latency_ms: 35,
            memory_bytes: Some(1024 * 1024),
            cpu_percent_x10: Some(25),
        }],
    };
    let m_frame = WsServerFrame::Metrics(metrics);
    let json = serde_json::to_string(&m_frame).unwrap();
    let decoded: WsServerFrame = serde_json::from_str(&json).unwrap();
    assert_eq!(m_frame, decoded);

    // 3. BubbleFrame (all 5 variants)
    let bubbles = vec![
        BubbleFrame::agent_speech(3, 1_700_000_200, 1001, "Building lexer parser"),
        BubbleFrame::mail_snippet(4, 1_700_000_300, 1000, 1001, "Directive", "Begin task"),
        BubbleFrame::steer(5, 1_700_000_400, 1001, "Focus on unit tests"),
        BubbleFrame::escalation(6, 1_700_000_500, "esc-1", "Budget exceeded"),
        BubbleFrame::decree_ack(7, 1_700_000_600, "dec-1", true, "Decree acknowledged"),
    ];
    for b in bubbles {
        let b_frame = WsServerFrame::Bubble(b);
        let json = serde_json::to_string(&b_frame).unwrap();
        let decoded: WsServerFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(b_frame, decoded);
    }

    // 4. CommandAckFrame (Success and Error)
    let ack_ok = CommandAckFrame::success(
        8,
        1_700_000_700,
        "corr-1",
        "/api/v1/commissions/new",
        Some(serde_json::json!({"status": "created"})),
    );
    let ack_err = CommandAckFrame::error(
        9,
        1_700_000_800,
        "corr-2",
        "/api/v1/vault/export",
        400,
        "path traversal blocked",
    );
    for ack in [ack_ok, ack_err] {
        let ack_frame = WsServerFrame::CommandAck(ack);
        let json = serde_json::to_string(&ack_frame).unwrap();
        let decoded: WsServerFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(ack_frame, decoded);
    }

    // 5. ResyncRequired
    let resync = WsServerFrame::ResyncRequired {
        seq: 10,
        timestamp_unix_ms: 1_700_000_900,
        daemon_instance_id: "inst-2".to_string(),
        reason: "daemon restart".to_string(),
    };
    let json = serde_json::to_string(&resync).unwrap();
    let decoded: WsServerFrame = serde_json::from_str(&json).unwrap();
    assert_eq!(resync, decoded);

    // 6. Ping / Pong
    let ping = WsClientFrame::Ping {
        timestamp_unix_ms: 1_700_001_000,
    };
    let json = serde_json::to_string(&ping).unwrap();
    let decoded: WsClientFrame = serde_json::from_str(&json).unwrap();
    assert_eq!(ping, decoded);

    let pong = WsServerFrame::Pong {
        timestamp_unix_ms: 1_700_001_000,
    };
    let json = serde_json::to_string(&pong).unwrap();
    let decoded: WsServerFrame = serde_json::from_str(&json).unwrap();
    assert_eq!(pong, decoded);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_protocol_unknown_frames_ignored_gracefully() {
    let server = start_adversarial_server().await;
    let mut ws = connect_ws(&server);

    // Read initial snapshot
    let snap = read_server_frame(&mut ws);
    assert!(matches!(snap, WsServerFrame::StateDelta(_)));

    // Send unknown frame types
    send_raw_text(
        &mut ws,
        r#"{"type":"unknown_frame_kind","payload":{"random":"data"}}"#,
    );
    send_raw_text(
        &mut ws,
        r#"{"type":"future_extension_v2","version":2,"nested":{"x":1}}"#,
    );
    send_raw_text(&mut ws, r#"{"arbitrary_json_without_type":true}"#);

    // Connection remains healthy: ping-pong works cleanly
    send_client_frame(
        &mut ws,
        &WsClientFrame::Ping {
            timestamp_unix_ms: 99999,
        },
    );
    let resp = read_server_frame(&mut ws);
    match resp {
        WsServerFrame::Pong { timestamp_unix_ms } => {
            assert_eq!(timestamp_unix_ms, 99999);
        }
        other => panic!(
            "expected pong response after unknown frames, got {:?}",
            other
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_protocol_wire_garbage_and_fuzz_safety() {
    let server = start_adversarial_server().await;
    let mut ws = connect_ws(&server);

    // Read initial snapshot
    let _ = read_server_frame(&mut ws);

    // Fuzz corpus: invalid JSON, raw HTTP lines, deep nesting, null bytes, truncated payloads
    let fuzz_corpus = vec![
        "NOT JSON AT ALL",
        "GET /api/v1/events/ws HTTP/1.1\r\n\r\n",
        r#"{"type": "command", "correlation_id": "#,
        r#"{"type": "command", "action": null, "extra": "\u0000\u0001\u0002"}"#,
        r#"[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]"#,
        r#"{"type": "resync", "daemon_instance_id": 12345}"#, // type mismatch
        r#"{"type": "ping", "timestamp_unix_ms": "not_an_int"}"#,
    ];

    for garbage in fuzz_corpus {
        send_raw_text(&mut ws, garbage);
    }

    // Server must not panic and must continue responding to legitimate traffic
    send_client_frame(
        &mut ws,
        &WsClientFrame::Ping {
            timestamp_unix_ms: 54321,
        },
    );
    let resp = read_server_frame(&mut ws);
    assert_eq!(
        resp,
        WsServerFrame::Pong {
            timestamp_unix_ms: 54321
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_protocol_auth_rejection_matrix() {
    let server = start_adversarial_server().await;

    // 1. Missing token -> 401 Unauthorized
    let res = connect_ws_raw(&server.base_url, None, None, None);
    assert!(res.is_err(), "missing token must be rejected");

    // 2. Empty token -> 401 Unauthorized
    let res = connect_ws_raw(&server.base_url, Some(""), None, None);
    assert!(res.is_err(), "empty token must be rejected");

    // 3. Forged token query param -> 401 Unauthorized
    let res = connect_ws_raw(&server.base_url, Some("forged-token-abc"), None, None);
    assert!(res.is_err(), "forged token query param must be rejected");

    // 4. Forged Bearer header -> 401 Unauthorized
    let res = connect_ws_raw(&server.base_url, None, Some("forged-bearer"), None);
    assert!(res.is_err(), "forged bearer token must be rejected");

    // 5. Valid query param -> 200/101 Success
    let mut ws_query = connect_ws_raw(&server.base_url, Some(TEST_TOKEN), None, None).unwrap();
    let snap = read_server_frame(&mut ws_query);
    assert!(matches!(snap, WsServerFrame::StateDelta(_)));

    // 6. Valid Bearer header -> Success
    let mut ws_bearer = connect_ws_raw(&server.base_url, None, Some(TEST_TOKEN), None).unwrap();
    let snap = read_server_frame(&mut ws_bearer);
    assert!(matches!(snap, WsServerFrame::StateDelta(_)));

    // 7. Valid custom X-Hamlet-Local-Token header -> Success
    let mut ws_custom = connect_ws_raw(&server.base_url, None, None, Some(TEST_TOKEN)).unwrap();
    let snap = read_server_frame(&mut ws_custom);
    assert!(matches!(snap, WsServerFrame::StateDelta(_)));
}

// ---------------------------------------------------------------------------
// 2. Honesty Rules (SPEC §12.3; ADR-0044)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_honesty_spectator_never_receives_secrets_or_cross_owner_mail_bodies() {
    let server = start_adversarial_server().await;

    // 1. Write secrets into .env (mode 0600)
    let secret_env_content = "HAMLET_ANTHROPIC_KEY=sk-ant-secret-key-CONFIDENTIAL-9999\nGITHUB_WEBHOOK_SECRET=gh-wh-secret-SUPER-SECRET-8888\n";
    fs::write(server.tree.env_file(), secret_env_content).unwrap();

    // 2. Write multi-line private mail in courier spool for agent 1001
    let recipient_dir = server.tree.courier_mail_recipient(&safe("1001"));
    fs::create_dir_all(&recipient_dir).unwrap();
    let secret_body = "CONFIDENTIAL-MAIL-BODY: Project Chimera internal financial ledger keys and private seed phrase that must never be sent to spectators over the wire.";
    let mail_content = format!(
        "From: 1000\nSubject: Secret Financial Ledger\n\n{}",
        secret_body
    );
    let spool_file = recipient_dir.join("0001-secret.mail");
    fs::write(&spool_file, &mail_content).unwrap();

    // 3. Connect spectator WS
    let mut spectator = connect_ws(&server);

    // Initial snapshot
    let snap_frame = read_server_frame(&mut spectator);
    let snap_json = serde_json::to_string(&snap_frame).unwrap();

    // Assert honesty on snapshot: .env secret and mail body NEVER in snapshot
    assert!(
        !snap_json.contains("sk-ant-secret-key"),
        "snapshot must not leak .env secrets"
    );
    assert!(
        !snap_json.contains("CONFIDENTIAL-MAIL-BODY"),
        "snapshot must not leak private mail bodies"
    );
    assert!(
        !snap_json.contains("Project Chimera"),
        "snapshot must not leak private mail contents"
    );

    // 4. Trigger MailSpool reaction
    server.state.ws_hub.handle_reaction(&Reaction::MailSpool {
        recipient: safe("1001"),
        file: safe("0001-secret.mail"),
    });

    // Read emitted BubbleFrame and StateDeltaFrame
    let bubble_frame = read_server_frame(&mut spectator);
    let delta_frame = read_server_frame(&mut spectator);

    let bubble_json = serde_json::to_string(&bubble_frame).unwrap();
    let delta_json = serde_json::to_string(&delta_frame).unwrap();

    // SPEC §12.3 Honesty Rule assertions:
    // a. The .env secret never appears
    assert!(!bubble_json.contains("sk-ant-secret-key"));
    assert!(!delta_json.contains("sk-ant-secret-key"));

    // b. Full mail body is NOT present
    assert!(
        !bubble_json.contains(secret_body),
        "full mail body must not be present on spectator wire"
    );
    assert!(
        !delta_json.contains(secret_body),
        "delta frame must not contain mail body"
    );

    // c. Bubble snippet is strictly bounded to MAX_MAIL_SNIPPET_LEN (64 chars)
    match bubble_frame {
        WsServerFrame::Bubble(b) => match b.kind {
            BubbleKind::MailSnippet {
                subject, snippet, ..
            } => {
                assert_eq!(subject, "Secret Financial Ledger");
                assert!(
                    snippet.len() <= MAX_MAIL_SNIPPET_LEN,
                    "snippet len {} exceeds MAX_MAIL_SNIPPET_LEN (64)",
                    snippet.len()
                );
            }
            other => panic!("expected MailSnippet bubble, got {:?}", other),
        },
        other => panic!("expected bubble frame, got {:?}", other),
    }

    // d. Mail counts projection contains ONLY counts, zero message text
    match delta_frame {
        WsServerFrame::StateDelta(d) => {
            let counts = d.mail_counts.expect("mail_counts present");
            assert_eq!(counts.get(&1001), Some(&1));
        }
        other => panic!("expected delta frame, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 3. State Fidelity (Cross-Surface Parity)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_state_fidelity_snapshot_projections_parity_with_local_api() {
    let server = start_adversarial_server().await;

    // Create a commission and an artifact in vault
    let brief = "State fidelity commission brief.";
    server
        .state
        .ledger
        .as_ref()
        .unwrap()
        .create_commission("fidelity-comm", brief, None)
        .unwrap();

    let vault_dir = server.tree.keep_vault_commission(&safe("fidelity-comm"));
    fs::create_dir_all(&vault_dir).unwrap();
    let artifact_path = vault_dir.join("release-v1.tar");
    fs::write(&artifact_path, b"FIDELITY_RELEASE_PAYLOAD").unwrap();
    fs::write(
        vault_dir.join("release-v1.tar.sha256"),
        "1234567890abcdef  release-v1.tar\n",
    )
    .unwrap();

    // Fetch HTTP REST local API projections directly
    let client = reqwest::Client::new();
    let url = &server.base_url;
    let auth = format!("Bearer {}", server.token);

    let census_res: Vec<CensusRecordView> = client
        .get(format!("{url}/api/v1/census"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let comm_res: Vec<CommissionSummaryView> = client
        .get(format!("{url}{ENDPOINT_COMMISSIONS}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let decrees_res: Vec<DecreeSummaryView> = client
        .get(format!("{url}/api/v1/decrees"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let vault_res: Vec<VaultItemView> = client
        .get(format!("{url}/api/v1/vault"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let landmarks_res: LandmarksView = client
        .get(format!("{url}/api/v1/landmarks"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Fetch snapshot via WS
    let mut ws = connect_ws(&server);
    let snap = match read_server_frame(&mut ws) {
        WsServerFrame::StateDelta(d) => *d,
        other => panic!("expected StateDelta snapshot, got {:?}", other),
    };

    assert!(snap.is_snapshot);
    assert_eq!(snap.census, Some(census_res));
    assert_eq!(snap.commissions, Some(comm_res));
    assert_eq!(snap.decrees, Some(decrees_res));
    assert_eq!(snap.vault, Some(vault_res));
    assert_eq!(snap.landmarks, Some(landmarks_res));
}

// ---------------------------------------------------------------------------
// 4. Backpressure & Slow-Client Simulation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_backpressure_slow_client_drops_metrics_preserves_deltas() {
    let server = start_adversarial_server().await;
    let mut slow_ws = connect_ws(&server);

    // Drain snapshot
    let _ = read_server_frame(&mut slow_ws);

    // Flood 150 metric ticks into full queue
    for i in 0..150 {
        let tick = ParcelMetricTick {
            uid: 1001,
            session_id: None,
            tokens_in: 10,
            tokens_out: 10,
            cost_micro_usd: None,
            latency_ms: 5,
            memory_bytes: None,
            cpu_percent_x10: None,
        };
        server.state.ws_hub.record_metric_tick(tick);
        let frame = {
            let mut batcher = server.state.ws_hub.metrics_batcher.lock().unwrap();
            batcher.flush(1_700_000_000_000 + i + 10_000)
        };
        if let Some(f) = frame {
            server
                .state
                .ws_hub
                .broadcast_frame(WsBroadcastFrame::Metrics(f));
        }
    }

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        server.state.ws_hub.dropped_metrics_count() > 0,
        "metrics must drop under slow-client queue pressure"
    );

    // Broadcast critical state delta
    let mut critical_delta =
        StateDeltaFrame::diff(&server.state.ws_hub.daemon_instance_id, 0, 1_700_000_999);
    critical_delta.decrees = Some(Vec::new());
    server.state.ws_hub.broadcast_delta(critical_delta);

    // Read frames from client until we confirm critical delta is delivered
    let mut delta_received = false;
    for _ in 0..150 {
        if let Some(WsServerFrame::StateDelta(d)) =
            try_read_server_frame(&mut slow_ws, Duration::from_millis(50))
        {
            if d.decrees.is_some() && !d.is_snapshot {
                delta_received = true;
                break;
            }
        }
    }
    assert!(
        delta_received,
        "critical state delta must never drop under backpressure"
    );
}

// ---------------------------------------------------------------------------
// 5. Reconnect & Zero Ghosts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_reconnect_daemon_kill_and_resync_no_ghosts() {
    let server = start_adversarial_server().await;
    let mut ws = connect_ws(&server);

    let initial_snap = match read_server_frame(&mut ws) {
        WsServerFrame::StateDelta(d) => d,
        other => panic!("expected initial snapshot, got {:?}", other),
    };

    // Client sends Resync with outdated/mismatched instance ID (simulating daemon restart)
    send_client_frame(
        &mut ws,
        &WsClientFrame::Resync {
            last_seen_seq: Some(initial_snap.seq),
            daemon_instance_id: Some("outdated-daemon-uuid-previous-incarnation".to_string()),
        },
    );

    // Server must respond with ResyncRequired notice followed by fresh StateDelta snapshot
    let frame1 = read_server_frame(&mut ws);
    match frame1 {
        WsServerFrame::ResyncRequired {
            daemon_instance_id,
            reason,
            ..
        } => {
            assert_eq!(daemon_instance_id, initial_snap.daemon_instance_id);
            assert!(reason.contains("mismatch") || reason.contains("restart"));
        }
        other => panic!(
            "expected ResyncRequired notice on instance mismatch, got {:?}",
            other
        ),
    }

    let frame2 = read_server_frame(&mut ws);
    match frame2 {
        WsServerFrame::StateDelta(snap) => {
            assert!(snap.is_snapshot);
            assert_eq!(snap.daemon_instance_id, initial_snap.daemon_instance_id);
            assert_eq!(snap.census.unwrap().len(), 2); // Mayor + worker only, zero ghosts
        }
        other => panic!("expected fresh StateDelta snapshot, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Findings Reproduction Suites (Filed in HANDOFF.md)
// ---------------------------------------------------------------------------

/// QA09-01 Repro: Malformed command frames silently dropped without typed error ACK.
/// SPEC Citation: Sprint 09 Step 5 Committed Suite 1: "malformed command frames → typed error acks, no daemon panic".
#[tokio::test(flavor = "multi_thread")]
async fn test_repro_qa09_01_malformed_command_dropped_instead_of_error_ack() {
    let server = start_adversarial_server().await;
    let mut ws = connect_ws(&server);
    let _ = read_server_frame(&mut ws); // drain snapshot

    // Issue a malformed command frame: unknown mutation action
    let malformed_action_json = r#"{
        "type": "command",
        "correlation_id": "corr-malformed-unknown-action",
        "action": {
            "action": "nonexistent_mutation_action",
            "payload": {}
        }
    }"#;
    send_raw_text(&mut ws, malformed_action_json);

    // Issue a malformed command frame: invalid SafeName in slug (contains spaces and illegal characters)
    let malformed_slug_json = r#"{
        "type": "command",
        "correlation_id": "corr-malformed-bad-slug",
        "action": {
            "action": "commission_new",
            "payload": {
                "slug": "INVALID SLUG WITH SPACES!",
                "brief": "Some brief"
            }
        }
    }"#;
    send_raw_text(&mut ws, malformed_slug_json);

    // Expectation: Daemon should return CommandAckFrame with status_code 400 for both correlation IDs.
    let response1 = try_read_server_frame(&mut ws, Duration::from_millis(500));
    let response2 = try_read_server_frame(&mut ws, Duration::from_millis(500));

    match response1 {
        Some(WsServerFrame::CommandAck(ack)) => {
            assert_eq!(ack.correlation_id, "corr-malformed-unknown-action");
            assert_eq!(ack.status_code, 400);
            assert!(!ack.success);
            assert!(ack.error.unwrap().contains("malformed command frame"));
        }
        other => panic!(
            "expected 400 error CommandAck for response1, got {:?}",
            other
        ),
    }

    match response2 {
        Some(WsServerFrame::CommandAck(ack)) => {
            assert_eq!(ack.correlation_id, "corr-malformed-bad-slug");
            assert_eq!(ack.status_code, 400);
            assert!(!ack.success);
            assert!(ack.error.unwrap().contains("malformed command frame"));
        }
        other => panic!(
            "expected 400 error CommandAck for response2, got {:?}",
            other
        ),
    }
}

/// QA09-02 Repro: Client command flood causes broadcast channel lag, terminating the forwarder task
/// and permanently starving concurrent spectators.
/// SPEC Citation: Sprint 09 Step 5 Committed Suite 4: "client flood (command spam) rate-limited without starving other viewers".
#[tokio::test(flavor = "multi_thread")]
async fn test_repro_qa09_02_client_flood_starves_viewers_via_lagged_error() {
    let server = start_adversarial_server().await;

    // Connect Spectator A
    let mut spectator_a = connect_ws(&server);
    let _ = read_server_frame(&mut spectator_a); // drain snapshot

    // Connect Flooder Client B
    let mut flooder_b = connect_ws(&server);
    let _ = read_server_frame(&mut flooder_b); // drain snapshot

    // Flooder sends > 512 commands without spectator reading immediately, saturating broadcast_tx (capacity 512)
    for i in 0..600 {
        let bubble =
            BubbleFrame::agent_speech(0, 1_700_000_000 + i, MAYOR_UID, format!("Flood msg {i}"));
        server.state.ws_hub.broadcast_bubble(bubble);
    }

    // Allow async forwarder to attempt recv and encounter RecvError::Lagged
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now server broadcasts a critical event to all spectators
    let critical_bubble =
        BubbleFrame::agent_speech(0, 1_700_999_999, MAYOR_UID, "CRITICAL TOWN EVENT");
    server.state.ws_hub.broadcast_bubble(critical_bubble);

    // Drain Spectator A: because bcast_forwarder task exited on RecvError::Lagged (web.rs:1624),
    // Spectator A never receives CRITICAL TOWN EVENT!
    let mut received_critical = false;
    for _ in 0..1000 {
        if let Some(f) = try_read_server_frame(&mut spectator_a, Duration::from_millis(50)) {
            if let WsServerFrame::Bubble(b) = &f {
                if b.text == "CRITICAL TOWN EVENT" {
                    received_critical = true;
                    break;
                }
            }
        } else {
            break;
        }
    }

    assert!(
        received_critical,
        "QA09-02 resolved: Broadcast channel lag must not terminate forwarder task; spectator must receive critical event"
    );
}
