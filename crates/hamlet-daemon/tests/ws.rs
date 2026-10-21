//! WebSocket streaming and static asset serving integration tests (SPEC §12.3; ADR-0044).
//!
//! Verifies:
//! 1. Static asset serving (Cache-Control, ETag, 304 Not Modified, directory traversal protection).
//! 2. WebSocket auth handshake (query param, bearer token, custom header, rejection of bad tokens).
//! 3. Initial full StateDelta snapshot with all 7 projection slots populated.
//! 4. Ping/Pong and Resync handling (instance ID mismatch triggers ResyncRequired).
//! 5. Live projection deltas and chat bubbles under reactions with SPEC §12.3 honesty rule.
//! 6. Sled metrics rate-limited batching and flushing.
//! 7. Bidirectional client mutation command execution and correlated CommandAckFrame.
//! 8. Multi-client fan-out with per-connection monotonic sequencing.
//! 9. Backpressure: metrics dropping on congested client queues while preserving state deltas and acks.

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
use hamlet_daemon::web::AssetServer;
use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use reqwest::StatusCode;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::stream::MaybeTlsStream;
use tokio_tungstenite::tungstenite::{connect, Message, WebSocket};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const TEST_TOKEN: &str = "test-token-0123456789abcdef0123456789abcdef";

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("valid safe name")
}

struct TestServer {
    _tmp: tempfile::TempDir,
    tree: Arc<HamletTree>,
    base_url: String,
    _token: LocalToken,
    _shutdown_tx: watch::Sender<bool>,
    state: Arc<LocalApiState>,
}

async fn start_test_server() -> TestServer {
    let tmp = tempfile::tempdir().unwrap();
    let tree = Arc::new(HamletTree::ensure(tmp.path().join(".hamlet")).unwrap());

    // Initialize minimal charter
    let mut charter = Charter::empty();
    charter.reserve_mayor(name("mayor")).unwrap();
    charter.allocate_slot(name("worker"), None).unwrap();
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

    // Point asset server at the built hamlet-web dist directory
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

    TestServer {
        _tmp: tmp,
        tree,
        base_url: format!("http://{}", addr),
        _token: token,
        _shutdown_tx: shutdown_tx,
        state,
    }
}

type WsClient = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

#[allow(clippy::result_large_err)]
fn connect_ws(
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
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    }
    Ok(socket)
}

fn read_server_frame(socket: &mut WsClient) -> WsServerFrame {
    loop {
        match socket.read().expect("read ws message") {
            Message::Text(t) => match serde_json::from_str::<WsServerFrame>(&t) {
                Ok(f) => return f,
                Err(e) => {
                    eprintln!("FAILED FRAME PARSE: {}\nJSON:\n{}", e, t);
                    panic!("valid WsServerFrame JSON: {}", e);
                }
            },
            Message::Ping(p) => {
                let _ = socket.write(Message::Pong(p));
                let _ = socket.flush();
            }
            Message::Pong(_) => {}
            other => panic!("unexpected ws message: {:?}", other),
        }
    }
}

fn read_ack_frame(socket: &mut WsClient, correlation_id: &str) -> CommandAckFrame {
    for _ in 0..20 {
        match read_server_frame(socket) {
            WsServerFrame::CommandAck(ack) => {
                if ack.correlation_id == correlation_id {
                    return ack;
                }
            }
            _ => continue,
        }
    }
    panic!(
        "timed out waiting for ack with correlation_id: {}",
        correlation_id
    );
}

fn send_client_frame(socket: &mut WsClient, frame: &WsClientFrame) {
    let t = serde_json::to_string(frame).expect("valid WsClientFrame JSON");
    socket.write(Message::Text(t)).expect("write ws message");
    socket.flush().expect("flush ws message");
}

