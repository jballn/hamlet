//! Inbound webhook endpoints + axum server (SPEC §8.5, §12.3; ADR-0022).
//!
//! `POST /webhook/<topic>` accepts external events and turns them into
//! **tree artifacts only**: a dispatch inbox file (ADR-0022 cl. 4
//! envelope) and/or mail to the topic's configured recipients (cl. 5),
//! then fires the inert [`WebhookWake`] hook (Sprint 05 plugs the Mayor
//! wake loop in) and one sled trace. Handlers hold **no orchestration
//! logic** — validate → write files → emit hook; the tree remains the
//! truth and every downstream reaction flows through the existing
//! watcher/router planes.
//!
//! # Rejection classes (nothing touches disk on any of them)
//!
//! - 404 — webhooks disabled, topic fails the §4 sanitizer (traversal,
//!   foreign charset), or topic has no configured route (one
//!   indistinguishable class, ADR-0022 cl. 2);
//! - 401 — missing/mismatched `X-Hamlet-Auth` header (cl. 1);
//! - 503 — configured auth `.env` key missing/empty (fails **closed**);
//! - 413 — body over `webhooks.max_body_bytes` (tower-http `RequestBodyLimitLayer`
//!   plus an explicit re-check, cl. 2);
//! - 400 — body is not a JSON value.
//!
//! # Secrets (CONVENTIONS §6)
//!
//! The shared secret lives in `.env` under `webhooks.auth_env_key` and is
//! compared without early exit ([`secret_eq`]); it is re-read per request
//! (never cached across config/operator edits) and never appears in
//! responses, traces, or tree artifacts.
//!
//! WS metrics/chat streaming (SPEC §12.3) lands in Sprint 09 on this same
//! server surface.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use axum::body::Bytes;
use axum::extract::{Path as TopicPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use hamlet_core::dispatch::InboxResponse;
use hamlet_core::mail::MAX_BODY_BYTES;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{CorrelationId, SessionId};
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower_http::limit::RequestBodyLimitLayer;

use crate::dispatch::load_env_secrets;
use crate::mailroute::{MailActor, MailRouteError, MailRouter};
use crate::supervise::Supervisor;
use crate::telemetry::{AgentKey, TraceRecord};

/// Auth header carrying the shared secret (ADR-0022 cl. 1).
pub const AUTH_HEADER: &str = "x-hamlet-auth";
/// The daemon-asserted `From:` patron name of webhook mail (cl. 5).
pub const WEBHOOK_SENDER: &str = "webhook";
/// Inbox `status` marker of daemon-accepted inbound events (cl. 4).
pub const INBOX_ACCEPTED_STATUS: u16 = 202;

// ---------------------------------------------------------------------------
// Mayor-wake hook (registered, inert until Sprint 05)
// ---------------------------------------------------------------------------

/// One accepted webhook event on its way to the Mayor-wake seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEvent {
    /// Sanitized topic name.
    pub topic: String,
    /// End-to-end correlation of the written artifacts.
    pub correlation: CorrelationId,
}

/// The hook seam Sprint 05 plugs the Mayor wake loop into (mirrors
/// [`crate::freight::MayorWake`]): fire-and-forget, must not block the
/// handler on Mayor liveness.
pub trait WebhookWake: Send + Sync {
    /// Record/deliver one accepted inbound event.
    fn webhook(&self, event: &WebhookEvent);
}

/// Default sink until Sprint 05: one structured stderr line, nothing else.
#[derive(Debug, Default)]
pub struct StderrWebhookWake;

impl WebhookWake for StderrWebhookWake {
    fn webhook(&self, event: &WebhookEvent) {
        eprintln!(
            "hamlet-daemon: MAYOR-WAKE webhook topic={} correlation={}",
            event.topic, event.correlation
        );
    }
}

/// In-process recording sink — the test/inspection seam.
#[derive(Debug, Default)]
pub struct RecordingWebhookWake {
    events: Mutex<Vec<WebhookEvent>>,
}

