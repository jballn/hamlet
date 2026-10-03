//! Dispatch outbox/inbox integration tests (Sprint 04 step 04; SPEC §8.4,
//! ADR-0020 + ADR-0023 — CONVENTIONS §9 doctrine: tempdir tree, scripted
//! [`HttpClient`] seam in place of TLS, no podman).
//!
//! Real [`DispatchPlane`] against a tempdir village. Coverage: successful
//! round-trip (secret resolution, correlation end-to-end, inbox envelope),
//! permanent 4xx delivery, retry → success, transport-error retry,
//! dead-letter on exhaustion, the validation-rejection class (allowlist,
//! missing env key, malformed, oversize, http scheme — all zero network
//! attempts), restart checkpoint resume/exhaustion/stale-outbox recovery,
//! disabled-plane freeze, recovery scan, idempotent re-processing, and a
//! watcher-driven end-to-end plane.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hamlet_core::charter::{Charter, CommissionStatus};
use hamlet_core::config::Config;
use hamlet_core::dispatch::{
    file_name, InboxResponse, Method, OutboxRequest, Outcome, DISPATCH_VERSION,
};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::CorrelationId;
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;
use hamlet_daemon::dispatch::{
    spawn_dispatch_plane, DispatchOutcome, DispatchPlane, HttpClient, WireRequest, WireResponse,
};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::{AgentKey, Telemetry};
use hamlet_daemon::watch::{Dispatched, Dispatcher};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const COMMISSION: u64 = 7;
const SCRIBE: u32 = 1001;
const ALLOWED: &str = "https://hooks.test.invalid/v1/hamlet";
const AUTH_KEY: &str = "HAMLET_DISPATCH_AUTH";
const AUTH_VALUE: &str = "test-key-do-not-use";

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

fn corr(raw: &str) -> CorrelationId {
    CorrelationId::parse(raw).unwrap()
}

// ---------------------------------------------------------------------------
// Scripted HTTP seam
// ---------------------------------------------------------------------------

/// Scripted client: pops one canned result per call (exhausted script →
/// transport failure), records every request for assertions.
#[derive(Default)]
struct ScriptedClient {
    script: Mutex<VecDeque<Result<WireResponse, String>>>,
    calls: Mutex<Vec<WireRequest>>,
}

impl ScriptedClient {
    fn new(script: Vec<Result<WireResponse, String>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<WireRequest> {
        self.calls.lock().unwrap().clone()
    }
}

fn http(status: u16, body: &str) -> Result<WireResponse, String> {
    Ok(WireResponse {
        status,
        headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
        body: body.to_owned(),
    })
}

fn transport(token: &str) -> Result<WireResponse, String> {
    Err(token.to_owned())
}

impl HttpClient for ScriptedClient {
    fn execute(
        &self,
        request: WireRequest,
    ) -> Pin<Box<dyn Future<Output = Result<WireResponse, String>> + Send + '_>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(request.clone());
            let next = self.script.lock().unwrap().pop_front();
            match next {
                Some(result) => result,
                None => Err("script exhausted".to_owned()),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    _tmp: tempfile::TempDir,
    tree: HamletTree,
    telemetry: Arc<Telemetry>,
    supervisor: Arc<Supervisor>,
}

fn fixture(tweak: impl FnOnce(&mut Config)) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    charter.reserve_mayor(name("mayor")).unwrap();
    assert_eq!(
        charter
            .allocate_slot(name("scribe"), Some(COMMISSION))
            .unwrap(),
        SCRIBE
    );
    charter.save_atomic(&tree.charter()).unwrap();
    // .env: obvious fakes only (CONVENTIONS §6).
    std::fs::write(tree.env_file(), format!("{AUTH_KEY}={AUTH_VALUE}\n")).unwrap();

