//! Integration and table-driven route tests for the local daemon API (SPEC §12.1–§12.2; ADR-0041).
//!
//! Verifies:
//! 1. Complete route table: method, path, authentication enforcement, status codes.
//! 2. State projections: daemon status, census, ledger commissions, decrees, vault, metrics, mail.
//! 3. Command execution: commission creation/close, mail sending, daemon stop.
//! 4. File lifecycle: daemon.token (0600), daemon.pid, daemon.endpoint.
//! 5. WebSocket event streaming over local API.

use std::fs;
use std::sync::Arc;

use axum::http::StatusCode;
use hamlet_core::api::*;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::decree::DecreeId;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::local_api::{
    cleanup_daemon_files, local_api_router, write_daemon_files, LocalApiState,
};
use hamlet_daemon::mailroute::MailRouter;
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::Telemetry;
use reqwest::header::AUTHORIZATION;
use tokio::net::TcpListener;
use tokio::sync::watch;

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const TEST_TOKEN: &str = "test-token-0123456789abcdef0123456789abcdef";

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("valid safe name")
}

struct TestServer {
    _tmp: tempfile::TempDir,
    tree: Arc<HamletTree>,
    base_url: String,
    token: LocalToken,
    _shutdown_tx: watch::Sender<bool>,
    _state: Arc<LocalApiState>,
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

    let state = Arc::new(
        LocalApiState::new(tree.clone(), token.clone(), shutdown_tx.clone())
            .with_supervisor(supervisor)
            .with_mail_router(mail_router),
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
        token,
        _shutdown_tx: shutdown_tx,
        _state: state,
    }
}

// ---------------------------------------------------------------------------
// Table-Driven Route Matrix Tests
// ---------------------------------------------------------------------------

struct RouteTestCase {
    method: &'static str,
    path: &'static str,
    body: Option<&'static str>,
    expected_status_unauth: StatusCode,
    expected_status_auth: StatusCode,
    description: &'static str,
}

#[tokio::test]
async fn test_table_driven_route_matrix() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();

    let cases = vec![
        RouteTestCase {
            method: "GET",
            path: ENDPOINT_STATUS,
            body: None,
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "daemon status projection",
        },
        RouteTestCase {
            method: "GET",
            path: ENDPOINT_CENSUS,
            body: None,
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "census records projection",
        },
        RouteTestCase {
            method: "GET",
            path: ENDPOINT_COMMISSIONS,
            body: None,
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "commission listing",
        },
        RouteTestCase {
            method: "GET",
            path: ENDPOINT_DECREES,
            body: None,
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "decrees listing",
        },
        RouteTestCase {
            method: "GET",
            path: ENDPOINT_VAULT,
            body: None,
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "vault listing",
        },
        RouteTestCase {
            method: "GET",
            path: ENDPOINT_METRICS,
            body: None,
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "telemetry metrics",
        },
        RouteTestCase {
            method: "GET",
            path: "/api/v1/mail/1001",
            body: None,
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "mail listing for slot 1001",
        },
        RouteTestCase {
            method: "POST",
            path: ENDPOINT_COMMISSION_NEW,
            body: Some(r#"{"slug":"alpha","brief":"build compiler","seed_dir":null}"#),
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::CREATED,
            description: "create commission",
        },
        RouteTestCase {
            method: "POST",
            path: ENDPOINT_MAIL_SEND,
            body: Some(r#"{"from":"operator","to":1001,"subject":"hi","body":"welcome"}"#),
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "send mail",
        },
        RouteTestCase {
            method: "GET",
            path: ENDPOINT_LANDMARKS,
            body: None,
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "landmarks projection",
        },
        RouteTestCase {
            method: "GET",
            path: ENDPOINT_ESCALATIONS,
            body: None,
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "escalations projection",
        },
        RouteTestCase {
            method: "POST",
            path: ENDPOINT_STOP,
            body: Some("{}"),
            expected_status_unauth: StatusCode::UNAUTHORIZED,
            expected_status_auth: StatusCode::OK,
            description: "daemon stop command",
        },
    ];

    for tc in cases {
        let url = format!("{}{}", server.base_url, tc.path);

        // 1. Without auth -> must return 401
        let req = match tc.method {
            "GET" => client.get(&url),
            "POST" => {
                let r = client.post(&url);
                if let Some(b) = tc.body {
                    r.header("content-type", "application/json").body(b)
                } else {
                    r
                }
            }
            _ => panic!("unsupported method"),
        };

        let res = req.send().await.unwrap();
        assert_eq!(
            res.status(),
            tc.expected_status_unauth,
            "Route {} ({}) without auth should fail with {}",
            tc.path,
            tc.description,
            tc.expected_status_unauth
        );

        let err_json: ApiErrorResponse = res.json().await.unwrap();
        assert_eq!(err_json.error, "unauthorized");

        // 2. With auth -> must match expected_status_auth
        let req_auth = match tc.method {
            "GET" => client.get(&url),
            "POST" => {
                let r = client.post(&url);
                if let Some(b) = tc.body {
                    r.header("content-type", "application/json").body(b)
                } else {
                    r
                }
            }
            _ => panic!("unsupported method"),
        }
        .header(AUTHORIZATION, format!("Bearer {}", server.token.as_str()));

        let res_auth = req_auth.send().await.unwrap();
        assert_eq!(
            res_auth.status(),
            tc.expected_status_auth,
            "Route {} ({}) with auth should succeed with {}",
            tc.path,
            tc.description,
            tc.expected_status_auth
        );
    }
}

// ---------------------------------------------------------------------------
// Detailed Functional Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_daemon_status_and_custom_header_auth() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let url = format!("{}{}", server.base_url, ENDPOINT_STATUS);

    // Test custom header `x-hamlet-local-token`
    let res = client
        .get(&url)
        .header(LOCAL_TOKEN_HEADER, server.token.as_str())
        .send()
        .await
        .unwrap();

    let code = res.status();
    let body_text = res.text().await.unwrap();
    println!("STATUS: {code}, BODY: {body_text}");
    assert_eq!(code, StatusCode::OK);
    let status: DaemonStatusResponse = serde_json::from_str(&body_text).unwrap();
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        status.village_root,
        server.tree.root().display().to_string()
    );
    assert_eq!(status.active_agents, 2);
    assert_eq!(status.commissions_count, 0);
}

