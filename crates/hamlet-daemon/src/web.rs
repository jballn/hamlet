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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as TopicPath, RawQuery, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hamlet_core::api::*;
use hamlet_core::dispatch::InboxResponse;
use hamlet_core::mail::MAX_BODY_BYTES;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{CorrelationId, InterruptPriority, InterruptSource, SessionId};
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tower_http::limit::RequestBodyLimitLayer;

use crate::census::ProductionStasis;
use crate::dispatch::load_env_secrets;
use crate::ledger::Ledger;
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

// ---------------------------------------------------------------------------
// Static Asset Layers (SPEC §12.3; ADR-0044)
// ---------------------------------------------------------------------------

/// Configuration for serving `hamlet-web` distribution assets.
#[derive(Debug, Clone)]
pub struct AssetConfig {
    /// Filesystem directory holding the built web bundle.
    pub root_dir: PathBuf,
    /// Dev mode flag (serves files directly from disk on request).
    pub dev_mode: bool,
}

impl Default for AssetConfig {
    fn default() -> Self {
        let root = if let Ok(env_dir) = std::env::var("HAMLET_WEB_DIR") {
            PathBuf::from(env_dir)
        } else {
            let dev_path = PathBuf::from("crates/hamlet-web/dist");
            if dev_path.is_dir() {
                dev_path
            } else {
                let alt = PathBuf::from("../crates/hamlet-web/dist");
                if alt.is_dir() {
                    alt
                } else {
                    dev_path
                }
            }
        };
        Self {
            root_dir: root,
            dev_mode: true,
        }
    }
}

/// Static asset server for `hamlet-web` (HTML, JS glue, WASM, sprites, map).
#[derive(Debug, Clone)]
pub struct AssetServer {
    config: AssetConfig,
    version_hash: String,
}

impl AssetServer {
    /// Creates a new asset server from the specified configuration.
    #[must_use]
    pub fn new(config: AssetConfig) -> Self {
        let manifest_path = config.root_dir.join("manifest.json");
        let version_hash = if let Ok(s) = fs::read_to_string(&manifest_path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&s) {
                val.get("version_hash")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "dev-version-hash".to_string())
            } else {
                "dev-version-hash".to_string()
            }
        } else {
            "dev-version-hash".to_string()
        };

        Self {
            config,
            version_hash,
        }
    }

    /// Creates an asset server with a specific root directory.
    #[must_use]
    pub fn from_dir(root_dir: impl Into<PathBuf>) -> Self {
        Self::new(AssetConfig {
            root_dir: root_dir.into(),
            dev_mode: true,
        })
    }

    /// Returns the asset version hash.
    #[must_use]
    pub fn version_hash(&self) -> &str {
        &self.version_hash
    }

    /// Returns the root asset directory.
    #[must_use]
    pub fn root_dir(&self) -> &Path {
        &self.config.root_dir
    }

    /// Serves a file with appropriate MIME type, immutable/no-cache caching, and ETag.
    #[must_use]
    pub fn serve_file(
        &self,
        relative_path: &str,
        is_index: bool,
        if_none_match: Option<&str>,
    ) -> Response {
        let clean = relative_path.trim_start_matches('/');
        let path = Path::new(clean);

        // Security: directory traversal defense (reject '..' or root components)
        for comp in path.components() {
            match comp {
                std::path::Component::Normal(c) => {
                    let s = c.to_string_lossy();
                    if s.starts_with('.') && !is_index {
                        return (StatusCode::NOT_FOUND, "Not Found").into_response();
                    }
                }
                _ => return (StatusCode::FORBIDDEN, "Forbidden").into_response(),
            }
        }

        let full_path = self.config.root_dir.join(clean);
        if !full_path.exists() || !full_path.is_file() {
            return (StatusCode::NOT_FOUND, "Not Found").into_response();
        }

        let content = match fs::read(&full_path) {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Read error").into_response(),
        };

        let mime = mime_guess_from_path(&full_path);

        if is_index {
            // SPEC §12.3 & ADR-0044: index.html served with Cache-Control: no-cache
            let resp = Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, mime)
                .header(CACHE_CONTROL, "no-cache")
                .body(axum::body::Body::from(content));
            match resp {
                Ok(r) => r.into_response(),
                Err(_) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "Response builder error").into_response()
                }
            }
        } else {
            let etag_val = format!("\"{}\"", self.version_hash);
            // Check If-None-Match for 304 Not Modified
            if let Some(req_etag) = if_none_match {
                if req_etag == etag_val || req_etag == self.version_hash || req_etag == "*" {
                    let resp = Response::builder()
                        .status(StatusCode::NOT_MODIFIED)
                        .header(ETAG, etag_val)
                        .header(CACHE_CONTROL, "public, max-age=31536000, immutable")
                        .body(axum::body::Body::empty());
                    return match resp {
                        Ok(r) => r.into_response(),
                        Err(_) => (StatusCode::NOT_MODIFIED, "").into_response(),
                    };
                }
            }

            let resp = Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, mime)
                .header(CACHE_CONTROL, "public, max-age=31536000, immutable")
                .header(ETAG, etag_val)
                .body(axum::body::Body::from(content));
            match resp {
                Ok(r) => r.into_response(),
                Err(_) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "Response builder error").into_response()
                }
            }
        }
    }
}