impl RecordingWebhookWake {
    /// A fresh empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event recorded so far, in order.
    #[must_use]
    pub fn events(&self) -> Vec<WebhookEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl WebhookWake for RecordingWebhookWake {
    fn webhook(&self, event: &WebhookEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event.clone());
    }
}

// ---------------------------------------------------------------------------
// Accept/reject vocabulary
// ---------------------------------------------------------------------------

/// What one accepted request wrote (returned to the caller as 202 JSON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// End-to-end correlation of the written artifacts.
    pub correlation: CorrelationId,
    /// Sanitized topic.
    pub topic: SafeName,
    /// Whether a dispatch inbox file was written.
    pub inbox_written: bool,
    /// Recipients a mail spool file was written for (sorted).
    pub mailed_to: Vec<u32>,
}

/// Rejection classes (ADR-0022 — every one leaves the tree untouched).
#[derive(Debug)]
pub enum Rejection {
    /// `webhooks.enabled = false` (cl. 3).
    Disabled,
    /// Topic fails the §4 sanitizer (traversal/foreign charset).
    BadTopic,
    /// Topic has no configured route.
    UnknownTopic,
    /// Configured auth `.env` key missing/empty — fails closed (cl. 1).
    AuthUnavailable,
    /// Missing or mismatched `X-Hamlet-Auth` header.
    BadAuth,
    /// Body over `webhooks.max_body_bytes` (cl. 2).
    TooLarge,
    /// Body is not a JSON value.
    MalformedJson(String),
    /// Plane-infrastructure failure (I/O, telemetry, `.env`, mail write).
    Internal(String),
}

impl Rejection {
    /// The HTTP status this rejection answers with.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Disabled | Self::BadTopic | Self::UnknownTopic => StatusCode::NOT_FOUND,
            Self::BadAuth => StatusCode::UNAUTHORIZED,
            Self::AuthUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::MalformedJson(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Fixed per-class reason token for the JSON error body — never
    /// echoes the request (topic, body, or internals).
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Disabled | Self::BadTopic | Self::UnknownTopic => "unknown-topic",
            Self::BadAuth => "bad-auth",
            Self::AuthUnavailable => "auth-unavailable",
            Self::TooLarge => "payload-too-large",
            Self::MalformedJson(_) => "malformed-json",
            Self::Internal(_) => "internal-error",
        }
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedJson(msg) => write!(f, "{}: {msg}", self.reason()),
            Self::Internal(msg) => write!(f, "{}: {msg}", self.reason()),
            _ => f.write_str(self.reason()),
        }
    }
}

/// Server-level startup failures.
#[derive(Debug)]
pub enum WebError {
    /// `webhooks.enabled = false` — the server does not start (ADR-0022
    /// cl. 3).
    Disabled,
    /// Listen-address parse failure (config validation normally catches
    /// this first).
    Addr(String),
    /// Bind/serve I/O failure.
    Io(io::Error),
}

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "webhooks are disabled in config.json"),
            Self::Addr(addr) => write!(f, "bad web_listen_addr {addr:?}"),
            Self::Io(e) => write!(f, "web server I/O: {e}"),
        }
    }
}

impl std::error::Error for WebError {}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// The inbound-webhook service: validation, artifact writes, hook. One
/// instance per village; handlers are thin wrappers around [`Self::accept`]
/// (no orchestration logic lives in axum extractors).
pub struct WebhookService {
    supervisor: Arc<Supervisor>,
    router: Arc<MailRouter>,
    wake: Arc<dyn WebhookWake>,
    tmp_serial: AtomicU64,
}

impl WebhookService {
    /// Service over the supervisor's village (config + telemetry seam),
    /// the mail plane's router (mail sink), and the Mayor-wake hook.
    #[must_use]
    pub fn new(
        supervisor: Arc<Supervisor>,
        router: Arc<MailRouter>,
        wake: Arc<dyn WebhookWake>,
    ) -> Self {
        Self {
            supervisor,
            router,
            wake,
            tmp_serial: AtomicU64::new(0),
        }
    }