    let mut config = Config::default();
    config.dispatch.enabled = true;
    config.dispatch.url_allowlist = vec!["https://hooks.test.invalid/v1".to_owned()];
    config.dispatch.max_attempts = 3;
    config.dispatch.backoff_initial_ms = 100;
    config.dispatch.backoff_max_ms = 400;
    tweak(&mut config);
    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    Fixture {
        _tmp: tmp,
        tree,
        telemetry,
        supervisor,
    }
}

fn plane(fixture: &Fixture, client: Arc<dyn HttpClient>) -> Arc<DispatchPlane> {
    Arc::new(DispatchPlane::new(Arc::clone(&fixture.supervisor), client))
}

/// Write a conforming outbox file; returns its correlation.
fn write_outbox(
    tree: &HamletTree,
    correlation: &str,
    url: &str,
    headers_env: BTreeMap<String, String>,
) -> CorrelationId {
    let id = corr(correlation);
    let request = OutboxRequest {
        body: serde_json::json!({"event": "deed-updated", "n": 7}),
        correlation: id.clone(),
        dispatch_version: DISPATCH_VERSION,
        headers_env,
        method: Method::Post,
        url: url.to_owned(),
    };
    let path = tree.courier_dispatch_outbox().join(file_name(&id));
    std::fs::write(&path, request.render().unwrap()).unwrap();
    id
}

fn auth_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("X-Hamlet-Auth".to_owned(), AUTH_KEY.to_owned())])
}

fn read_inbox(tree: &HamletTree, id: &CorrelationId) -> InboxResponse {
    let path = tree.courier_dispatch_inbox().join(file_name(id));
    InboxResponse::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn outbox_exists(tree: &HamletTree, id: &CorrelationId) -> bool {
    tree.courier_dispatch_outbox().join(file_name(id)).exists()
}

fn dead_exists(tree: &HamletTree, id: &CorrelationId) -> bool {
    tree.courier_dispatch_dead().join(file_name(id)).exists()
}

fn traces(telemetry: &Telemetry, uid: u32) -> Vec<String> {
    telemetry
        .recent_traces(AgentKey { commission: 0, uid }, 100)
        .unwrap()
        .iter()
        .map(|record| record.event.clone())
        .collect()
}

async fn poll_until(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

/// Happy path: secret resolved from .env by key name, correlation
/// preserved end-to-end, inbox envelope written atomically, outbox
/// consumed, no secret in any artifact.
#[tokio::test]
async fn outbox_roundtrip_success() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![http(200, "{\"ok\":true}")]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(&fixture.tree, "d-1-1001.0", ALLOWED, auth_headers());

    let outcome = plane.process(&id).await.unwrap();
    assert_eq!(
        outcome,
        DispatchOutcome::Delivered {
            attempt: 1,
            status: 200
        }
    );

    // The wire request carried the resolved secret — by key name only.
    let calls = client.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].url, ALLOWED);
    assert_eq!(calls[0].method, Method::Post);
    assert_eq!(
        calls[0].headers.get("X-Hamlet-Auth").map(String::as_str),
        Some(AUTH_VALUE)
    );
    assert_eq!(calls[0].body, r#"{"event":"deed-updated","n":7}"#);

    // Inbox file: correlation echoed, attempt 1, body recorded.
    let inbox = read_inbox(&fixture.tree, &id);
    assert_eq!(inbox.correlation, id);
    assert_eq!(inbox.attempt, 1);
    match &inbox.outcome {
        Outcome::Http {
            status,
            body,
            headers,
        } => {
            assert_eq!(*status, 200);
            assert_eq!(body, "{\"ok\":true}");
            assert_eq!(
                headers.get("content-type").map(String::as_str),
                Some("application/json")
            );
        }
        Outcome::Error { error } => panic!("expected http outcome, got {error}"),
    }
    // Outbox consumed; nothing dead.
    assert!(!outbox_exists(&fixture.tree, &id));
    assert!(!dead_exists(&fixture.tree, &id));
    // Secret never appears in any tree artifact.
    for dir in [
        fixture.tree.courier_dispatch_inbox(),
        fixture.tree.courier_dispatch_dead(),
    ] {
        for entry in std::fs::read_dir(dir).unwrap() {
            let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            assert!(!text.contains(AUTH_VALUE), "secret leaked into {text}");
        }
    }
    // Sled trace per attempt + delivery.
    let events = traces(&fixture.telemetry, SCRIBE);
    assert_eq!(
        events.iter().filter(|e| *e == "dispatch-attempt").count(),
        1
    );
    assert!(events.contains(&"dispatch-delivered".to_owned()));
}

/// A permanent 4xx is a delivered response: inbox written, no retry, no
/// dead-letter (ADR-0020 cl. 4).
#[tokio::test]
async fn permanent_4xx_is_delivered_without_retry() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![http(404, "nope")]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(&fixture.tree, "d-2-1001.0", ALLOWED, auth_headers());

    let outcome = plane.process(&id).await.unwrap();
    assert_eq!(
        outcome,
        DispatchOutcome::Delivered {
            attempt: 1,
            status: 404
        }
    );
    assert_eq!(client.calls().len(), 1);
    assert!(!dead_exists(&fixture.tree, &id));
    assert!(!outbox_exists(&fixture.tree, &id));
}

