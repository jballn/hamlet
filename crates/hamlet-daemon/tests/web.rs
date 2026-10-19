//! Inbound webhook integration tests (Sprint 04 step 04; SPEC §8.5,
//! ADR-0022 — CONVENTIONS §9: tempdir tree, ephemeral-bind axum server,
//! reqwest over plain HTTP from the test client).
//!
//! Coverage:
//!
//! - accepted event → dispatch inbox envelope + mail spool writes + wake
//!   hook + sled trace;
//! - every rejection class (bad/missing auth, missing env key, oversize,
//!   malformed JSON, unknown topic, traversal-shaped topics) leaves the
//!   tree untouched;
//! - disabled config never binds; correlation ids are monotonic across a
//!   server restart.

use std::collections::BTreeMap;
use std::sync::Arc;

use hamlet_core::charter::Charter;
use hamlet_core::commission::CommissionRecord;
use hamlet_core::config::{Config, WebhookRoute};
use hamlet_core::dispatch::{InboxResponse, Outcome};
use hamlet_core::mail::is_incoming;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::mailroute::MailRouter;
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::{AgentKey, Telemetry};
use hamlet_daemon::web::{
    spawn_web_server, RecordingWebhookWake, WebError, WebServerHandle, WebhookService, AUTH_HEADER,
};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const COMMISSION: u64 = 7;
const SCRIBE: u32 = 1001;
const HERALD: u32 = 1002;
const WEBHOOK_SECRET: &str = "test-key-do-not-use";

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    tree: HamletTree,
    telemetry: Arc<Telemetry>,
    supervisor: Arc<Supervisor>,
    wake: Arc<RecordingWebhookWake>,
    service: Arc<WebhookService>,
}

fn routes() -> BTreeMap<String, WebhookRoute> {
    BTreeMap::from([
        (
            "deploy".to_owned(),
            WebhookRoute {
                dispatch_inbox: true,
                mail_to: vec![SCRIBE],
            },
        ),
        (
            "audit".to_owned(),
            WebhookRoute {
                dispatch_inbox: false,
                mail_to: vec![SCRIBE, HERALD],
            },
        ),
        (
            "inbox-only".to_owned(),
            WebhookRoute {
                dispatch_inbox: true,
                mail_to: vec![],
            },
        ),
    ])
}

fn fixture(tweak: impl FnOnce(&mut Config)) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionRecord::draft(name("commission")));
    charter.reserve_mayor(name("mayor")).unwrap();
    assert_eq!(
        charter
            .allocate_slot(name("scribe"), Some(COMMISSION))
            .unwrap(),
        SCRIBE
    );
    assert_eq!(
        charter
            .allocate_slot(name("herald"), Some(COMMISSION))
            .unwrap(),
        HERALD
    );
    charter.save_atomic(&tree.charter()).unwrap();
    std::fs::write(
        tree.env_file(),
        format!("HAMLET_WEBHOOK_AUTH={WEBHOOK_SECRET}\n"),
    )
    .unwrap();

    let mut config = Config {
        web_listen_addr: "127.0.0.1:0".to_owned(),
        ..Default::default()
    };
    config.webhooks.enabled = true;
    config.webhooks.max_body_bytes = 4096;
    config.webhooks.routes = routes();
    tweak(&mut config);
    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    let router = Arc::new(MailRouter::new(Arc::clone(&supervisor)));
    let wake = Arc::new(RecordingWebhookWake::new());
    let service = Arc::new(WebhookService::new(
        Arc::clone(&supervisor),
        router,
        Arc::clone(&wake) as Arc<dyn hamlet_daemon::web::WebhookWake>,
    ));
    Fixture {
        _tmp: tmp,
        tree,
        telemetry,
        supervisor,
        wake,
        service,
    }
}

async fn serve(fixture: &Fixture) -> (WebServerHandle, String) {
    let handle = spawn_web_server(Arc::clone(&fixture.service))
        .await
        .expect("server binds");
    let base = format!("http://{}", handle.local_addr());
    (handle, base)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn post(base: &str, topic: &str, auth: Option<&str>, body: &[u8]) -> reqwest::Response {
    let mut request = client()
        .post(format!("{base}/webhook/{topic}"))
        .header("content-type", "application/json");
    if let Some(secret) = auth {
        request = request.header(AUTH_HEADER, secret);
    }
    request.body(body.to_vec()).send().await.unwrap()
}

/// Every visible file in the village tree, sorted — rejection tests assert
/// this is unchanged (nothing touches disk).
fn tree_listing(tree: &HamletTree) -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if path.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut listing = Vec::new();
    walk(tree.root(), tree.root(), &mut listing);
    listing.sort();
    listing
}