// ---------------------------------------------------------------------------
// 1. Static Asset Serving Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_static_asset_serving_and_traversal_defense() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    // 1. GET / (index.html) -> 200, text/html, Cache-Control: no-cache
    let res = client.get(&server.base_url).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(CACHE_CONTROL).unwrap().to_str().unwrap(),
        "no-cache"
    );
    assert!(res
        .headers()
        .get(CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let body = res.text().await.unwrap();
    assert!(
        body.to_lowercase().contains("hamlet"),
        "index.html content mismatch"
    );

    // 2. GET /index.html -> same no-cache
    let res = client
        .get(format!("{}/index.html", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(CACHE_CONTROL).unwrap().to_str().unwrap(),
        "no-cache"
    );

    // 3. GET /manifest.json -> 200, application/json, version_hash present
    let res = client
        .get(format!("{}/manifest.json", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let manifest_val: serde_json::Value = res.json().await.unwrap();
    let version_hash = manifest_val["version_hash"].as_str().unwrap();
    let expected_etag = format!("\"{}\"", version_hash);

    // 4. GET /hamlet.js -> 200, immutable cache header, ETag present
    let res = client
        .get(format!("{}/hamlet.js", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(CACHE_CONTROL).unwrap().to_str().unwrap(),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        res.headers().get(ETAG).unwrap().to_str().unwrap(),
        expected_etag
    );

    // 5. GET /style.css -> 200, immutable cache header, text/css
    let res = client
        .get(format!("{}/style.css", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(CACHE_CONTROL).unwrap().to_str().unwrap(),
        "public, max-age=31536000, immutable"
    );
    assert!(res
        .headers()
        .get(CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/css"));

    // 6. GET /assets/village_map.json -> 200, immutable
    let res = client
        .get(format!("{}/assets/village_map.json", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(CACHE_CONTROL).unwrap().to_str().unwrap(),
        "public, max-age=31536000, immutable"
    );

    // 7. Conditional request: If-None-Match matching ETag -> 304 Not Modified
    let res = client
        .get(format!("{}/hamlet.js", server.base_url))
        .header(IF_NONE_MATCH, &expected_etag)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    let body = res.bytes().await.unwrap();
    assert!(body.is_empty(), "304 body must be empty");

    // 8. Conditional request: If-None-Match mismatch -> 200 OK
    let res = client
        .get(format!("{}/hamlet.js", server.base_url))
        .header(IF_NONE_MATCH, "\"wrong-etag\"")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // 9. Traversal defense: path with .. must be rejected (403 Forbidden)
    let traversal_resp = server
        .state
        .asset_server
        .serve_file("../Cargo.toml", false, None);
    assert_eq!(traversal_resp.status(), axum::http::StatusCode::FORBIDDEN);

    let traversal_resp2 =
        server
            .state
            .asset_server
            .serve_file("assets/../../etc/passwd", false, None);
    assert_eq!(traversal_resp2.status(), axum::http::StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// 2. WebSocket Authentication Matrix Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_ws_auth_handshake_matrix() {
    let server = start_test_server().await;

    // 1. Success via ?token= query parameter
    let mut ws1 = connect_ws(&server.base_url, Some(TEST_TOKEN), None, None)
        .expect("connection with query token succeeds");
    let frame1 = read_server_frame(&mut ws1);
    assert!(matches!(frame1, WsServerFrame::StateDelta(_)));

    // 2. Success via Authorization: Bearer <token>
    let mut ws2 = connect_ws(&server.base_url, None, Some(TEST_TOKEN), None)
        .expect("connection with bearer header succeeds");
    let frame2 = read_server_frame(&mut ws2);
    assert!(matches!(frame2, WsServerFrame::StateDelta(_)));

    // 3. Success via X-Hamlet-Token: <token>
    let mut ws3 = connect_ws(&server.base_url, None, None, Some(TEST_TOKEN))
        .expect("connection with custom header succeeds");
    let frame3 = read_server_frame(&mut ws3);
    assert!(matches!(frame3, WsServerFrame::StateDelta(_)));

    // 4. Rejection via bad token (401)
    let res_bad = connect_ws(&server.base_url, Some("wrong-secret-token"), None, None);
    assert!(res_bad.is_err(), "bad token must be rejected");

    // 5. Rejection via missing token (401)
    let res_missing = connect_ws(&server.base_url, None, None, None);
    assert!(res_missing.is_err(), "missing token must be rejected");
}

// ---------------------------------------------------------------------------
// 3. Initial Snapshot & Projections Test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_ws_initial_snapshot_contains_all_seven_projections() {
    let server = start_test_server().await;
    let mut ws =
        connect_ws(&server.base_url, Some(TEST_TOKEN), None, None).expect("connect succeeds");

    let frame = read_server_frame(&mut ws);
    match frame {
        WsServerFrame::StateDelta(delta) => {
            assert!(delta.is_snapshot, "first frame must be full snapshot");
            assert_eq!(delta.seq, 1, "first frame sequence number must be 1");
            assert!(
                !delta.daemon_instance_id.is_empty(),
                "daemon_instance_id must not be empty"
            );

            // Verify all 7 projections are populated in the snapshot
            assert!(delta.census.is_some(), "census must be present");
            assert!(delta.commissions.is_some(), "commissions must be present");
            assert!(delta.decrees.is_some(), "decrees must be present");
            assert!(delta.vault.is_some(), "vault must be present");
            assert!(delta.mail_counts.is_some(), "mail_counts must be present");
            assert!(delta.landmarks.is_some(), "landmarks must be present");
            assert!(delta.escalations.is_some(), "escalations must be present");

            // Verify landmarks content
            let landmarks = delta.landmarks.unwrap();
            assert_eq!(landmarks.manor.decrees_rejected, 0);
            assert_eq!(landmarks.manor.escalations_open, 0);
        }
        other => panic!("expected StateDelta frame, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 4. Ping/Pong & Resync Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_ws_ping_pong_and_resync() {
    let server = start_test_server().await;
    let mut ws =
        connect_ws(&server.base_url, Some(TEST_TOKEN), None, None).expect("connect succeeds");

    // Drain initial snapshot
    let snap = match read_server_frame(&mut ws) {
        WsServerFrame::StateDelta(d) => d,
        other => panic!("expected snapshot, got {:?}", other),
    };

    // 1. Ping -> Pong round-trip
    let ping_time = 1_700_000_123_456u64;
    send_client_frame(
        &mut ws,
        &WsClientFrame::Ping {
            timestamp_unix_ms: ping_time,
        },
    );
    let frame = read_server_frame(&mut ws);
    match frame {
        WsServerFrame::Pong { timestamp_unix_ms } => {
            assert_eq!(timestamp_unix_ms, ping_time);
        }
        other => panic!("expected Pong, got {:?}", other),
    }

    // 2. Resync with valid instance id -> fresh StateDelta snapshot
    send_client_frame(
        &mut ws,
        &WsClientFrame::Resync {
            last_seen_seq: Some(1),
            daemon_instance_id: Some(snap.daemon_instance_id.clone()),
        },
    );
    let frame = read_server_frame(&mut ws);
    match frame {
        WsServerFrame::StateDelta(delta) => {
            assert!(delta.is_snapshot, "resync response must be a snapshot");
            assert!(delta.seq > 1, "resync sequence must be monotonic");
            assert_eq!(delta.daemon_instance_id, snap.daemon_instance_id);
        }
        other => panic!("expected StateDelta on resync, got {:?}", other),
    }

    // 3. Resync with mismatched instance id -> ResyncRequired frame
    send_client_frame(
        &mut ws,
        &WsClientFrame::Resync {
            last_seen_seq: Some(5),
            daemon_instance_id: Some("outdated-daemon-uuid-999".to_string()),
        },
    );
    let frame = read_server_frame(&mut ws);
    match frame {
        WsServerFrame::ResyncRequired {
            daemon_instance_id,
            reason,
            ..
        } => {
            assert_eq!(daemon_instance_id, snap.daemon_instance_id);
            assert!(reason.contains("mismatch") || reason.contains("restart"));
        }
        other => panic!("expected ResyncRequired, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 5. Live Reactions & SPEC §12.3 Honesty Rule Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_ws_live_reactions_and_bubble_honesty_rule() {
    let server = start_test_server().await;
    let mut ws =
        connect_ws(&server.base_url, Some(TEST_TOKEN), None, None).expect("connect succeeds");

    // Drain initial snapshot
    let _ = read_server_frame(&mut ws);

    // Simulate inotify reaction: MailSpool with secret body
    let recipient = name("1001");
    let spool_dir = server.tree.courier_mail_recipient(&recipient);
    fs::create_dir_all(&spool_dir).unwrap();
    let mail_file = spool_dir.join("msg-001.txt");

    let mail_content = "\
From: 1000
To: 1001
Subject: Project Alpha Directives

Starting task. Full body details follow below in private section.
SECRET_OPERATOR_API_KEY_DO_NOT_EXPOSE
This is confidential payload data that must never appear in spectator streams.";
    fs::write(&mail_file, mail_content).unwrap();

    server.state.ws_hub.handle_reaction(&Reaction::MailSpool {
        recipient,
        file: name("msg-001.txt"),
    });

    // 1. Verify Bubble frame
    let frame1 = read_server_frame(&mut ws);
    match frame1 {
        WsServerFrame::Bubble(bubble) => {
            assert_eq!(bubble.target, BubbleTarget::Agent(1001));
            match bubble.kind {
                BubbleKind::MailSnippet {
                    from_uid,
                    to_uid,
                    subject,
                    snippet,
                } => {
                    assert_eq!(from_uid, 1000);
                    assert_eq!(to_uid, 1001);
                    assert_eq!(subject, "Project Alpha Directives");
                    // SPEC §12.3 Honesty Rule: mail bodies are strictly suppressed on wire
                    assert!(
                        !snippet.contains("SECRET_OPERATOR_API_KEY_DO_NOT_EXPOSE"),
                        "secret key must not appear in snippet"
                    );
                }
                other => panic!("expected MailSnippet bubble, got {:?}", other),
            }
            assert!(
                !bubble
                    .text
                    .contains("SECRET_OPERATOR_API_KEY_DO_NOT_EXPOSE"),
                "full mail body must never leak into chat bubble text"
            );
        }
        other => panic!("expected Bubble frame, got {:?}", other),
    }

    // 2. Verify StateDelta frame with mail_counts
    let frame2 = read_server_frame(&mut ws);
    match frame2 {
        WsServerFrame::StateDelta(delta) => {
            assert!(!delta.is_snapshot, "reaction delta is not a snapshot");
            assert!(delta.mail_counts.is_some(), "mail_counts must be present");
        }
        other => panic!("expected StateDelta frame, got {:?}", other),
    }

    // Simulate inotify reaction: DecreeWrite
    server.state.ws_hub.handle_reaction(&Reaction::DecreeWrite {
        path: PathBuf::from("001_commission_start.md"),
    });

    // Verify Decree bubble + delta
    let frame3 = read_server_frame(&mut ws);
    assert!(matches!(frame3, WsServerFrame::Bubble(_)));
    let frame4 = read_server_frame(&mut ws);
    match frame4 {
        WsServerFrame::StateDelta(delta) => {
            assert!(
                delta.decrees.is_some(),
                "decrees projection must be present"
            );
        }
        other => panic!("expected StateDelta for decrees, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 6. Metrics Batching Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_ws_metrics_batching_and_flushing() {
    let server = start_test_server().await;
    let mut ws =
        connect_ws(&server.base_url, Some(TEST_TOKEN), None, None).expect("connect succeeds");

    // Drain initial snapshot
    let _ = read_server_frame(&mut ws);

    // Record 3 metric ticks
    let tick1 = ParcelMetricTick {
        uid: 1001,
        session_id: Some("sess-1".into()),
        tokens_in: 25,
        tokens_out: 10,
        cost_micro_usd: Some(100),
        latency_ms: 15,
        memory_bytes: Some(32 * 1024 * 1024),
        cpu_percent_x10: Some(50),
    };
    let tick2 = ParcelMetricTick {
        uid: 1001,
        session_id: Some("sess-1".into()),
        tokens_in: 50,
        tokens_out: 20,
        cost_micro_usd: Some(200),
        latency_ms: 25,
        memory_bytes: Some(34 * 1024 * 1024),
        cpu_percent_x10: Some(100),
    };
    let tick3 = ParcelMetricTick {
        uid: 1002,
        session_id: Some("sess-2".into()),
        tokens_in: 15,
        tokens_out: 5,
        cost_micro_usd: Some(50),
        latency_ms: 10,
        memory_bytes: Some(16 * 1024 * 1024),
        cpu_percent_x10: Some(25),
    };

    server
        .state
        .ws_hub
        .record_metric_ticks(vec![tick1, tick2, tick3]);
    let flushed = server.state.ws_hub.flush_metrics();
    assert!(flushed, "metrics batch must flush when non-empty");

    let frame = read_server_frame(&mut ws);
    match frame {
        WsServerFrame::Metrics(batch) => {
            assert_eq!(batch.ticks.len(), 3, "must batch exactly 3 ticks");
            assert_eq!(batch.ticks[0].uid, 1001);
            assert_eq!(batch.ticks[2].uid, 1002);
            assert!(batch.seq > 1);
        }
        other => panic!("expected Metrics frame, got {:?}", other),
    }

    // Flushing an empty batch returns false and produces no frame
    let flushed_again = server.state.ws_hub.flush_metrics();
    assert!(!flushed_again, "empty flush returns false");
}

// ---------------------------------------------------------------------------
// 7. Client Mutation Command Round-Trip Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_ws_command_round_trip_and_ack_correlation() {
    let server = start_test_server().await;
    let mut ws =
        connect_ws(&server.base_url, Some(TEST_TOKEN), None, None).expect("connect succeeds");

    // Drain initial snapshot
    let _ = read_server_frame(&mut ws);

    // 1. CommissionNew command
    let cmd_comm = WsClientFrame::Command(WsCommandFrame {
        correlation_id: "corr-commission-1".to_string(),
        action: WsCommandAction::CommissionNew(CommissionNewRequest {
            slug: name("web-sprint-09"),
            brief: "Building the 8-bit web canvas".to_string(),
            seed_dir: None,
        }),
    });
    send_client_frame(&mut ws, &cmd_comm);

    // Should receive CommandAckFrame correlated to corr-commission-1
    let ack1 = read_ack_frame(&mut ws, "corr-commission-1");
    assert_eq!(ack1.correlation_id, "corr-commission-1");
    assert_eq!(ack1.command_endpoint, "/api/v1/commissions/new");
    assert_eq!(ack1.status_code, 200);
    assert!(ack1.success);
    let resp: CommissionNewResponse =
        serde_json::from_value(ack1.payload.expect("ack response body")).unwrap();
    assert_eq!(resp.slug, name("web-sprint-09"));
    assert_eq!(resp.state, "draft");

    // 2. CommissionClose command
    let cmd_close = WsClientFrame::Command(WsCommandFrame {
        correlation_id: "corr-commission-close-1".to_string(),
        action: WsCommandAction::CommissionClose {
            slug: name("web-sprint-09"),
            request: CommissionCloseRequest {
                slug: name("web-sprint-09"),
            },
        },
    });
    send_client_frame(&mut ws, &cmd_close);

    let ack2 = read_ack_frame(&mut ws, "corr-commission-close-1");
    assert_eq!(ack2.correlation_id, "corr-commission-close-1");
    assert_eq!(
        ack2.command_endpoint,
        "/api/v1/commissions/web-sprint-09/close"
    );
    assert_eq!(ack2.status_code, 200);
    assert!(ack2.success);
    let resp: CommissionCloseResponse =
        serde_json::from_value(ack2.payload.expect("ack response body")).unwrap();
    assert_eq!(resp.slug, name("web-sprint-09"));
    assert_eq!(resp.state, "closed");

    // 3. MailSend command (Courier panel)
    let cmd_mail = WsClientFrame::Command(WsCommandFrame {
        correlation_id: "corr-mail-1".to_string(),
        action: WsCommandAction::MailSend(MailSendRequest {
            to: 1000,
            from: Some("builder".to_string()),
            subject: "Update from parcel".to_string(),
            body: "Lexer parser tasks completed.".to_string(),
        }),
    });
    send_client_frame(&mut ws, &cmd_mail);

    let ack_mail = read_ack_frame(&mut ws, "corr-mail-1");
    assert_eq!(ack_mail.correlation_id, "corr-mail-1");
    assert_eq!(ack_mail.command_endpoint, "/api/v1/mail/send");
    assert_eq!(ack_mail.status_code, 200);
    assert!(ack_mail.success);

    // 4. Command rejection and error ACK correlation (VaultExport for non-existent artifact)
    let cmd_reject = WsClientFrame::Command(WsCommandFrame {
        correlation_id: "corr-reject-vault-1".to_string(),
        action: WsCommandAction::VaultExport(VaultExportRequest {
            artifact: "non-existent-artifact.tar.gz".to_string(),
            destination: "/tmp/out.tar".to_string(),
            commission: None,
        }),
    });
    send_client_frame(&mut ws, &cmd_reject);

    let ack_reject = read_ack_frame(&mut ws, "corr-reject-vault-1");
    assert_eq!(ack_reject.correlation_id, "corr-reject-vault-1");
    assert_eq!(ack_reject.command_endpoint, "/api/v1/vault/export");
    assert_eq!(ack_reject.status_code, 400);
    assert!(!ack_reject.success);
    assert!(ack_reject.error.is_some());
}

// ---------------------------------------------------------------------------
// 8. Multi-Client Fan-Out & Monotonic Sequencing Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_ws_concurrent_fan_out_to_multiple_viewers() {
    let server = start_test_server().await;

    // Connect Client A and Client B
    let mut client_a = connect_ws(&server.base_url, Some(TEST_TOKEN), None, None).unwrap();
    let mut client_b = connect_ws(&server.base_url, Some(TEST_TOKEN), None, None).unwrap();

    // Both receive snapshot with their own seq == 1
    let snap_a = match read_server_frame(&mut client_a) {
        WsServerFrame::StateDelta(d) => d,
        other => panic!("client A expected snapshot, got {:?}", other),
    };
    assert_eq!(snap_a.seq, 1);

    let snap_b = match read_server_frame(&mut client_b) {
        WsServerFrame::StateDelta(d) => d,
        other => panic!("client B expected snapshot, got {:?}", other),
    };
    assert_eq!(snap_b.seq, 1);

    // Server broadcasts a speech bubble
    let bubble = BubbleFrame::agent_speech(
        0,
        1_700_000_000_000,
        MAYOR_UID,
        "Town hall broadcast to all spectators",
    );
    server.state.ws_hub.broadcast_bubble(bubble);

    // Both clients receive the bubble with seq == 2
    let frame_a = read_server_frame(&mut client_a);
    match frame_a {
        WsServerFrame::Bubble(b) => {
            assert_eq!(b.seq, 2, "client A sequence must be 2");
            assert_eq!(b.text, "Town hall broadcast to all spectators");
        }
        other => panic!("client A expected bubble, got {:?}", other),
    }

    let frame_b = read_server_frame(&mut client_b);
    match frame_b {
        WsServerFrame::Bubble(b) => {
            assert_eq!(b.seq, 2, "client B sequence must be 2");
            assert_eq!(b.text, "Town hall broadcast to all spectators");
        }
        other => panic!("client B expected bubble, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// 9. Backpressure & Slow Client Behavior Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_ws_backpressure_slow_client_drops_metrics_preserves_deltas() {
    let server = start_test_server().await;
    let mut slow_ws = connect_ws(&server.base_url, Some(TEST_TOKEN), None, None).unwrap();

    // Drain initial snapshot
    let _ = read_server_frame(&mut slow_ws);

    // Flood 100 metrics batch frames without reading from slow_ws
    for i in 0..100 {
        let tick = ParcelMetricTick {
            uid: 1001,
            session_id: None,
            tokens_in: 1,
            tokens_out: 1,
            cost_micro_usd: None,
            latency_ms: 1,
            memory_bytes: None,
            cpu_percent_x10: None,
        };
        server.state.ws_hub.record_metric_tick(tick);
        // Force flush
        let frame = {
            let mut batcher = server.state.ws_hub.metrics_batcher.lock().unwrap();
            batcher.flush(1_700_000_000_000 + i + 10_000)
        };
        if let Some(f) = frame {
            server
                .state
                .ws_hub
                .broadcast_frame(hamlet_daemon::web::WsBroadcastFrame::Metrics(f));
        }
    }

    // Allow background channel try_send to run
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify metrics frames were dropped due to bounded queue (capacity 64)
    let dropped = server.state.ws_hub.dropped_metrics_count();
    assert!(
        dropped > 0,
        "backpressure drop policy must drop metrics ticks on full queue, dropped={dropped}"
    );

    // Now broadcast a critical state delta (policy: NEVER drop state deltas)
    let mut critical_delta = StateDeltaFrame::diff(
        &server.state.ws_hub.daemon_instance_id,
        0,
        1_700_000_999_999,
    );
    critical_delta.mail_counts = Some(BTreeMap::new());
    server.state.ws_hub.broadcast_delta(critical_delta);

    // Drain the client until we find the critical state delta
    let mut found_critical_delta = false;
    for _ in 0..120 {
        match read_server_frame(&mut slow_ws) {
            WsServerFrame::StateDelta(d) => {
                if d.mail_counts.is_some() && !d.is_snapshot {
                    found_critical_delta = true;
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(
        found_critical_delta,
        "critical state delta must be preserved despite queue congestion"
    );
}