/// Retryable statuses follow the deterministic schedule to success
/// (ADR-0020 cl. 4; ADR-0023 cl. 1 checkpoints each failure).
#[tokio::test]
async fn retry_then_success_records_final_attempt() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![
        http(500, "boom"),
        http(429, "slow"),
        http(201, "made"),
    ]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(&fixture.tree, "d-3-1001.0", ALLOWED, auth_headers());

    let outcome = plane.process(&id).await.unwrap();
    assert_eq!(
        outcome,
        DispatchOutcome::Delivered {
            attempt: 3,
            status: 201
        }
    );
    assert_eq!(client.calls().len(), 3);
    let inbox = read_inbox(&fixture.tree, &id);
    assert_eq!(inbox.attempt, 3);
    // One trace per attempt.
    let events = traces(&fixture.telemetry, SCRIBE);
    assert_eq!(
        events.iter().filter(|e| *e == "dispatch-attempt").count(),
        3
    );
}

/// Transport errors retry like 5xx (ADR-0020 cl. 4).
#[tokio::test]
async fn transport_error_retries() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![
        transport("transport connect failure"),
        http(200, "late ok"),
    ]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(&fixture.tree, "d-4-1001.0", ALLOWED, auth_headers());

    let outcome = plane.process(&id).await.unwrap();
    assert_eq!(
        outcome,
        DispatchOutcome::Delivered {
            attempt: 2,
            status: 200
        }
    );
    assert_eq!(client.calls().len(), 2);
}

/// Exhaustion: final error inbox + rename into dead/ (ADR-0020 cl. 5).
#[tokio::test]
async fn dead_letter_after_retry_exhaustion() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![http(503, "x"), http(503, "x"), http(503, "x")]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(&fixture.tree, "d-5-1001.0", ALLOWED, auth_headers());

    let outcome = plane.process(&id).await.unwrap();
    assert_eq!(
        outcome,
        DispatchOutcome::DeadLettered {
            attempt: 3,
            reason: "http 503".to_owned()
        }
    );
    assert_eq!(client.calls().len(), 3);
    assert!(!outbox_exists(&fixture.tree, &id));
    // dead/ holds the original request, byte-comparable.
    let dead =
        std::fs::read_to_string(fixture.tree.courier_dispatch_dead().join(file_name(&id))).unwrap();
    assert!(dead.contains(ALLOWED));
    let inbox = read_inbox(&fixture.tree, &id);
    assert_eq!(inbox.attempt, 3);
    assert!(matches!(inbox.outcome, Outcome::Error { .. }));
    let events = traces(&fixture.telemetry, SCRIBE);
    assert!(events.contains(&"dispatch-dead".to_owned()));
}

// ---------------------------------------------------------------------------
// Validation rejections (zero network attempts, immediate dead-letter)
// ---------------------------------------------------------------------------

async fn assert_validation_dead(fixture: &Fixture, client: &ScriptedClient, id: &CorrelationId) {
    assert!(
        client.calls().is_empty(),
        "validation must not touch the network"
    );
    assert!(!outbox_exists(&fixture.tree, id));
    assert!(dead_exists(&fixture.tree, id));
    let inbox = read_inbox(&fixture.tree, id);
    assert_eq!(inbox.attempt, 1);
    assert!(matches!(inbox.outcome, Outcome::Error { .. }));
}

#[tokio::test]
async fn validation_rejects_disallowed_url() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(
        &fixture.tree,
        "d-6-1001.0",
        "https://evil.test.invalid/steal",
        auth_headers(),
    );
    let outcome = plane.process(&id).await.unwrap();
    assert!(matches!(
        outcome,
        DispatchOutcome::DeadLettered { attempt: 1, .. }
    ));
    assert_validation_dead(&fixture, &client, &id).await;
}