    /// The village tree this service writes into.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        self.supervisor.tree()
    }

    /// Body cap for the tower-http `RequestBodyLimitLayer`.
    #[must_use]
    pub fn max_body_bytes(&self) -> usize {
        self.supervisor.config().webhooks.max_body_bytes as usize
    }

    /// Validate one request and write its artifacts (ADR-0022). Pure
    /// validate → write → hook; every rejection class leaves the tree
    /// untouched.
    ///
    /// # Errors
    ///
    /// The [`Rejection`] class to answer with.
    pub fn accept(
        &self,
        topic: &str,
        auth: Option<&str>,
        body: &[u8],
    ) -> Result<Accepted, Rejection> {
        let webhooks = &self.supervisor.config().webhooks;
        if !webhooks.enabled {
            return Err(Rejection::Disabled);
        }
        // Sanitizer at the boundary (CONVENTIONS §2): the raw topic never
        // reaches a path, a subject, or a response body.
        let topic_name = sanitize_name(topic).map_err(|_| Rejection::BadTopic)?;
        let route = webhooks
            .routes
            .get(topic_name.as_str())
            .ok_or(Rejection::UnknownTopic)?;
        if u64::try_from(body.len()).unwrap_or(u64::MAX) > webhooks.max_body_bytes {
            return Err(Rejection::TooLarge);
        }
        // Auth: shared secret from .env, fail closed, no early exit.
        let secrets =
            load_env_secrets(self.tree()).map_err(|e| Rejection::Internal(e.to_string()))?;
        let Some(secret) = secrets.get(&webhooks.auth_env_key) else {
            return Err(Rejection::AuthUnavailable);
        };
        match auth {
            Some(presented) if secret_eq(presented, secret.expose()) => {}
            _ => return Err(Rejection::BadAuth),
        }
        // JSON validation + canonical re-render (serde_json Value maps are
        // BTreeMaps — sorted keys, CONVENTIONS §3.1).
        let event: Value =
            serde_json::from_slice(body).map_err(|e| Rejection::MalformedJson(e.to_string()))?;
        let canonical =
            serde_json::to_string(&event).map_err(|e| Rejection::Internal(e.to_string()))?;

        let telemetry = self.supervisor.telemetry();
        let correlation_key = AgentKey {
            commission: 0,
            uid: MAYOR_UID,
        };
        let seq = telemetry
            .next_correlation_seq(correlation_key)
            .map_err(|e| Rejection::Internal(e.to_string()))?;
        let session =
            SessionId::first(MAYOR_UID).map_err(|e| Rejection::Internal(e.to_string()))?;
        let correlation = CorrelationId::for_dispatch(&session, seq)
            .map_err(|e| Rejection::Internal(e.to_string()))?;

        // Sink 1: dispatch inbox file (ADR-0022 cl. 4 envelope).
        let mut inbox_written = false;
        if route.dispatch_inbox {
            self.write_inbox(&correlation, &canonical)
                .map_err(|e| Rejection::Internal(e.to_string()))?;
            inbox_written = true;
        }
        // Sink 2: mail to configured recipients (cl. 5).
        let mut mailed_to = Vec::new();
        if !route.mail_to.is_empty() {
            let actor = MailActor::Patron(
                sanitize_name(WEBHOOK_SENDER).expect("static sender name is sanitizer-clean"),
            );
            let subject = format!("webhook: {}", topic_name.as_str());
            let mail_body = mail_body(&canonical);
            for uid in &route.mail_to {
                self.router
                    .send(actor.clone(), *uid, &subject, &mail_body, None)
                    .map_err(|e: MailRouteError| Rejection::Internal(e.to_string()))?;
                mailed_to.push(*uid);
            }
        }

        // Trace (topic + correlation + sinks only — never body bytes).
        let record = TraceRecord {
            at_unix_ms: unix_now_ms(),
            event: "webhook-inbound".to_owned(),
            detail: format!(
                "topic={} correlation={} inbox={inbox_written} mail={mailed_to:?}",
                topic_name.as_str(),
                correlation
            ),
        };
        if let Err(e) = telemetry.trace_event(correlation_key, record) {
            eprintln!("hamlet-daemon: webhook trace failed: {e}");
        }
        // Mayor-wake hook (inert until Sprint 05).
        self.wake.webhook(&WebhookEvent {
            topic: topic_name.as_str().to_owned(),
            correlation: correlation.clone(),
        });
        Ok(Accepted {
            correlation,
            topic: topic_name,
            inbox_written,
            mailed_to,
        })
    }

    /// Atomic inbox write of the ADR-0022 cl. 4 envelope (through the
    /// ADR-0020 cl. 2 storage constructor: 64 KiB body cap + truncation
    /// marker apply): hidden temp → fsync → rename → dir fsync (ADR-0006
    /// discipline).
    fn write_inbox(&self, correlation: &CorrelationId, canonical: &str) -> Result<(), Rejection> {
        let response = InboxResponse::http(
            correlation.clone(),
            1,
            INBOX_ACCEPTED_STATUS,
            &BTreeMap::new(),
            canonical,
        );
        let rendered = response
            .render()
            .map_err(|e| Rejection::Internal(e.to_string()))?;
        let dir = self.tree().courier_dispatch_inbox();
        fs::create_dir_all(&dir).map_err(|e| Rejection::Internal(e.to_string()))?;
        let final_path = dir.join(hamlet_core::dispatch::file_name(correlation));
        let serial = self.tmp_serial.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".tmp-webhook-{}-{serial}", std::process::id()));
        let result = (|| -> io::Result<()> {
            {
                let mut out = fs::File::create(&tmp)?;
                io::Write::write_all(&mut out, rendered.as_bytes())?;
                out.sync_all()?;
            }
            fs::rename(&tmp, &final_path)?;
            if let Ok(dir_handle) = fs::File::open(&dir) {
                let _ = dir_handle.sync_all();
            }
            Ok(())
        })();
        result.map_err(|e| {
            let _ = fs::remove_file(&tmp);
            Rejection::Internal(e.to_string())
        })
    }
}