fn mime_guess_from_path(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "wav" => "audio/wav",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Builds the axum router for static web assets.
pub fn asset_router(server: Arc<AssetServer>) -> Router {
    Router::new()
        .route("/", get(asset_index_handler))
        .route("/index.html", get(asset_index_handler))
        .route("/hamlet_web.wasm", get(asset_file_handler_wasm))
        .route("/hamlet.js", get(asset_file_handler_js))
        .route("/style.css", get(asset_file_handler_css))
        .route("/manifest.json", get(asset_file_handler_manifest))
        .route("/assets/*path", get(asset_subpath_handler))
        .with_state(server)
}

async fn asset_index_handler(State(server): State<Arc<AssetServer>>) -> Response {
    server.serve_file("index.html", true, None)
}

async fn asset_file_handler_wasm(
    State(server): State<Arc<AssetServer>>,
    headers: HeaderMap,
) -> Response {
    let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    server.serve_file("hamlet_web.wasm", false, if_none_match)
}

async fn asset_file_handler_js(
    State(server): State<Arc<AssetServer>>,
    headers: HeaderMap,
) -> Response {
    let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    server.serve_file("hamlet.js", false, if_none_match)
}

async fn asset_file_handler_css(
    State(server): State<Arc<AssetServer>>,
    headers: HeaderMap,
) -> Response {
    let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    server.serve_file("style.css", false, if_none_match)
}

async fn asset_file_handler_manifest(
    State(server): State<Arc<AssetServer>>,
    headers: HeaderMap,
) -> Response {
    let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    server.serve_file("manifest.json", false, if_none_match)
}

async fn asset_subpath_handler(
    State(server): State<Arc<AssetServer>>,
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    let if_none_match = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    let full = format!("assets/{path}");
    server.serve_file(&full, false, if_none_match)
}

// ---------------------------------------------------------------------------
// WebSocket Streaming Hub & Session Engine (SPEC §12.3; ADR-0044)
// ---------------------------------------------------------------------------

/// Broadcast frame envelope before connection-specific monotonic `seq` assignment.
#[derive(Debug, Clone)]
pub enum WsBroadcastFrame {
    /// State delta frame.
    StateDelta(Box<StateDeltaFrame>),
    /// Telemetry metrics batch frame.
    Metrics(MetricsBatchFrame),
    /// Chat or notification bubble.
    Bubble(BubbleFrame),
    /// Mutation command acknowledgment.
    CommandAck(CommandAckFrame),
    /// Resync notice due to restart or sequence desync.
    ResyncRequired {
        daemon_instance_id: String,
        reason: String,
    },
    /// Backward-compatible legacy event frame.
    LegacyEvent(WsEvent),
}

/// Accumulates parcel metric ticks and produces rate-limited batch frames.
#[derive(Debug, Default)]
pub struct MetricsBatcher {
    pending: Vec<ParcelMetricTick>,
}

impl MetricsBatcher {
    /// Creates a fresh metrics batcher.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one parcel metric tick.
    pub fn record(&mut self, tick: ParcelMetricTick) {
        self.pending.push(tick);
    }

    /// Records multiple parcel metric ticks.
    pub fn record_many(&mut self, ticks: impl IntoIterator<Item = ParcelMetricTick>) {
        self.pending.extend(ticks);
    }

    /// Flushes accumulated ticks into a batch frame if non-empty.
    #[must_use]
    pub fn flush(&mut self, now_ms: u64) -> Option<MetricsBatchFrame> {
        if self.pending.is_empty() {
            None
        } else {
            let ticks = std::mem::take(&mut self.pending);
            Some(MetricsBatchFrame::new(0, now_ms, ticks))
        }
    }

    /// Returns count of pending unbatched ticks.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// Central hub coordinating WebSocket connections, broadcasts, backpressure,
/// and mutation command execution.
#[derive(Clone)]
pub struct WsHub {
    pub daemon_instance_id: String,
    pub tree: Arc<HamletTree>,
    pub token: LocalToken,
    pub shutdown_tx: watch::Sender<bool>,
    pub supervisor: Option<Arc<Supervisor>>,
    pub mail_router: Option<Arc<MailRouter>>,
    pub stasis: Option<Arc<ProductionStasis>>,
    pub ledger: Option<Arc<Ledger>>,
    pub broadcast_tx: broadcast::Sender<WsBroadcastFrame>,
    pub metrics_batcher: Arc<Mutex<MetricsBatcher>>,
    pub dropped_metrics: Arc<AtomicU64>,
}

impl WsHub {
    /// Creates a fresh WebSocket hub.
    #[must_use]
    pub fn new(tree: Arc<HamletTree>, token: LocalToken, shutdown_tx: watch::Sender<bool>) -> Self {
        let (broadcast_tx, _) = broadcast::channel(512);
        let pid = std::process::id();
        let daemon_instance_id = format!("hamlet-{}-{}", pid, unix_now_ms());
        Self {
            daemon_instance_id,
            tree,
            token,
            shutdown_tx,
            supervisor: None,
            mail_router: None,
            stasis: None,
            ledger: None,
            broadcast_tx,
            metrics_batcher: Arc::new(Mutex::new(MetricsBatcher::new())),
            dropped_metrics: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Sets the supervisor.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: Arc<Supervisor>) -> Self {
        self.supervisor = Some(supervisor);
        self
    }

    /// Sets the mail router.
    #[must_use]
    pub fn with_mail_router(mut self, router: Arc<MailRouter>) -> Self {
        self.mail_router = Some(router);
        self
    }

    /// Sets the stasis governor.
    #[must_use]
    pub fn with_stasis(mut self, stasis: Arc<ProductionStasis>) -> Self {
        self.stasis = Some(stasis);
        self
    }

    /// Sets the ledger.
    #[must_use]
    pub fn with_ledger(mut self, ledger: Arc<Ledger>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Overrides the daemon instance id (used in restart tests).
    #[must_use]
    pub fn with_instance_id(mut self, id: impl Into<String>) -> Self {
        self.daemon_instance_id = id.into();
        self
    }

    /// Authenticates incoming HTTP headers or query parameter against `self.token`.
    #[must_use]
    pub fn authenticate(&self, headers: &HeaderMap, raw_query: Option<&str>) -> bool {
        // 1. Authorization: Bearer <token>
        if let Some(auth_val) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
            if let Some(token_str) = auth_val.strip_prefix("Bearer ") {
                if self.token.constant_time_eq(token_str.trim()) {
                    return true;
                }
            }
        }

        // 2. x-hamlet-local-token: <token>
        if let Some(token_val) = headers
            .get("x-hamlet-local-token")
            .and_then(|v| v.to_str().ok())
        {
            if self.token.constant_time_eq(token_val.trim()) {
                return true;
            }
        }

        // 3. ?token=<token>
        if let Some(query) = raw_query {
            for pair in query.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    if k == "token" && self.token.constant_time_eq(v.trim()) {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Produces a full state snapshot frame with all 7 projection slots.
    #[must_use]
    pub fn produce_snapshot(&self, seq: u64) -> StateDeltaFrame {
        let now = unix_now_ms();
        let telemetry = self.supervisor.as_ref().map(|s| s.telemetry().as_ref());
        let census = crate::local_api::project_census(&self.tree, telemetry);
        let commissions = crate::local_api::project_commissions(self.ledger.as_deref(), &self.tree);
        let decrees = crate::local_api::project_decrees(&self.tree);
        let vault = crate::local_api::project_vault(&self.tree);
        let mail_counts = crate::local_api::project_mail_counts(&self.tree);
        let landmarks = crate::local_api::project_landmarks(&self.tree);
        let escalations = crate::local_api::project_escalations(&self.tree);

        StateDeltaFrame::snapshot(
            &self.daemon_instance_id,
            seq,
            now,
            census,
            commissions,
            decrees,
            vault,
            mail_counts,
            landmarks,
            escalations,
        )
    }

    /// Dispatches and executes a typed mutation command.
    pub async fn dispatch_command(&self, seq: u64, cmd: WsCommandFrame) -> CommandAckFrame {
        let now = unix_now_ms();
        let correlation_id = cmd.correlation_id;
        let endpoint_path = cmd.action.endpoint_path();

        match cmd.action {
            WsCommandAction::CommissionNew(req) => {
                let ledger = self
                    .ledger
                    .clone()
                    .unwrap_or_else(|| Arc::new(Ledger::new(self.tree.as_ref().clone())));
                let seed_path = req.seed_dir.as_deref().map(Path::new);
                match ledger.create_commission(req.slug.as_str(), &req.brief, seed_path) {
                    Ok(created) => {
                        let resp = CommissionNewResponse {
                            commission_id: created.id,
                            slug: created.slug.clone(),
                            message: created.summary(),
                            state: created.state.kind_name().to_string(),
                            ledger_dir: format!("guild/ledger/commission{}", created.id),
                            git_dir: format!("guild/ledger/commission{}/git", created.id),
                            brief_file: format!(
                                "guild/ledger/commission{}/COMMISSION.md",
                                created.id
                            ),
                            seed_desc: match (created.seeded_files, created.seeded_commit) {
                                (Some(n), Some(oid)) => format!("{n} files ({oid})"),
                                _ => "none (empty bare repository)".to_string(),
                            },
                            mayor_wake: "commission_created".to_string(),
                        };
                        let bubble = BubbleFrame::agent_speech(
                            0,
                            now,
                            MAYOR_UID,
                            format!("Admitted commission: {}", resp.slug),
                        );
                        let _ = self.broadcast_tx.send(WsBroadcastFrame::Bubble(bubble));
                        let mut delta = StateDeltaFrame::diff(&self.daemon_instance_id, 0, now);
                        delta.commissions = Some(crate::local_api::project_commissions(
                            self.ledger.as_deref(),
                            &self.tree,
                        ));
                        let _ = self
                            .broadcast_tx
                            .send(WsBroadcastFrame::StateDelta(Box::new(delta)));

                        CommandAckFrame::success(
                            seq,
                            now,
                            correlation_id,
                            endpoint_path,
                            Some(serde_json::to_value(resp).unwrap_or_default()),
                        )
                    }
                    Err(e) => CommandAckFrame::error(
                        seq,
                        now,
                        correlation_id,
                        endpoint_path,
                        400,
                        format!("failed to create commission: {e}"),
                    ),
                }
            }
            WsCommandAction::CommissionClose { slug, .. } => {
                let ledger = self
                    .ledger
                    .clone()
                    .unwrap_or_else(|| Arc::new(Ledger::new(self.tree.as_ref().clone())));
                match ledger.close_commission(slug.as_str()) {
                    Ok(closed) => {
                        let resp = CommissionCloseResponse {
                            commission_id: closed.id,
                            slug: closed.slug.clone(),
                            state: closed.state.kind_name().to_string(),
                            parcels_stopped: closed.parcels_stopped,
                            escalation_filed: closed
                                .convergence
                                .as_ref()
                                .map(|c| !c.overall_success())
                                .unwrap_or(false),
                        };
                        let bubble = BubbleFrame::agent_speech(
                            0,
                            now,
                            MAYOR_UID,
                            format!("Commission {} closed", closed.slug),
                        );
                        let _ = self.broadcast_tx.send(WsBroadcastFrame::Bubble(bubble));
                        let mut delta = StateDeltaFrame::diff(&self.daemon_instance_id, 0, now);
                        delta.commissions = Some(crate::local_api::project_commissions(
                            self.ledger.as_deref(),
                            &self.tree,
                        ));
                        let _ = self
                            .broadcast_tx
                            .send(WsBroadcastFrame::StateDelta(Box::new(delta)));

                        CommandAckFrame::success(
                            seq,
                            now,
                            correlation_id,
                            endpoint_path,
                            Some(serde_json::to_value(resp).unwrap_or_default()),
                        )
                    }
                    Err(e) => CommandAckFrame::error(
                        seq,
                        now,
                        correlation_id,
                        endpoint_path,
                        400,
                        format!("failed to close commission: {e}"),
                    ),
                }
            }
            WsCommandAction::AgentHibernate { uid, request } => {
                if let Some(ref stasis) = self.stasis {
                    let trigger = request.trigger.as_deref().unwrap_or("operator:ws");
                    match crate::local_api::hibernate_agent(stasis, uid, trigger).await {
                        Ok(doc) => {
                            let resp = AgentHibernateResponse {
                                uid,
                                patron: doc.frontmatter.patron,
                                status: "hibernated".to_string(),
                            };
                            let telemetry =
                                self.supervisor.as_ref().map(|s| s.telemetry().as_ref());
                            let mut delta = StateDeltaFrame::diff(&self.daemon_instance_id, 0, now);
                            delta.census =
                                Some(crate::local_api::project_census(&self.tree, telemetry));
                            let _ = self
                                .broadcast_tx
                                .send(WsBroadcastFrame::StateDelta(Box::new(delta)));

                            CommandAckFrame::success(
                                seq,
                                now,
                                correlation_id,
                                endpoint_path,
                                Some(serde_json::to_value(resp).unwrap_or_default()),
                            )
                        }
                        Err(e) => CommandAckFrame::error(
                            seq,
                            now,
                            correlation_id,
                            endpoint_path,
                            500,
                            format!("hibernation failed: {e}"),
                        ),
                    }
                } else {
                    CommandAckFrame::error(
                        seq,
                        now,
                        correlation_id,
                        endpoint_path,
                        503,
                        "stasis governor not attached",
                    )
                }
            }
            WsCommandAction::AgentWake(request) => {
                let commission_id = if let Some(ref slug) = request.commission {
                    match hamlet_core::charter::Charter::load(&self.tree.charter()) {
                        Ok(charter) => charter
                            .commissions
                            .iter()
                            .find(|(_, rec)| rec.slug == *slug)
                            .map(|(&id, _)| id),
                        Err(e) => {
                            return CommandAckFrame::error(
                                seq,
                                now,
                                correlation_id,
                                endpoint_path,
                                500,
                                format!("load charter: {e}"),
                            );
                        }
                    }
                } else {
                    None
                };

                if let Some(ref stasis) = self.stasis {
                    match stasis.awaken(&request.patron, commission_id).await {
                        Ok(agent) => {
                            let resp = AgentWakeResponse {
                                allocated_uid: agent.uid,
                                identity: agent.identity,
                                commission: request.commission,
                            };
                            let telemetry =
                                self.supervisor.as_ref().map(|s| s.telemetry().as_ref());
                            let mut delta = StateDeltaFrame::diff(&self.daemon_instance_id, 0, now);
                            delta.census =
                                Some(crate::local_api::project_census(&self.tree, telemetry));
                            let _ = self
                                .broadcast_tx
                                .send(WsBroadcastFrame::StateDelta(Box::new(delta)));

                            CommandAckFrame::success(
                                seq,
                                now,
                                correlation_id,
                                endpoint_path,
                                Some(serde_json::to_value(resp).unwrap_or_default()),
                            )
                        }
                        Err(e) => CommandAckFrame::error(
                            seq,
                            now,
                            correlation_id,
                            endpoint_path,
                            500,
                            format!("wake failed: {e}"),
                        ),
                    }
                } else {
                    CommandAckFrame::error(
                        seq,
                        now,
                        correlation_id,
                        endpoint_path,
                        503,
                        "stasis governor not attached",
                    )
                }
            }
            WsCommandAction::AgentSteer { uid, request } => {
                if let Some(ref supervisor) = self.supervisor {
                    match supervisor
                        .steer(
                            uid,
                            InterruptSource::Human,
                            InterruptPriority::Normal,
                            &request.directive,
                        )
                        .await
                    {
                        Ok(corr) => {
                            let resp = AgentSteerResponse {
                                uid,
                                injected: true,
                                correlation: corr.to_string(),
                            };
                            let bubble = BubbleFrame::steer(0, now, uid, &request.directive);
                            let _ = self.broadcast_tx.send(WsBroadcastFrame::Bubble(bubble));

                            CommandAckFrame::success(
                                seq,
                                now,
                                correlation_id,
                                endpoint_path,
                                Some(serde_json::to_value(resp).unwrap_or_default()),
                            )
                        }
                        Err(e) => CommandAckFrame::error(
                            seq,
                            now,
                            correlation_id,
                            endpoint_path,
                            500,
                            format!("steer injection failed: {e}"),
                        ),
                    }
                } else {
                    CommandAckFrame::error(
                        seq,
                        now,
                        correlation_id,
                        endpoint_path,
                        503,
                        "supervisor not attached",
                    )
                }
            }
            WsCommandAction::MailSend(request) => {
                let sender = request.from.as_deref().unwrap_or("operator");
                let actor = match sanitize_name(sender) {
                    Ok(n) => MailActor::Patron(n),
                    Err(e) => {
                        return CommandAckFrame::error(
                            seq,
                            now,
                            correlation_id,
                            endpoint_path,
                            400,
                            format!("invalid sender name: {e}"),
                        );
                    }
                };
                if let Some(ref router) = self.mail_router {
                    match router.send(actor, request.to, &request.subject, &request.body, None) {
                        Ok(path) => {
                            let resp = MailSendResponse {
                                delivered: true,
                                spool_file: path.as_path().display().to_string(),
                            };
                            // HONESTY RULE bubble: only subject and snippet delivered to spectators!
                            let from_uid = 1000;
                            let bubble = BubbleFrame::mail_snippet(
                                0,
                                now,
                                from_uid,
                                request.to,
                                &request.subject,
                                &request.body,
                            );
                            let _ = self.broadcast_tx.send(WsBroadcastFrame::Bubble(bubble));
                            let mut delta = StateDeltaFrame::diff(&self.daemon_instance_id, 0, now);
                            delta.mail_counts =
                                Some(crate::local_api::project_mail_counts(&self.tree));
                            let _ = self
                                .broadcast_tx
                                .send(WsBroadcastFrame::StateDelta(Box::new(delta)));

                            CommandAckFrame::success(
                                seq,
                                now,
                                correlation_id,
                                endpoint_path,
                                Some(serde_json::to_value(resp).unwrap_or_default()),
                            )
                        }
                        Err(e) => CommandAckFrame::error(
                            seq,
                            now,
                            correlation_id,
                            endpoint_path,
                            500,
                            format!("failed to send mail: {e}"),
                        ),
                    }
                } else {
                    CommandAckFrame::error(
                        seq,
                        now,
                        correlation_id,
                        endpoint_path,
                        503,
                        "mail router not attached",
                    )
                }
            }
            WsCommandAction::VaultExport(request) => {
                match crate::local_api::export_vault_artifact(&self.tree, &request) {
                    Ok(resp) => CommandAckFrame::success(
                        seq,
                        now,
                        correlation_id,
                        endpoint_path,
                        Some(serde_json::to_value(resp).unwrap_or_default()),
                    ),
                    Err((code, msg)) => CommandAckFrame::error(
                        seq,
                        now,
                        correlation_id,
                        endpoint_path,
                        code.as_u16(),
                        msg,
                    ),
                }
            }
            WsCommandAction::EscalationResolve { id, request } => {
                match crate::local_api::resolve_escalation(
                    &self.tree,
                    self.mail_router.as_deref(),
                    id.as_str(),
                    &request.resolution,
                ) {
                    Ok(resp) => {
                        let bubble = BubbleFrame::agent_speech(
                            0,
                            now,
                            MAYOR_UID,
                            format!(
                                "Resolved escalation {}: {}",
                                resp.id,
                                request.resolution.trim()
                            ),
                        );
                        let _ = self.broadcast_tx.send(WsBroadcastFrame::Bubble(bubble));
                        let mut delta = StateDeltaFrame::diff(&self.daemon_instance_id, 0, now);
                        delta.escalations = Some(crate::local_api::project_escalations(&self.tree));
                        let _ = self
                            .broadcast_tx
                            .send(WsBroadcastFrame::StateDelta(Box::new(delta)));

                        CommandAckFrame::success(
                            seq,
                            now,
                            correlation_id,
                            endpoint_path,
                            Some(serde_json::to_value(resp).unwrap_or_default()),
                        )
                    }
                    Err((code, msg)) => CommandAckFrame::error(
                        seq,
                        now,
                        correlation_id,
                        endpoint_path,
                        code.as_u16(),
                        msg,
                    ),
                }
            }
            WsCommandAction::DaemonStop => {
                let _ = self.shutdown_tx.send(true);
                let resp = DaemonStopResponse {
                    stopping: true,
                    pid: std::process::id(),
                };
                CommandAckFrame::success(
                    seq,
                    now,
                    correlation_id,
                    endpoint_path,
                    Some(serde_json::to_value(resp).unwrap_or_default()),
                )
            }
        }
    }

    /// Handles an inotify/filesystem reaction by broadcasting appropriate deltas or bubbles.
    pub fn handle_reaction(&self, reaction: &hamlet_core::watch::Reaction) {
        let now = unix_now_ms();
        match reaction {
            hamlet_core::watch::Reaction::MailSpool { recipient, file } => {
                let recipient_uid = recipient.as_str().parse::<u32>().unwrap_or(0);
                let spool_path = self
                    .tree
                    .courier_mail_recipient(recipient)
                    .join(file.as_str());
                let (from_uid, subject, snippet) =
                    if let Ok(content) = fs::read_to_string(&spool_path) {
                        let mut subj = "(no subject)".to_string();
                        let mut from_str = "1000".to_string();
                        let mut body = String::new();
                        let mut in_body = false;
                        for line in content.lines() {
                            if in_body {
                                body.push_str(line);
                                body.push(' ');
                            } else if let Some(s) = line.strip_prefix("Subject: ") {
                                subj = s.trim().to_string();
                            } else if let Some(f) = line.strip_prefix("From: ") {
                                from_str = f.trim().to_string();
                            } else if line.is_empty() {
                                in_body = true;
                            }
                        }
                        let from_uid = from_str.parse::<u32>().unwrap_or(1000);
                        (from_uid, subj, body)
                    } else {
                        (1000, "new mail".to_string(), String::new())
                    };

                // Honesty rule: mail body is suppressed by BubbleFrame::mail_snippet
                let bubble =
                    BubbleFrame::mail_snippet(0, now, from_uid, recipient_uid, subject, snippet);
                let _ = self.broadcast_tx.send(WsBroadcastFrame::Bubble(bubble));

                let mut delta = StateDeltaFrame::diff(&self.daemon_instance_id, 0, now);
                delta.mail_counts = Some(crate::local_api::project_mail_counts(&self.tree));
                let _ = self
                    .broadcast_tx
                    .send(WsBroadcastFrame::StateDelta(Box::new(delta)));
            }
            hamlet_core::watch::Reaction::DecreeWrite { path } => {
                let decree_id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                let bubble = BubbleFrame::decree_ack(
                    0,
                    now,
                    decree_id,
                    true,
                    format!("Decree: {decree_id}"),
                );
                let _ = self.broadcast_tx.send(WsBroadcastFrame::Bubble(bubble));

                let mut delta = StateDeltaFrame::diff(&self.daemon_instance_id, 0, now);
                delta.decrees = Some(crate::local_api::project_decrees(&self.tree));
                let _ = self
                    .broadcast_tx
                    .send(WsBroadcastFrame::StateDelta(Box::new(delta)));
            }
            hamlet_core::watch::Reaction::DeedMutation { uid } => {
                let bubble = BubbleFrame::agent_speech(0, now, *uid, "DEED modified");
                let _ = self.broadcast_tx.send(WsBroadcastFrame::Bubble(bubble));

                let telemetry = self.supervisor.as_ref().map(|s| s.telemetry().as_ref());
                let mut delta = StateDeltaFrame::diff(&self.daemon_instance_id, 0, now);
                delta.census = Some(crate::local_api::project_census(&self.tree, telemetry));
                delta.landmarks = Some(crate::local_api::project_landmarks(&self.tree));
                let _ = self
                    .broadcast_tx
                    .send(WsBroadcastFrame::StateDelta(Box::new(delta)));
            }
            _ => {}
        }
    }

    /// Broadcasts an envelope to all connected clients.
    pub fn broadcast_frame(&self, frame: WsBroadcastFrame) {
        let _ = self.broadcast_tx.send(frame);
    }

    /// Broadcasts a state delta frame.
    pub fn broadcast_delta(&self, delta: StateDeltaFrame) {
        let _ = self
            .broadcast_tx
            .send(WsBroadcastFrame::StateDelta(Box::new(delta)));
    }

    /// Broadcasts a chat bubble frame (with honesty rule enforced).
    pub fn broadcast_bubble(&self, bubble: BubbleFrame) {
        let _ = self.broadcast_tx.send(WsBroadcastFrame::Bubble(bubble));
    }

    /// Broadcasts a backward-compatible legacy event frame.
    pub fn broadcast_event(&self, event: WsEvent) {
        let _ = self.broadcast_tx.send(WsBroadcastFrame::LegacyEvent(event));
    }

    /// Records one parcel metric tick into the batcher.
    pub fn record_metric_tick(&self, tick: ParcelMetricTick) {
        let mut batcher = self.metrics_batcher.lock().unwrap();
        batcher.record(tick);
    }

    /// Records multiple parcel metric ticks into the batcher.
    pub fn record_metric_ticks(&self, ticks: impl IntoIterator<Item = ParcelMetricTick>) {
        let mut batcher = self.metrics_batcher.lock().unwrap();
        batcher.record_many(ticks);
    }

    /// Flushes accumulated metrics to all connected clients.
    pub fn flush_metrics(&self) -> bool {
        let now = unix_now_ms();
        let frame = {
            let mut batcher = self.metrics_batcher.lock().unwrap();
            batcher.flush(now)
        };
        if let Some(frame) = frame {
            let _ = self.broadcast_tx.send(WsBroadcastFrame::Metrics(frame));
            true
        } else {
            false
        }
    }

    /// Returns the number of metrics frames dropped due to backpressure.
    #[must_use]
    pub fn dropped_metrics_count(&self) -> u64 {
        self.dropped_metrics.load(Ordering::Relaxed)
    }

    /// Handles a connected WebSocket connection.
    pub async fn handle_socket(&self, mut socket: WebSocket) {
        let conn_seq = Arc::new(AtomicU64::new(0));
        let next_seq = |seq: &AtomicU64| seq.fetch_add(1, Ordering::SeqCst) + 1;

        // Bounded per-connection queue (64 items)
        let (send_tx, mut send_rx) = mpsc::channel::<WsServerFrame>(64);

        // Frame 1: initial full snapshot
        let init_seq = next_seq(&conn_seq);
        let snapshot = self.produce_snapshot(init_seq);
        if send_tx
            .send(WsServerFrame::StateDelta(Box::new(snapshot)))
            .await
            .is_err()
        {
            return;
        }

        let mut bcast_rx = self.broadcast_tx.subscribe();
        let conn_seq_clone = Arc::clone(&conn_seq);
        let dropped_metrics = Arc::clone(&self.dropped_metrics);
        let send_tx_for_bcast = send_tx.clone();
        let daemon_id_for_bcast = self.daemon_instance_id.clone();

        // Forwarder task
        let bcast_forwarder = tokio::spawn(async move {
            loop {
                let msg = match bcast_rx.recv().await {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let seq = next_seq(&conn_seq_clone);
                        let _ = send_tx_for_bcast.try_send(WsServerFrame::ResyncRequired {
                            seq,
                            timestamp_unix_ms: unix_now_ms(),
                            daemon_instance_id: daemon_id_for_bcast.clone(),
                            reason: format!("broadcast buffer lagged: {skipped} frames missed"),
                        });
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let seq = next_seq(&conn_seq_clone);
                let frame = match msg {
                    WsBroadcastFrame::StateDelta(mut d) => {
                        d.seq = seq;
                        WsServerFrame::StateDelta(d)
                    }
                    WsBroadcastFrame::Metrics(mut m) => {
                        m.seq = seq;
                        WsServerFrame::Metrics(m)
                    }
                    WsBroadcastFrame::Bubble(mut b) => {
                        b.seq = seq;
                        WsServerFrame::Bubble(b)
                    }
                    WsBroadcastFrame::CommandAck(mut a) => {
                        a.seq = seq;
                        WsServerFrame::CommandAck(a)
                    }
                    WsBroadcastFrame::ResyncRequired {
                        daemon_instance_id,
                        reason,
                    } => WsServerFrame::ResyncRequired {
                        seq,
                        timestamp_unix_ms: unix_now_ms(),
                        daemon_instance_id,
                        reason,
                    },
                    WsBroadcastFrame::LegacyEvent(e) => WsServerFrame::Event(e),
                };

                match &frame {
                    WsServerFrame::Metrics(_) => {
                        // Backpressure drop policy: drop metrics tick if channel is full
                        if let Err(mpsc::error::TrySendError::Full(_)) =
                            send_tx_for_bcast.try_send(frame)
                        {
                            dropped_metrics.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    _ => {
                        // Backpressure record policy: never drop state deltas / acks / bubbles!
                        if tokio::time::timeout(
                            Duration::from_secs(5),
                            send_tx_for_bcast.send(frame),
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        // Main event loop
        loop {
            tokio::select! {
                Some(frame) = send_rx.recv() => {
                    let text = match serde_json::to_string(&frame) {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    if socket.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                msg = socket.recv() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<WsClientFrame>(&text) {
                                Ok(client_frame) => match client_frame {
                                    WsClientFrame::Ping { timestamp_unix_ms } => {
                                        let pong = WsServerFrame::Pong { timestamp_unix_ms };
                                        let _ = send_tx.send(pong).await;
                                    }
                                    WsClientFrame::Pong { .. } => {}
                                    WsClientFrame::Resync {
                                        last_seen_seq: _,
                                        daemon_instance_id,
                                    } => {
                                        let now = unix_now_ms();
                                        if daemon_instance_id.as_deref()
                                            != Some(&self.daemon_instance_id)
                                        {
                                            let resync = WsServerFrame::ResyncRequired {
                                                seq: next_seq(&conn_seq),
                                                timestamp_unix_ms: now,
                                                daemon_instance_id: self.daemon_instance_id.clone(),
                                                reason: "daemon restart / instance mismatch"
                                                    .to_string(),
                                            };
                                            let _ = send_tx.send(resync).await;
                                        }
                                        let snap = self.produce_snapshot(next_seq(&conn_seq));
                                        let _ = send_tx
                                            .send(WsServerFrame::StateDelta(Box::new(snap)))
                                            .await;
                                    }
                                    WsClientFrame::Command(cmd) => {
                                        let cmd_seq = next_seq(&conn_seq);
                                        let ack = self.dispatch_command(cmd_seq, cmd).await;
                                        let _ = send_tx.send(WsServerFrame::CommandAck(ack)).await;
                                    }
                                },
                                Err(err) => {
                                    if let Ok(val) =
                                        serde_json::from_str::<serde_json::Value>(&text)
                                    {
                                        if val.get("type").and_then(|t| t.as_str())
                                            == Some("command")
                                        {
                                            if let Some(correlation_id) = val
                                                .get("correlation_id")
                                                .and_then(|c| c.as_str())
                                            {
                                                let ack = CommandAckFrame::error(
                                                    next_seq(&conn_seq),
                                                    unix_now_ms(),
                                                    correlation_id.to_string(),
                                                    "/api/v1/command",
                                                    400,
                                                    format!("malformed command frame: {err}"),
                                                );
                                                let _ = send_tx
                                                    .send(WsServerFrame::CommandAck(ack))
                                                    .await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        #[allow(clippy::collapsible_match)]
                        Some(Ok(Message::Ping(data))) => {
                            if socket.send(Message::Pong(data)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        bcast_forwarder.abort();
    }
}

/// Builds the axum router for the WebSocket endpoint.
pub fn ws_router(hub: Arc<WsHub>) -> Router {
    Router::new()
        .route(ENDPOINT_WS_EVENTS, get(ws_events_handler))
        .with_state(hub)
}

async fn ws_events_handler(
    State(hub): State<Arc<WsHub>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    if !hub.authenticate(&headers, raw_query.as_deref()) {
        let err =
            hamlet_core::api::ApiErrorResponse::unauthorized("invalid or missing local auth token");
        return (StatusCode::UNAUTHORIZED, Json(err)).into_response();
    }

    ws.on_upgrade(move |socket| async move {
        hub.handle_socket(socket).await;
    })
}

/// Builds a unified router for static assets and WebSocket streams.
pub fn frontend_router(hub: Arc<WsHub>, assets: Arc<AssetServer>) -> Router {
    let assets_r = asset_router(assets);
    let ws_r = ws_router(hub);
    assets_r.merge(ws_r)
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