#[tokio::test]
async fn test_commission_lifecycle_endpoints() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", server.token.as_str());

    // 1. Create commission 'beta'
    let new_url = format!("{}{}", server.base_url, ENDPOINT_COMMISSION_NEW);
    let payload = CommissionNewRequest {
        slug: name("beta"),
        brief: "Ship sprint 08".to_string(),
        seed_dir: None,
    };

    let res = client
        .post(&new_url)
        .header(AUTHORIZATION, &auth)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let created: CommissionNewResponse = res.json().await.unwrap();
    assert_eq!(created.slug, name("beta"));
    assert_eq!(created.commission_id, 1);

    // 2. List commissions
    let list_url = format!("{}{}", server.base_url, ENDPOINT_COMMISSIONS);
    let res = client
        .get(&list_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let items: Vec<CommissionSummaryView> = res.json().await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].slug, name("beta"));
    assert_eq!(items[0].state, "draft");

    // 3. Show commission detail
    let detail_url = format!("{}/api/v1/ledger/commissions/beta", server.base_url);
    let res = client
        .get(&detail_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let detail: CommissionDetailView = res.json().await.unwrap();
    assert_eq!(detail.summary.slug, name("beta"));
    assert!(detail.brief.as_ref().unwrap().contains("Ship sprint 08"));

    // 4. Close commission
    let close_url = format!("{}/api/v1/commissions/beta/close", server.base_url);
    let res = client
        .post(&close_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let closed: CommissionCloseResponse = res.json().await.unwrap();
    assert_eq!(closed.slug, name("beta"));
    assert_eq!(closed.state, "closed");
}

#[tokio::test]
async fn test_mail_send_and_list() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", server.token.as_str());

    // 1. Send mail to slot 1001
    let send_url = format!("{}{}", server.base_url, ENDPOINT_MAIL_SEND);
    let send_req = MailSendRequest {
        from: Some("mayor".to_string()),
        to: 1001,
        subject: "Commission Assignment".to_string(),
        body: "Your task is to inspect the guild ledger.".to_string(),
    };

    let res = client
        .post(&send_url)
        .header(AUTHORIZATION, &auth)
        .json(&send_req)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let send_resp: MailSendResponse = res.json().await.unwrap();
    assert!(send_resp.delivered);

    // 2. List mail for slot 1001
    let list_url = format!("{}/api/v1/mail/1001", server.base_url);
    let res = client
        .get(&list_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let messages: Vec<MailMessageSummary> = res.json().await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].to, 1001);
    assert_eq!(messages[0].from, "mayor");
    assert_eq!(messages[0].subject, "Commission Assignment");
    assert!(!messages[0].read);
}