#[tokio::test]
async fn validation_rejects_missing_env_key() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(
        &fixture.tree,
        "d-7-1001.0",
        ALLOWED,
        BTreeMap::from([("X-Token".to_owned(), "NONEXISTENT_KEY".to_owned())]),
    );
    let outcome = plane.process(&id).await.unwrap();
    match outcome {
        DispatchOutcome::DeadLettered { attempt: 1, reason } => {
            assert!(reason.contains("NONEXISTENT_KEY"), "reason was {reason}");
        }
        other => panic!("expected dead-letter, got {other:?}"),
    }
    assert_validation_dead(&fixture, &client, &id).await;
}

#[tokio::test]
async fn validation_rejects_malformed_schema() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = corr("d-8-1001.0");
    std::fs::write(
        fixture.tree.courier_dispatch_outbox().join(file_name(&id)),
        "{\"not\": \"an outbox request\"}",
    )
    .unwrap();
    let outcome = plane.process(&id).await.unwrap();
    assert!(matches!(
        outcome,
        DispatchOutcome::DeadLettered { attempt: 1, .. }
    ));
    assert_validation_dead(&fixture, &client, &id).await;
}

#[tokio::test]
async fn validation_rejects_oversize_file() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = corr("d-9-1001.0");
    let padding = "x".repeat(hamlet_core::dispatch::MAX_OUTBOX_BYTES + 1);
    std::fs::write(
        fixture.tree.courier_dispatch_outbox().join(file_name(&id)),
        padding,
    )
    .unwrap();
    let outcome = plane.process(&id).await.unwrap();
    assert!(matches!(
        outcome,
        DispatchOutcome::DeadLettered { attempt: 1, .. }
    ));
    assert_validation_dead(&fixture, &client, &id).await;
}

#[tokio::test]
async fn validation_rejects_http_scheme() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    // An http:// URL fails schema validation before the allowlist even
    // matters (ADR-0020 cl. 1: https only).
    let id = corr("d-10-1001.0");
    let request = OutboxRequest {
        body: serde_json::Value::Null,
        correlation: id.clone(),
        dispatch_version: DISPATCH_VERSION,
        headers_env: BTreeMap::new(),
        method: Method::Post,
        url: "http://hooks.test.invalid/v1/hamlet".to_owned(),
    };
    std::fs::write(
        fixture.tree.courier_dispatch_outbox().join(file_name(&id)),
        request.render().unwrap(),
    )
    .unwrap();
    let outcome = plane.process(&id).await.unwrap();
    assert!(matches!(
        outcome,
        DispatchOutcome::DeadLettered { attempt: 1, .. }
    ));
    assert_validation_dead(&fixture, &client, &id).await;
}

// ---------------------------------------------------------------------------
// Restart-resumability (ADR-0023 cl. 2–3)
// ---------------------------------------------------------------------------