// ---------------------------------------------------------------------------
// axum surface
// ---------------------------------------------------------------------------

/// Constant-time-ish secret comparison (ADR-0022 cl. 1): no early exit on
/// mismatched bytes; the length fold is the only length-dependent branch
/// (unavoidable without a crypto crate — the locked stack has none).
#[must_use]
pub fn secret_eq(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    let mut acc = 0u8;
    for (a, b) in presented.iter().zip(expected) {
        acc |= a ^ b;
    }
    acc == 0
}

/// Mail body for the mail sink: canonical event JSON + LF, truncated at
/// the mail format's 64 KiB body cap (ADR-0022 cl. 5).
fn mail_body(canonical: &str) -> String {
    const MARKER: &str = "\n...[truncated]\n";
    if canonical.len() < MAX_BODY_BYTES {
        return format!("{canonical}\n");
    }
    let mut cut = MAX_BODY_BYTES.saturating_sub(MARKER.len());
    while cut > 0 && !canonical.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{MARKER}", &canonical[..cut])
}

async fn webhook_handler(
    State(service): State<Arc<WebhookService>>,
    TopicPath(topic): TopicPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let auth = headers
        .get(AUTH_HEADER)
        .and_then(|value| value.to_str().ok());
    match service.accept(&topic, auth, &body) {
        Ok(accepted) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "accepted": true,
                "correlation": accepted.correlation.to_string(),
                "topic": accepted.topic.as_str(),
            })),
        )
            .into_response(),
        Err(rejection) => {
            let status = rejection.status();
            let reason = json!({ "error": rejection.reason() });
            if matches!(rejection, Rejection::Internal(_)) {
                eprintln!("hamlet-daemon: webhook internal failure: {rejection}");
            }
            (status, Json(reason)).into_response()
        }
    }
}