fn inbox_files(tree: &HamletTree) -> Vec<String> {
    let mut names = std::fs::read_dir(tree.courier_dispatch_inbox())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn spool_files(tree: &HamletTree, uid: u32) -> Vec<String> {
    let dir = tree.courier_mail_recipient(&name(&uid.to_string()));
    let mut names = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| is_incoming(n))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Accepted events
// ---------------------------------------------------------------------------

/// Happy path: 202 + inbox envelope (status 202 marker, canonical body,
/// correlation `d-<seq>-1000.0`) + mail spool file + wake hook + trace.
#[tokio::test]
async fn accepted_event_writes_inbox_and_mail() {
    let fixture = fixture(|_| {});
    let (server, base) = serve(&fixture).await;

    let response = post(
        &base,
        "deploy",
        Some(WEBHOOK_SECRET),
        br#"{"event":"ship","n":3}"#,
    )
    .await;
    assert_eq!(response.status(), 202);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["accepted"], true);
    let correlation = body["correlation"].as_str().unwrap().to_owned();
    assert_eq!(correlation, "d-1-1000.0");
    assert_eq!(body["topic"], "deploy");

    // Inbox envelope per ADR-0022 cl. 4.
    let inbox_names = inbox_files(&fixture.tree);
    assert_eq!(inbox_names, vec![format!("{correlation}.json")]);
    let inbox = InboxResponse::parse(
        &std::fs::read_to_string(fixture.tree.courier_dispatch_inbox().join(&inbox_names[0]))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(inbox.attempt, 1);
    assert_eq!(inbox.correlation.as_str(), correlation);
    match &inbox.outcome {
        Outcome::Http {
            status,
            headers,
            body,
        } => {
            assert_eq!(*status, 202); // daemon-accepted inbound marker
            assert!(headers.is_empty());
            assert_eq!(body, r#"{"event":"ship","n":3}"#); // canonical JSON
        }
        Outcome::Error { error } => panic!("expected http outcome, got {error}"),
    }

    // Mail sink: spool file for the scribe, From: webhook, subject topic.
    let spool = spool_files(&fixture.tree, SCRIBE);
    assert_eq!(spool.len(), 1);
    let mail = std::fs::read_to_string(
        fixture
            .tree
            .courier_mail_recipient(&name("1001"))
            .join(&spool[0]),
    )
    .unwrap();
    assert!(mail.starts_with("From: webhook\n"), "mail was:\n{mail}");
    assert!(mail.contains("To: 1001\n"));
    assert!(mail.contains("Subject: webhook: deploy\n"));
    assert!(mail.contains(r#"{"event":"ship","n":3}"#));
    assert!(spool_files(&fixture.tree, HERALD).is_empty());

    // Wake hook fired (inert until Sprint 05) with the correlation.
    let events = fixture.wake.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].topic, "deploy");
    assert_eq!(events[0].correlation.as_str(), correlation);

    // Sled trace under the mayor key, no body bytes in the detail.
    let traces = fixture
        .telemetry
        .recent_traces(
            AgentKey {
                commission: 0,
                uid: 1000,
            },
            50,
        )
        .unwrap();
    assert!(traces.iter().any(|t| t.event == "webhook-inbound"
        && t.detail.contains("topic=deploy")
        && t.detail.contains(&correlation)
        && !t.detail.contains("ship")));

    server.stop().await;
}

/// Canonical key order: the inbox body re-renders the event with sorted
/// keys (CONVENTIONS §3.1) regardless of request order.
#[tokio::test]
async fn inbox_body_is_canonical_sorted_json() {
    let fixture = fixture(|_| {});
    let (server, base) = serve(&fixture).await;
    let response = post(
        &base,
        "inbox-only",
        Some(WEBHOOK_SECRET),
        br#"{"b":1,"a":{"z":2,"y":3}}"#,
    )
    .await;
    assert_eq!(response.status(), 202);
    let names = inbox_files(&fixture.tree);
    assert_eq!(names.len(), 1);
    let inbox = InboxResponse::parse(
        &std::fs::read_to_string(fixture.tree.courier_dispatch_inbox().join(&names[0])).unwrap(),
    )
    .unwrap();
    match inbox.outcome {
        Outcome::Http { body, .. } => assert_eq!(body, r#"{"a":{"y":3,"z":2},"b":1}"#),
        Outcome::Error { error } => panic!("expected http outcome, got {error}"),
    }
    // Mail-less route wrote no spool files.
    assert!(spool_files(&fixture.tree, SCRIBE).is_empty());
    server.stop().await;
}

/// Mail-only route: two recipients, no inbox file.
#[tokio::test]
async fn mail_only_route_skips_inbox() {
    let fixture = fixture(|_| {});
    let (server, base) = serve(&fixture).await;
    let before = inbox_files(&fixture.tree);
    let response = post(&base, "audit", Some(WEBHOOK_SECRET), br#"{"finding":1}"#).await;
    assert_eq!(response.status(), 202);
    assert_eq!(inbox_files(&fixture.tree), before);
    assert_eq!(spool_files(&fixture.tree, SCRIBE).len(), 1);
    assert_eq!(spool_files(&fixture.tree, HERALD).len(), 1);
    server.stop().await;
}

// ---------------------------------------------------------------------------
// Rejection classes — the tree stays untouched
// ---------------------------------------------------------------------------

async fn assert_rejected(
    fixture: &Fixture,
    base: &str,
    topic: &str,
    auth: Option<&str>,
    body: &[u8],
    expected: u16,
) {
    let before = tree_listing(&fixture.tree);
    let wakes_before = fixture.wake.events().len();
    let response = post(base, topic, auth, body).await;
    assert_eq!(response.status().as_u16(), expected, "topic {topic:?}");
    // Our rejection classes carry a JSON error body; hyper-level rejects
    // (route-mismatch 404, `RequestBodyLimitLayer` 413 "length limit
    // exceeded") answer with an empty or plain-text body — either way the
    // status class is right and nothing hit disk.
    let text = response.text().await.unwrap();
    if !text.is_empty() {
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(json) => assert!(json["error"].is_string()),
            Err(_) => assert_eq!(text, "length limit exceeded", "topic {topic:?}"),
        }
    }
    assert_eq!(
        tree_listing(&fixture.tree),
        before,
        "rejection of {topic:?} mutated the tree"
    );
    assert_eq!(fixture.wake.events().len(), wakes_before);
}

#[tokio::test]
async fn bad_auth_is_rejected() {
    let fixture = fixture(|_| {});
    let (server, base) = serve(&fixture).await;
    assert_rejected(
        &fixture,
        &base,
        "deploy",
        Some("wrong-secret"),
        br#"{"a":1}"#,
        401,
    )
    .await;
    // Missing header entirely.
    assert_rejected(&fixture, &base, "deploy", None, br#"{"a":1}"#, 401).await;
    // Empty secret.
    assert_rejected(&fixture, &base, "deploy", Some(""), br#"{"a":1}"#, 401).await;
    server.stop().await;
}

#[tokio::test]
async fn missing_env_key_fails_closed() {
    // No HAMLET_WEBHOOK_AUTH in .env → every request 503s, never open.
    let fixture = fixture(|_| {});
    std::fs::write(fixture.tree.env_file(), "# empty on purpose\n").unwrap();
    let (server, base) = serve(&fixture).await;
    assert_rejected(
        &fixture,
        &base,
        "deploy",
        Some(WEBHOOK_SECRET),
        br#"{"a":1}"#,
        503,
    )
    .await;
    server.stop().await;
}

#[tokio::test]
async fn oversize_body_is_rejected() {
    let fixture = fixture(|c| c.webhooks.max_body_bytes = 64);
    let (server, base) = serve(&fixture).await;
    let big = vec![b'x'; 256];
    assert_rejected(&fixture, &base, "deploy", Some(WEBHOOK_SECRET), &big, 413).await;
    server.stop().await;
}

#[tokio::test]
async fn malformed_json_is_rejected() {
    let fixture = fixture(|_| {});
    let (server, base) = serve(&fixture).await;
    assert_rejected(
        &fixture,
        &base,
        "deploy",
        Some(WEBHOOK_SECRET),
        b"{not json",
        400,
    )
    .await;
    assert_rejected(&fixture, &base, "deploy", Some(WEBHOOK_SECRET), b"", 400).await;
    server.stop().await;
}

#[tokio::test]
async fn unknown_topic_is_rejected() {
    let fixture = fixture(|_| {});
    let (server, base) = serve(&fixture).await;
    assert_rejected(
        &fixture,
        &base,
        "not-configured",
        Some(WEBHOOK_SECRET),
        br#"{"a":1}"#,
        404,
    )
    .await;
    server.stop().await;
}

/// Traversal-shaped and sanitizer-failing topics: one indistinguishable
/// 404 class, zero disk effect (ADR-0022 cl. 2; CONVENTIONS §2 boundary
/// sanitizer).
#[tokio::test]
async fn traversal_topics_are_rejected_without_disk_effect() {
    let fixture = fixture(|_| {});
    let (server, base) = serve(&fixture).await;
    for topic in [
        "..",
        ".",
        "%2e%2e",
        "%2e%2e%2fetc",
        "..%2f..%2fetc",
        "a%2fb",
        "-leading-dash",
        "spaced%20topic",
        "nul%00byte",
    ] {
        assert_rejected(
            &fixture,
            &base,
            topic,
            Some(WEBHOOK_SECRET),
            br#"{"a":1}"#,
            404,
        )
        .await;
    }
    server.stop().await;
}

/// Disabled webhooks: the server never binds (ADR-0022 cl. 3), and even a
/// directly-invoked service rejects with the 404 class.
#[tokio::test]
async fn disabled_webhooks_never_bind() {
    let fixture = fixture(|c| c.webhooks.enabled = false);
    let error = spawn_web_server(Arc::clone(&fixture.service))
        .await
        .unwrap_err();
    assert!(matches!(error, WebError::Disabled));
    let rejection = fixture
        .service
        .accept("deploy", Some(WEBHOOK_SECRET), br#"{"a":1}"#)
        .unwrap_err();
    assert_eq!(rejection.status(), axum::http::StatusCode::NOT_FOUND);
    assert!(inbox_files(&fixture.tree).is_empty());
}

// ---------------------------------------------------------------------------
// Restart behavior
// ---------------------------------------------------------------------------

/// Correlation seqs come from the durable sled counter: monotonic across a
/// server restart, never reused (no wall-clock input, CONVENTIONS §3.4).
#[tokio::test]
async fn correlations_are_monotonic_across_restart() {
    let fixture = fixture(|_| {});
    let (server, base) = serve(&fixture).await;
    for expected in ["d-1-1000.0", "d-2-1000.0"] {
        let response = post(&base, "inbox-only", Some(WEBHOOK_SECRET), br#"{"a":1}"#).await;
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["correlation"], expected);
    }
    server.stop().await;

    // "Restart": release every holder of the sled lock, then rebuild a
    // fresh supervisor/telemetry/service over the same tree and data dir —
    // the counter continues where it left off.
    let tmp_path = fixture._tmp.path().to_owned();
    let tree = fixture.tree.clone();
    let config = fixture.supervisor.config().clone();
    drop(fixture.service);
    drop(fixture.wake);
    drop(fixture.supervisor);
    drop(fixture.telemetry);

    let telemetry = Arc::new(Telemetry::open(tmp_path.join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    let router = Arc::new(MailRouter::new(Arc::clone(&supervisor)));
    let service = Arc::new(WebhookService::new(
        supervisor,
        router,
        Arc::new(RecordingWebhookWake::new()) as Arc<dyn hamlet_daemon::web::WebhookWake>,
    ));
    let server = spawn_web_server(service).await.unwrap();
    let base = format!("http://{}", server.local_addr());
    let response = post(&base, "inbox-only", Some(WEBHOOK_SECRET), br#"{"a":2}"#).await;
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["correlation"], "d-3-1000.0");
    assert_eq!(inbox_files(&tree).len(), 3);
    server.stop().await;
}

/// Oversized *accepted* events are truncated for the mail sink at the
/// 64 KiB mail cap (ADR-0022 cl. 5) while the inbox keeps the full
/// canonical body up to the body cap.
#[tokio::test]
async fn large_event_mail_is_truncated_inbox_is_full() {
    let fixture = fixture(|c| {
        c.webhooks.max_body_bytes = 256 * 1024;
        c.webhooks.routes = BTreeMap::from([(
            "big".to_owned(),
            WebhookRoute {
                dispatch_inbox: true,
                mail_to: vec![SCRIBE],
            },
        )]);
    });
    let (server, base) = serve(&fixture).await;
    let payload = format!("{{\"blob\":\"{}\"}}", "y".repeat(100 * 1024));
    let response = post(&base, "big", Some(WEBHOOK_SECRET), payload.as_bytes()).await;
    assert_eq!(response.status(), 202);

    let names = inbox_files(&fixture.tree);
    assert_eq!(names.len(), 1);
    let inbox = InboxResponse::parse(
        &std::fs::read_to_string(fixture.tree.courier_dispatch_inbox().join(&names[0])).unwrap(),
    )
    .unwrap();
    match inbox.outcome {
        Outcome::Http { body, .. } => {
            // The ADR-0020 cl. 2 inbox cap applies: truncated with marker.
            assert!(body.len() <= hamlet_core::dispatch::MAX_INBOX_BODY_BYTES);
            assert!(body.ends_with(hamlet_core::dispatch::TRUNCATION_MARKER));
        }
        Outcome::Error { error } => panic!("expected http outcome, got {error}"),
    }
    let spool = spool_files(&fixture.tree, SCRIBE);
    assert_eq!(spool.len(), 1);
    let mail = std::fs::read_to_string(
        fixture
            .tree
            .courier_mail_recipient(&name("1001"))
            .join(&spool[0]),
    )
    .unwrap();
    assert!(mail.contains("...[truncated]"));
    assert!(mail.len() <= hamlet_core::mail::MAX_MESSAGE_BYTES);
    server.stop().await;
}