/// A crash mid-backoff resumes at checkpoint attempt + 1 — the budget is
/// per file, not per process lifetime.
#[tokio::test]
async fn restart_resumes_from_error_checkpoint() {
    let fixture = fixture(|_| {});
    let id = write_outbox(&fixture.tree, "d-11-1001.0", ALLOWED, auth_headers());
    // Simulate the previous life: attempts 1–2 failed (checkpoint attempt 2).
    let checkpoint = InboxResponse::error(id.clone(), 2, "http 500");
    std::fs::write(
        fixture.tree.courier_dispatch_inbox().join(file_name(&id)),
        checkpoint.render().unwrap(),
    )
    .unwrap();

    let client = ScriptedClient::new(vec![http(200, "resumed")]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let outcome = plane.process(&id).await.unwrap();
    assert_eq!(
        outcome,
        DispatchOutcome::Delivered {
            attempt: 3,
            status: 200
        }
    );
    assert_eq!(client.calls().len(), 1);
    assert!(!outbox_exists(&fixture.tree, &id));
}

/// Checkpoint at max_attempts + surviving outbox (crash between checkpoint
/// and dead rename) completes the dead-letter with zero network attempts.
#[tokio::test]
async fn restart_completes_exhausted_dead_letter() {
    let fixture = fixture(|_| {});
    let id = write_outbox(&fixture.tree, "d-12-1001.0", ALLOWED, auth_headers());
    let checkpoint = InboxResponse::error(id.clone(), 3, "http 500");
    std::fs::write(
        fixture.tree.courier_dispatch_inbox().join(file_name(&id)),
        checkpoint.render().unwrap(),
    )
    .unwrap();

    let client = ScriptedClient::new(vec![]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let outcome = plane.process(&id).await.unwrap();
    assert_eq!(
        outcome,
        DispatchOutcome::DeadLettered {
            attempt: 3,
            reason: "http 500".to_owned()
        }
    );
    assert!(client.calls().is_empty());
    assert!(dead_exists(&fixture.tree, &id));
    assert!(!outbox_exists(&fixture.tree, &id));
}

/// Delivered checkpoint + stale outbox (crash between inbox write and
/// removal): recovery removes the outbox, zero sends (idempotent).
#[tokio::test]
async fn recovery_removes_stale_outbox_after_delivery() {
    let fixture = fixture(|_| {});
    let id = write_outbox(&fixture.tree, "d-13-1001.0", ALLOWED, auth_headers());
    let delivered = InboxResponse::http(id.clone(), 2, 200, &BTreeMap::new(), "done");
    std::fs::write(
        fixture.tree.courier_dispatch_inbox().join(file_name(&id)),
        delivered.render().unwrap(),
    )
    .unwrap();

    let client = ScriptedClient::new(vec![]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let outcome = plane.process(&id).await.unwrap();
    assert_eq!(outcome, DispatchOutcome::AlreadyDelivered);
    assert!(client.calls().is_empty());
    assert!(!outbox_exists(&fixture.tree, &id));
    // Processing again stays a cheap no-op (delivered checkpoint wins).
    assert_eq!(
        plane.process(&id).await.unwrap(),
        DispatchOutcome::AlreadyDelivered
    );
}

/// Idempotency: re-processing a delivered correlation never re-sends.
#[tokio::test]
async fn second_process_is_idempotent_noop() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![http(200, "once")]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(&fixture.tree, "d-14-1001.0", ALLOWED, auth_headers());
    plane.process(&id).await.unwrap();
    assert_eq!(
        plane.process(&id).await.unwrap(),
        DispatchOutcome::AlreadyDelivered
    );
    assert_eq!(client.calls().len(), 1);
}

/// `dispatch.enabled = false` freezes the plane: files stay pending
/// untouched (ADR-0023 cl. 6).
#[tokio::test]
async fn disabled_plane_leaves_outbox_pending() {
    let fixture = fixture(|c| c.dispatch.enabled = false);
    let client = ScriptedClient::new(vec![http(200, "never")]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(&fixture.tree, "d-15-1001.0", ALLOWED, auth_headers());

    assert_eq!(plane.process(&id).await.unwrap(), DispatchOutcome::Disabled);
    assert!(client.calls().is_empty());
    assert!(outbox_exists(&fixture.tree, &id));
    assert!(!fixture
        .tree
        .courier_dispatch_inbox()
        .join(file_name(&id))
        .exists());

    // The recovery scan reports them pending, untouched.
    let report = plane.recover_outbox().await.unwrap();
    assert_eq!(report.scanned, 1);
    assert_eq!(report.pending, 1);
    assert!(outbox_exists(&fixture.tree, &id));
}

/// Recovery scan: sorted, every conforming file processed, foreign names
/// skipped and never touched.
#[tokio::test]
async fn recover_outbox_scan_processes_all_pending() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![http(200, "a"), http(200, "b")]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let a = write_outbox(&fixture.tree, "d-16-1001.0", ALLOWED, auth_headers());
    let b = write_outbox(&fixture.tree, "d-17-1001.1", ALLOWED, auth_headers());
    std::fs::write(
        fixture.tree.courier_dispatch_outbox().join("notes.txt"),
        "operator shopping list",
    )
    .unwrap();

    let report = plane.recover_outbox().await.unwrap();
    assert_eq!(report.scanned, 2);
    assert_eq!(report.delivered, 2);
    assert_eq!(report.dead, 0);
    assert_eq!(report.skipped, 1);
    assert!(!outbox_exists(&fixture.tree, &a));
    assert!(!outbox_exists(&fixture.tree, &b));
    assert!(fixture
        .tree
        .courier_dispatch_outbox()
        .join("notes.txt")
        .exists());
}

/// Mixed recovery: one delivered, one dead-lettered, one skipped.
#[tokio::test]
async fn recover_outbox_report_mixes_outcomes() {
    let fixture = fixture(|_| {});
    let client = ScriptedClient::new(vec![
        http(200, "fine"),
        http(400, "bad"),
        http(400, "bad"),
        http(400, "bad"),
    ]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    write_outbox(&fixture.tree, "d-18-1001.0", ALLOWED, auth_headers());
    // 400 is permanent → delivered (1 call). Second file: not in allowlist
    // → dead with zero extra calls.
    write_outbox(
        &fixture.tree,
        "d-19-1001.1",
        "https://other.test.invalid/x",
        auth_headers(),
    );
    let report = plane.recover_outbox().await.unwrap();
    assert_eq!(report.scanned, 2);
    assert_eq!(report.delivered, 1);
    assert_eq!(report.dead, 1);
    assert_eq!(client.calls().len(), 1);
}

// ---------------------------------------------------------------------------
// Watcher-driven end-to-end
// ---------------------------------------------------------------------------

/// Full plane: watcher → reaction → worker → inbox file, and
/// `spawn_dispatch_plane`'s startup recovery runs before events.
#[tokio::test]
async fn watcher_driven_outbox_roundtrip() {
    let fixture = fixture(|_| {});
    // Pre-seed an outbox file from "downtime": the plane's recovery scan
    // must pick it up at spawn (before/independent of any event).
    let recovered_id = write_outbox(&fixture.tree, "d-20-1001.0", ALLOWED, auth_headers());
    let client = ScriptedClient::new(vec![http(200, "recovered"), http(200, "live")]);
    let dispatcher = Dispatcher::new();

    // The tree watcher (one per daemon) with an empty startup hook; the
    // dispatch plane registers its handler on the shared dispatcher.
    let _watcher = hamlet_daemon::watch::spawn_tree_watcher(
        fixture.tree.clone(),
        Arc::clone(&dispatcher),
        || {},
    )
    .unwrap();
    let (plane, _handle) = spawn_dispatch_plane(
        Arc::clone(&fixture.supervisor),
        Arc::clone(&client) as Arc<dyn HttpClient>,
        &dispatcher,
    )
    .await
    .unwrap();

    // Recovery already delivered the pre-seeded file.
    assert!(
        poll_until(Duration::from_secs(5), || !outbox_exists(
            &fixture.tree,
            &recovered_id
        ))
        .await
    );
    let inbox = read_inbox(&fixture.tree, &recovered_id);
    assert_eq!(inbox.attempt, 1);

    // Live event: drop a new outbox file, the watcher reaction drives it.
    let live_id = write_outbox(&fixture.tree, "d-21-1001.1", ALLOWED, auth_headers());
    let tree = fixture.tree.clone();
    assert!(
        poll_until(Duration::from_secs(10), || {
            tree.courier_dispatch_inbox()
                .join(file_name(&live_id))
                .exists()
                && !outbox_exists(&tree, &live_id)
        })
        .await
    );
    let inbox = read_inbox(&fixture.tree, &live_id);
    match inbox.outcome {
        Outcome::Http { status, .. } => assert_eq!(status, 200),
        Outcome::Error { error } => panic!("expected delivery, got {error}"),
    }
    assert_eq!(client.calls().len(), 2);
    assert!(plane.tree().courier_dispatch_inbox().exists());

    // A direct dispatcher push also works without the watcher (handlers
    // only ever see typed reactions).
    dispatcher
        .dispatch(Dispatched {
            seq: 999,
            reaction: Reaction::DispatchOutbox {
                correlation: corr("d-404-1001.9"),
            },
        })
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await; // no file → Missing no-op
    assert_eq!(client.calls().len(), 2);
}

/// The retry schedule is honored tick-by-tick: with initial 100 ms the gap
/// between attempt 1 and 2 is ≥ 100 ms (deterministic, jitter-free —
/// ADR-0020 cl. 4).
#[tokio::test]
async fn backoff_delays_are_deterministic() {
    let fixture = fixture(|c| {
        c.dispatch.max_attempts = 2;
        c.dispatch.backoff_initial_ms = 200;
        c.dispatch.backoff_max_ms = 200;
    });
    let client = ScriptedClient::new(vec![http(500, "x"), http(200, "ok")]);
    let plane = plane(&fixture, Arc::clone(&client) as Arc<dyn HttpClient>);
    let id = write_outbox(&fixture.tree, "d-22-1001.0", ALLOWED, auth_headers());

    let start = std::time::Instant::now();
    let outcome = plane.process(&id).await.unwrap();
    let elapsed = start.elapsed();
    assert_eq!(
        outcome,
        DispatchOutcome::Delivered {
            attempt: 2,
            status: 200
        }
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "backoff not honored: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "unexpected extra delay: {elapsed:?}"
    );
}