#[tokio::test]
async fn test_vault_listing_and_export() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", server.token.as_str());

    // Seed vault directory with an artifact
    let vault_dir = server.tree.keep_vault().join("release-1");
    fs::create_dir_all(&vault_dir).unwrap();
    let artifact_path = vault_dir.join("binary.tar");
    fs::write(&artifact_path, b"mock-executable-content").unwrap();
    fs::write(
        vault_dir.join("binary.tar.sha256"),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n",
    )
    .unwrap();

    // 1. List vault
    let list_url = format!("{}{}", server.base_url, ENDPOINT_VAULT);
    let res = client
        .get(&list_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let items: Vec<VaultItemView> = res.json().await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "binary.tar");
    assert_eq!(items[0].commission, Some(name("release-1")));
    assert_eq!(items[0].size_bytes, 23);

    // 2. Export artifact
    let export_dir = server._tmp.path().join("exports");
    fs::create_dir_all(&export_dir).unwrap();
    let export_dest = export_dir.join("downloaded.bin");

    let export_url = format!("{}{}", server.base_url, ENDPOINT_VAULT_EXPORT);
    let export_req = VaultExportRequest {
        artifact: "binary.tar".to_string(),
        destination: export_dest.display().to_string(),
        commission: Some(name("release-1")),
    };

    let res = client
        .post(&export_url)
        .header(AUTHORIZATION, &auth)
        .json(&export_req)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let export_resp: VaultExportResponse = res.json().await.unwrap();
    assert_eq!(export_resp.bytes_written, 23);
    assert!(export_dest.exists());
    assert_eq!(fs::read(&export_dest).unwrap(), b"mock-executable-content");

    // 3. Path traversal export must be rejected with 400 Bad Request (QA08-01, SPEC §2, §11.3)
    let bad_export_req = VaultExportRequest {
        artifact: "../../charter.json".to_string(),
        destination: export_dir.join("exfiltrated.json").display().to_string(),
        commission: Some(name("release-1")),
    };
    let res_bad = client
        .post(&export_url)
        .header(AUTHORIZATION, &auth)
        .json(&bad_export_req)
        .send()
        .await
        .unwrap();
    assert_eq!(res_bad.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_decrees_list_and_detail() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", server.token.as_str());

    // Write a decree file in manor decrees
    let decrees_dir = server.tree.manor_decrees();
    fs::create_dir_all(&decrees_dir).unwrap();

    let decree_id = DecreeId::new(1, "c0225c3b8fabf543").unwrap();
    let decree_file = server.tree.manor_decree_file(&decree_id);
    let decree_content = r#"{"decree_id":"d-000000000001-c0225c3b8fabf543","action":"provision_agent","params":{"identity":"scout"}}"#;
    fs::write(&decree_file, decree_content).unwrap();

    // 1. List decrees
    let list_url = format!("{}{}", server.base_url, ENDPOINT_DECREES);
    let res = client
        .get(&list_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let decrees: Vec<DecreeSummaryView> = res.json().await.unwrap();
    assert_eq!(decrees.len(), 1);
    assert_eq!(decrees[0].decree_id, decree_id.to_string());
    assert_eq!(decrees[0].action, "provision_agent");

    // 2. Show decree detail
    let detail_url = format!("{}/api/v1/decrees/{}", server.base_url, decree_id);
    let res = client
        .get(&detail_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let detail: DecreeDetailView = res.json().await.unwrap();
    assert_eq!(detail.summary.decree_id, decree_id.to_string());
    assert_eq!(detail.raw_json, decree_content);
}

#[tokio::test]
async fn test_landmarks_and_escalations_endpoints() {
    let server = start_test_server().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", server.token.as_str());

    // 1. Landmarks view
    fs::create_dir_all(server.tree.parcel(1001)).unwrap();
    let landmarks_url = format!("{}{}", server.base_url, ENDPOINT_LANDMARKS);
    let res = client
        .get(&landmarks_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let landmarks: LandmarksView = res.json().await.unwrap();
    assert_eq!(landmarks.keep.census_parcels.len(), 1);
    assert_eq!(landmarks.keep.census_parcels[0], 1001);

    // 2. Escalations list (empty initially)
    let esc_url = format!("{}{}", server.base_url, ENDPOINT_ESCALATIONS);
    let res = client
        .get(&esc_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let escs: Vec<EscalationView> = res.json().await.unwrap();
    assert!(escs.is_empty());

    // Write a dummy escalation notice
    let decree_id = DecreeId::new(1, "0000000000000001").unwrap();
    let notice = hamlet_core::decree::EscalationNotice {
        escalation_version: hamlet_core::decree::ESCALATION_VERSION,
        decree_id: decree_id.clone(),
        evidence_refs: vec![hamlet_core::decree::TreeRelPath::parse("charter.json").unwrap()],
        recorded_unix_ms: 1729000000000,
        summary: "human review required".into(),
    };
    let notice_file = decree_id.decree_file_name().unwrap();
    let notice_path = server.tree.manor_escalations().join(notice_file.as_str());
    fs::create_dir_all(server.tree.manor_escalations()).unwrap();
    fs::write(&notice_path, notice.render().unwrap()).unwrap();

    // Query escalations again
    let res = client
        .get(&esc_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let escs: Vec<EscalationView> = res.json().await.unwrap();
    assert_eq!(escs.len(), 1);
    assert_eq!(escs[0].id, notice_file.as_str());
    assert_eq!(escs[0].summary, "human review required");
    assert!(!escs[0].resolved);

    // 3. Resolve escalation
    let resolve_url = format!(
        "{}/api/v1/escalations/{}/resolve",
        server.base_url,
        notice_file.as_str()
    );
    let res = client
        .post(&resolve_url)
        .header(AUTHORIZATION, &auth)
        .json(&EscalationResolveRequest {
            resolution: "proceed with caution".into(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let resolved: EscalationResolveResponse = res.json().await.unwrap();
    assert_eq!(resolved.id, notice_file.as_str());
    assert!(resolved.resolved);

    // 4. Resolving again should conflict (409)
    let res_conflict = client
        .post(&resolve_url)
        .header(AUTHORIZATION, &auth)
        .json(&EscalationResolveRequest {
            resolution: "again".into(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(res_conflict.status(), StatusCode::CONFLICT);

    // 5. Query escalations again -> should show resolved
    let res = client
        .get(&esc_url)
        .header(AUTHORIZATION, &auth)
        .send()
        .await
        .unwrap();
    let escs: Vec<EscalationView> = res.json().await.unwrap();
    assert_eq!(escs.len(), 1);
    assert!(escs[0].resolved);
    assert_eq!(escs[0].resolution.as_deref(), Some("proceed with caution"));
}

#[test]
fn test_daemon_discovery_file_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let token = LocalToken::new(TEST_TOKEN);
    let ep = DaemonEndpoint::new("127.0.0.1", 8765, std::process::id(), 1729000000000);

    // 1. Write files
    write_daemon_files(&tree, &ep, &token).unwrap();
    assert!(tree.daemon_token().exists());
    assert!(tree.daemon_pid().exists());
    assert!(tree.daemon_endpoint().exists());

    // 2. Verify token permissions (0600 on unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(tree.daemon_token())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "daemon.token must have 0600 permissions");
    }

    // 3. Verify endpoint contents
    let ep_content = fs::read_to_string(tree.daemon_endpoint()).unwrap();
    let decoded_ep: DaemonEndpoint = serde_json::from_str(&ep_content).unwrap();
    assert_eq!(decoded_ep, ep);

    // 4. Verify token contents
    let token_content = fs::read_to_string(tree.daemon_token()).unwrap();
    assert_eq!(token_content, TEST_TOKEN);

    // 5. Cleanup
    cleanup_daemon_files(&tree).unwrap();
    assert!(!tree.daemon_token().exists());
    assert!(!tree.daemon_pid().exists());
    assert!(!tree.daemon_endpoint().exists());
}