/// The webhook router: `POST /webhook/<topic>` with the configured body
/// cap. Extracted so tests can drive handlers without a socket.
///
/// The cap is enforced at the hyper level by [`RequestBodyLimitLayer`]
/// (413 before any handler runs — axum's `DefaultBodyLimit` does not apply
/// to the raw `Bytes` extractor in 0.7, so we limit where it bites). The
/// service re-checks its size for direct invocations without a socket.
pub fn webhook_router(service: Arc<WebhookService>) -> Router {
    let limit = service.max_body_bytes().max(1);
    Router::new()
        .route("/webhook/:topic", post(webhook_handler))
        .layer(RequestBodyLimitLayer::new(limit))
        .with_state(service)
}

/// Handle of a running webhook server. [`WebServerHandle::stop`] shuts it
/// down gracefully; dropping the handle aborts best-effort.
#[derive(Debug)]
#[must_use = "dropping the handle stops the server"]
pub struct WebServerHandle {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl WebServerHandle {
    /// The bound listen address (differs from the configured one when the
    /// config asked for an ephemeral port — tests).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Graceful shutdown: finish in-flight requests, then stop.
    pub async fn stop(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for WebServerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Bind `web_listen_addr` and serve the webhook router.
///
/// # Errors
///
/// [`WebError::Disabled`] when `webhooks.enabled = false` (the server
/// does not start at all — ADR-0022 cl. 3), [`WebError::Addr`] /
/// [`WebError::Io`] on bind failures.
pub async fn spawn_web_server(service: Arc<WebhookService>) -> Result<WebServerHandle, WebError> {
    if !service.supervisor.config().webhooks.enabled {
        return Err(WebError::Disabled);
    }
    let addr = service.supervisor.config().web_listen_addr.clone();
    let listener = TcpListener::bind(&addr).await.map_err(WebError::Io)?;
    let bound = listener.local_addr().map_err(WebError::Io)?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let result = axum::serve(listener, webhook_router(service))
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;
        if let Err(e) = result {
            eprintln!("hamlet-daemon: web server stopped with error: {e}");
        }
    });
    Ok(WebServerHandle {
        addr: bound,
        shutdown: shutdown_tx,
        task: Some(task),
    })
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_eq_matches_without_early_exit() {
        assert!(secret_eq("test-key-do-not-use", "test-key-do-not-use"));
        assert!(!secret_eq("test-key-do-not-use", "test-key-do-not-ussf"));
        assert!(!secret_eq("short", "test-key-do-not-use"));
        assert!(!secret_eq("", "x"));
        assert!(secret_eq("", ""));
    }

    #[test]
    fn mail_body_fits_the_mail_cap() {
        let big = format!("{{\"x\":\"{}\"}}", "y".repeat(MAX_BODY_BYTES));
        let body = mail_body(&big);
        assert!(body.len() <= MAX_BODY_BYTES, "body {} over cap", body.len());
        assert!(body.ends_with("...[truncated]\n"));
        let small = "{\"a\":1}";
        assert_eq!(mail_body(small), "{\"a\":1}\n");
    }

    #[test]
    fn rejection_statuses_are_fixed() {
        assert_eq!(Rejection::BadTopic.status(), StatusCode::NOT_FOUND);
        assert_eq!(Rejection::UnknownTopic.status(), StatusCode::NOT_FOUND);
        assert_eq!(Rejection::Disabled.status(), StatusCode::NOT_FOUND);
        assert_eq!(Rejection::BadAuth.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            Rejection::AuthUnavailable.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(Rejection::TooLarge.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            Rejection::MalformedJson("x".into()).status(),
            StatusCode::BAD_REQUEST
        );
    }
}
