//! External webhook dispatch — the outbox/inbox plane (SPEC §8.4;
//! protocol ADR-0020, runtime contract ADR-0023).
//!
//! Consumes [`Reaction::DispatchOutbox`] from the [`crate::watch`]
//! dispatcher: validates the outbox file against the ADR-0020 schema,
//! checks the URL against the config allowlist, resolves `headers_env`
//! secrets from `.env` **by key name at send time** (never cached, never
//! inline), POSTs via the injected [`HttpClient`] (reqwest + rustls in
//! production), and records every outcome as an atomic inbox file.
//! Retryable failures checkpoint through the inbox file (ADR-0023 cl. 1),
//! so a daemon killed mid-backoff resumes at `attempt + 1` with the same
//! deterministic schedule; exhausted or invalid requests are renamed into
//! `courier/dispatch/dead/` (tree-visible, operator-recoverable).
//!
//! # File-as-truth
//!
//! All state is derivable from `courier/dispatch/` alone: outbox file
//! present + no checkpoint = unsent; error checkpoint = mid-retry (resume
//! at `attempt + 1`); http checkpoint = delivered (the outbox file is
//! removed — the inbox *is* the record); `dead/` entry = terminal. The
//! in-flight dedupe set is process-local telemetry of nothing: across
//! restarts the tree dedupes (ADR-0023 cl. 2–3).
//!
//! # Secret hygiene (CONVENTIONS §6)
//!
//! `.env` values travel as [`Secret`] and are exposed only into the wire
//! request headers. Traces, inbox files and dead-letter artifacts carry
//! correlation ids, attempt numbers, method and URL *authority* only —
//! never headers, bodies, query strings, or secret values.
//!
//! # Concurrency
//!
//! One coordinator task fans `DispatchOutbox` reactions out to one worker
//! task per correlation (deduped), so a backoff sleep never blocks other
//! files; each worker owns its file end-to-end (ADR-0023 cl. 5).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fmt, fs, io};

use hamlet_core::config::Secret;
use hamlet_core::dispatch::{
    file_name, is_retryable_status, InboxResponse, Method, OutboxRequest, Outcome, UrlAllowlist,
    MAX_INBOX_BODY_BYTES, MAX_OUTBOX_BYTES,
};
use hamlet_core::session::CorrelationId;
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;
use tokio::task::JoinHandle;

use crate::supervise::Supervisor;
use crate::telemetry::{AgentKey, TelemetryError, TraceRecord};
use crate::watch::Dispatcher;

/// Boxed-future alias for the dyn-compatible [`HttpClient`] seam (the
/// locked stack has no `async-trait`; RPITIT is not object-safe).
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// HTTP seam
// ---------------------------------------------------------------------------

/// One fully-resolved outbound HTTP request (secrets already substituted;
/// this type never touches disk or telemetry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRequest {
    /// HTTP method.
    pub method: Method,
    /// Allowlisted `https://` URL.
    pub url: String,
    /// Resolved header name → value.
    pub headers: BTreeMap<String, String>,
    /// Canonical JSON body text.
    pub body: String,
}

/// One inbound HTTP response, pre-capped for inbox storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireResponse {
    /// HTTP status code.
    pub status: u16,
    /// Lowercased response headers.
    pub headers: BTreeMap<String, String>,
    /// Body text, truncated at [`MAX_INBOX_BODY_BYTES`].
    pub body: String,
}

/// The mock seam for dispatch sends (CONVENTIONS §9): production uses
/// [`ReqwestClient`]; tests script attempt sequences without TLS. The
/// `Err(String)` is an **opaque ASCII transport-error token** — it lands
/// in inbox error outcomes, so implementations must never include URLs,
/// headers, or secret material in it.
pub trait HttpClient: Send + Sync {
    /// Execute one request.
    fn execute(&self, request: WireRequest) -> BoxFuture<'_, Result<WireResponse, String>>;
}

/// Production client: reqwest over rustls-tls (SPEC §3 locked stack),
/// per-request timeout from `dispatch.timeout_secs`.
pub struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    /// Build the shared client.
    ///
    /// # Errors
    ///
    /// reqwest builder failures (invalid timeout/TLS backend).
    pub fn new(timeout: Duration) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { client })
    }
}

impl HttpClient for ReqwestClient {
    fn execute(&self, request: WireRequest) -> BoxFuture<'_, Result<WireResponse, String>> {
        Box::pin(async move {
            let method = match request.method {
                Method::Get => reqwest::Method::GET,
                Method::Post => reqwest::Method::POST,
                Method::Put => reqwest::Method::PUT,
                Method::Patch => reqwest::Method::PATCH,
                Method::Delete => reqwest::Method::DELETE,
            };
            let mut builder = self.client.request(method, &request.url);
            let mut has_content_type = false;
            for (name, value) in &request.headers {
                if name.eq_ignore_ascii_case("content-type") {
                    has_content_type = true;
                }
                builder = builder.header(name.as_str(), value.as_str());
            }
            if !has_content_type {
                builder = builder.header(reqwest::header::CONTENT_TYPE, "application/json");
            }
            let response = builder
                .body(request.body)
                .send()
                .await
                .map_err(|e| transport_token(&e))?;
            let status = response.status().as_u16();
            let mut headers = BTreeMap::new();
            for (name, value) in response.headers() {
                if let Ok(text) = value.to_str() {
                    headers.insert(name.as_str().to_ascii_lowercase(), text.to_owned());
                }
            }
            let bytes = response.bytes().await.map_err(|e| transport_token(&e))?;
            Ok(WireResponse {
                status,
                headers,
                body: capped_utf8(&bytes, MAX_INBOX_BODY_BYTES),
            })
        })
    }
}

/// Opaque transport-error classification — deliberately vague: reqwest's
/// `Display` embeds the URL (query strings may carry secrets), which must
/// never reach inbox files or sled (CONVENTIONS §6).
fn transport_token(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "transport timeout".to_owned()
    } else if error.is_connect() {
        "transport connect failure".to_owned()
    } else if error.is_body() || error.is_decode() {
        "transport body failure".to_owned()
    } else {
        "transport failure".to_owned()
    }
}

/// Lossy UTF-8 of the first `cap` bytes (char-boundary safe: continuation
/// bytes are dropped, `from_utf8_lossy` heals the rest).
fn capped_utf8(bytes: &[u8], cap: usize) -> String {
    if bytes.len() <= cap {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut cut = cap;
    while cut > 0 && (bytes[cut] & 0b1100_0000) == 0b1000_0000 {
        cut -= 1;
    }
    String::from_utf8_lossy(&bytes[..cut]).into_owned()
}

// ---------------------------------------------------------------------------
// Errors & outcomes
// ---------------------------------------------------------------------------

/// Plane-level failures (I/O, telemetry, `.env` access). Protocol-level
/// rejections are *outcomes* (error inbox + dead-letter), never errors.
#[derive(Debug)]
pub enum DispatchPlaneError {
    /// Filesystem failure with its operation context.
    Io {
        /// What was attempted.
        context: String,
        /// Underlying error.
        source: io::Error,
    },
    /// sled telemetry failure.
    Telemetry(TelemetryError),
    /// `.env` exists but is unreadable/malformed (message never carries
    /// values — CONVENTIONS §6).
    Env(String),
    /// A dispatch artifact failed to render/parse at the protocol layer.
    Protocol(String),
}

impl fmt::Display for DispatchPlaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::Telemetry(e) => write!(f, "telemetry: {e}"),
            Self::Env(msg) => write!(f, ".env: {msg}"),
            Self::Protocol(msg) => write!(f, "dispatch protocol: {msg}"),
        }
    }
}

impl std::error::Error for DispatchPlaneError {}

impl From<TelemetryError> for DispatchPlaneError {
    fn from(e: TelemetryError) -> Self {
        Self::Telemetry(e)
    }
}

/// Terminal state of processing one outbox correlation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// `dispatch.enabled = false`: the file stays pending, untouched
    /// (ADR-0023 cl. 6).
    Disabled,
    /// No outbox file and no deliverable checkpoint: idempotent no-op.
    Missing,
    /// A delivered checkpoint existed with the outbox file still present
    /// (crash between inbox write and removal) — the stale outbox file
    /// was removed, zero network attempts (ADR-0023 cl. 3).
    AlreadyDelivered,
    /// A non-retryable HTTP response was recorded.
    Delivered {
        /// 1-based attempt that delivered.
        attempt: u32,
        /// HTTP status.
        status: u16,
    },
    /// Validation failure or retry exhaustion: final error inbox written
    /// and the outbox file renamed into `dead/`.
    DeadLettered {
        /// Attempt recorded in the final error inbox.
        attempt: u32,
        /// Opaque failure token.
        reason: String,
    },
}

/// [`DispatchPlane::recover_outbox`] summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchRecoveryReport {
    /// Valid `<correlation>.json` names scanned.
    pub scanned: usize,
    /// Terminal [`DispatchOutcome::Delivered`]/[`DispatchOutcome::AlreadyDelivered`].
    pub delivered: usize,
    /// Terminal [`DispatchOutcome::DeadLettered`].
    pub dead: usize,
    /// Left pending (disabled plane).
    pub pending: usize,
    /// Non-conforming names skipped (left for the operator).
    pub skipped: usize,
}

// ---------------------------------------------------------------------------
// Plane
// ---------------------------------------------------------------------------

/// The dispatch outbox/inbox plane (singular-governor seam: the only
/// component that sends outbox requests and writes dispatch artifacts).
pub struct DispatchPlane {
    tree: HamletTree,
    supervisor: Arc<Supervisor>,
    client: Arc<dyn HttpClient>,
    /// Correlations with a live worker task (process-local dedupe only —
    /// across restarts the tree dedupes, ADR-0023 cl. 5).
    inflight: Mutex<BTreeSet<String>>,
    tmp_serial: AtomicU64,
}

impl DispatchPlane {
    /// Plane over the supervisor's village with an injected HTTP client.
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>, client: Arc<dyn HttpClient>) -> Self {
        Self {
            tree: supervisor.tree().clone(),
            supervisor,
            client,
            inflight: Mutex::new(BTreeSet::new()),
            tmp_serial: AtomicU64::new(0),
        }
    }

    /// The village tree this plane reads/writes.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.tree
    }

    /// Process one correlation end-to-end (ADR-0020 cl. 4–6; ADR-0023).
    /// Idempotent: re-processing a delivered or vanished file is a cheap
    /// no-op, so reactions stay hints (ADR-0019 cl. 3).
    ///
    /// # Errors
    ///
    /// [`DispatchPlaneError`] on plane-infrastructure failures (I/O,
    /// telemetry). Protocol rejections are returned as
    /// [`DispatchOutcome::DeadLettered`], not errors.
    pub async fn process(
        &self,
        correlation: &CorrelationId,
    ) -> Result<DispatchOutcome, DispatchPlaneError> {
        if !self.supervisor.config().dispatch.enabled {
            return Ok(DispatchOutcome::Disabled);
        }
        let policy = self.supervisor.config().dispatch.retry_policy();
        let name = file_name(correlation);
        let outbox_path = self.tree.courier_dispatch_outbox().join(&name);
        let inbox_path = self.tree.courier_dispatch_inbox().join(&name);

        let checkpoint = self.read_checkpoint(&inbox_path)?;
        if !outbox_path.exists() {
            return Ok(match checkpoint {
                Some(InboxResponse {
                    outcome: Outcome::Http { .. },
                    ..
                }) => DispatchOutcome::AlreadyDelivered,
                _ => DispatchOutcome::Missing,
            });
        }
        // Crash-recovery branches (ADR-0023 cl. 2–3).
        if let Some(check) = &checkpoint {
            match check.outcome {
                Outcome::Http { .. } => {
                    self.remove_outbox(&outbox_path)?;
                    self.trace(
                        correlation,
                        "dispatch-recovered",
                        "delivered checkpoint; stale outbox removed",
                    );
                    return Ok(DispatchOutcome::AlreadyDelivered);
                }
                Outcome::Error { .. } if check.attempt >= policy.max_attempts => {
                    let reason = checkpoint_reason(check);
                    self.dead_letter(correlation, &outbox_path, &reason)?;
                    return Ok(DispatchOutcome::DeadLettered {
                        attempt: check.attempt,
                        reason,
                    });
                }
                Outcome::Error { .. } => {}
            }
        }
        let start_attempt = match &checkpoint {
            Some(InboxResponse { attempt, .. }) => attempt.saturating_add(1),
            None => 1,
        };

        // Read + validate the request (oversize checked before reading the
        // whole file into memory).
        let meta = fs::metadata(&outbox_path).map_err(|source| DispatchPlaneError::Io {
            context: format!("stat {}", outbox_path.display()),
            source,
        })?;
        if meta.len() > MAX_OUTBOX_BYTES as u64 {
            return self.validation_dead(
                correlation,
                &outbox_path,
                &format!("oversize: {} bytes", meta.len()),
            );
        }
        let contents =
            fs::read_to_string(&outbox_path).map_err(|source| DispatchPlaneError::Io {
                context: format!("read {}", outbox_path.display()),
                source,
            })?;
        let request = match OutboxRequest::parse(&contents) {
            Ok(request) => request,
            Err(e) => {
                return self.validation_dead(correlation, &outbox_path, &e.to_string());
            }
        };
        let allowlist = match UrlAllowlist::parse(&self.supervisor.config().dispatch.url_allowlist)
        {
            Ok(allowlist) => allowlist,
            Err(e) => return self.validation_dead(correlation, &outbox_path, &e.to_string()),
        };
        if !allowlist.allows(&request.url) {
            return self.validation_dead(
                correlation,
                &outbox_path,
                &format!("url not allowed: {}", url_authority(&request.url)),
            );
        }

        // Attempt loop (deterministic schedule, ADR-0020 cl. 4).
        let mut attempt = start_attempt;
        loop {
            // Secrets resolve at send time only, per attempt (cl. 6).
            let env = load_env_secrets(&self.tree)?;
            let mut headers = BTreeMap::new();
            let mut missing_key: Option<String> = None;
            for (header, key) in &request.headers_env {
                match env.get(key) {
                    Some(secret) => {
                        headers.insert(header.clone(), secret.expose().to_owned());
                    }
                    None => missing_key = Some(key.clone()),
                }
            }
            if let Some(key) = missing_key {
                return self.validation_dead(
                    correlation,
                    &outbox_path,
                    &format!("missing .env key {key}"),
                );
            }
            let wire = WireRequest {
                method: request.method,
                url: request.url.clone(),
                headers,
                body: serde_json::to_string(&request.body)
                    .expect("validated JSON value always serializes"),
            };
            self.trace(
                correlation,
                "dispatch-attempt",
                format!(
                    "attempt {attempt} {} {}",
                    request.method,
                    url_authority(&request.url)
                ),
            );
            let failure = match self.client.execute(wire).await {
                Ok(response) if !is_retryable_status(response.status) => {
                    let inbox = InboxResponse::http(
                        correlation.clone(),
                        attempt,
                        response.status,
                        &response.headers,
                        &response.body,
                    );
                    self.write_inbox(&inbox)?;
                    self.remove_outbox(&outbox_path)?;
                    self.trace(
                        correlation,
                        "dispatch-delivered",
                        format!("attempt {attempt} status {}", response.status),
                    );
                    return Ok(DispatchOutcome::Delivered {
                        attempt,
                        status: response.status,
                    });
                }
                Ok(response) => format!("http {}", response.status),
                Err(token) => token,
            };
            // Retryable failure: checkpoint through the inbox file
            // (ADR-0023 cl. 1) before sleeping.
            let inbox = InboxResponse::error(correlation.clone(), attempt, &failure);
            self.write_inbox(&inbox)?;
            match policy.delay_before(attempt + 1) {
                None => {
                    self.dead_letter(correlation, &outbox_path, &failure)?;
                    return Ok(DispatchOutcome::DeadLettered {
                        attempt,
                        reason: failure,
                    });
                }
                Some(ms) => {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Startup/restart recovery: scan the outbox in sorted order and
    /// process every conforming `<correlation>.json` sequentially
    /// (restart-resumability, CONVENTIONS §3 r6). Foreign names are
    /// skipped, never touched.
    ///
    /// # Errors
    ///
    /// Directory-scan or processing failures.
    pub async fn recover_outbox(&self) -> Result<DispatchRecoveryReport, DispatchPlaneError> {
        let mut report = DispatchRecoveryReport::default();
        let dir = self.tree.courier_dispatch_outbox();
        for name in sorted_dir_names(&dir)? {
            if !name.ends_with(".json") {
                report.skipped += 1;
                continue;
            }
            let Ok(correlation) = hamlet_core::dispatch::correlation_from_file_name(&name) else {
                report.skipped += 1;
                continue;
            };
            report.scanned += 1;
            match self.process(&correlation).await? {
                DispatchOutcome::Delivered { .. } | DispatchOutcome::AlreadyDelivered => {
                    report.delivered += 1;
                }
                DispatchOutcome::DeadLettered { .. } => report.dead += 1,
                DispatchOutcome::Missing => {}
                DispatchOutcome::Disabled => report.pending += 1,
            }
        }
        Ok(report)
    }

    /// Register the outbox handler on the dispatcher and spawn its
    /// coordinator task: one worker per correlation, deduped by the
    /// in-flight set (ADR-0023 cl. 5). Other reaction variants are
    /// ignored — each plane owns its variant.
    pub fn attach(self: &Arc<Self>, dispatcher: &Dispatcher) -> JoinHandle<()> {
        let mut rx = dispatcher.register("dispatch");
        let plane = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let Reaction::DispatchOutbox { correlation } = item.reaction else {
                    continue;
                };
                let key = correlation.to_string();
                {
                    let mut inflight = plane.inflight.lock().unwrap_or_else(|e| e.into_inner());
                    if !inflight.insert(key.clone()) {
                        continue; // a worker already owns this correlation
                    }
                }
                let worker = Arc::clone(&plane);
                tokio::spawn(async move {
                    if let Err(e) = worker.process(&correlation).await {
                        eprintln!("hamlet-daemon: dispatch worker failed: {e}");
                    }
                    worker
                        .inflight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&key);
                });
            }
        })
    }

    // --- internals -----------------------------------------------------------

    /// Parse an existing inbox checkpoint (a corrupt checkpoint is treated
    /// as absent — the request is reprocessed from attempt 1 rather than
    /// trusting broken state; the checkpoint is daemon-written, so this
    /// cannot happen without external tampering).
    fn read_checkpoint(
        &self,
        inbox_path: &std::path::Path,
    ) -> Result<Option<InboxResponse>, DispatchPlaneError> {
        match fs::read_to_string(inbox_path) {
            Ok(contents) => Ok(InboxResponse::parse(&contents).ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(DispatchPlaneError::Io {
                context: format!("read {}", inbox_path.display()),
                source,
            }),
        }
    }

    /// Atomic inbox write: hidden temp → fsync → rename → dir fsync
    /// (ADR-0006 discipline; hidden temps are invisible to the watcher).
    fn write_inbox(&self, response: &InboxResponse) -> Result<(), DispatchPlaneError> {
        let dir = self.tree.courier_dispatch_inbox();
        fs::create_dir_all(&dir).map_err(|source| DispatchPlaneError::Io {
            context: format!("create {}", dir.display()),
            source,
        })?;
        let rendered = response
            .render()
            .map_err(|e| DispatchPlaneError::Protocol(e.to_string()))?;
        let final_path = dir.join(file_name(&response.correlation));
        let serial = self.tmp_serial.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".tmp-dispatch-{}-{serial}", std::process::id()));
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
        result.map_err(|source| {
            let _ = fs::remove_file(&tmp);
            DispatchPlaneError::Io {
                context: format!("write {}", final_path.display()),
                source,
            }
        })
    }

    /// Validation-failure terminal (ADR-0020 cl. 4): `attempt: 1` error
    /// inbox + immediate dead-letter rename, zero network attempts.
    fn validation_dead(
        &self,
        correlation: &CorrelationId,
        outbox_path: &std::path::Path,
        reason: &str,
    ) -> Result<DispatchOutcome, DispatchPlaneError> {
        let inbox = InboxResponse::error(correlation.clone(), 1, reason);
        self.write_inbox(&inbox)?;
        self.dead_letter(correlation, outbox_path, reason)?;
        Ok(DispatchOutcome::DeadLettered {
            attempt: 1,
            reason: reason.to_owned(),
        })
    }

    /// Rename the outbox file into `dead/` (overwrite-on-collision is
    /// fine: same correlation = same logical request, freshest evidence
    /// wins) + terminal trace.
    fn dead_letter(
        &self,
        correlation: &CorrelationId,
        outbox_path: &std::path::Path,
        reason: &str,
    ) -> Result<(), DispatchPlaneError> {
        let dead_dir = self.tree.courier_dispatch_dead();
        fs::create_dir_all(&dead_dir).map_err(|source| DispatchPlaneError::Io {
            context: format!("create {}", dead_dir.display()),
            source,
        })?;
        let target = dead_dir.join(file_name(correlation));
        fs::rename(outbox_path, &target).map_err(|source| DispatchPlaneError::Io {
            context: format!(
                "dead-letter {} → {}",
                outbox_path.display(),
                target.display()
            ),
            source,
        })?;
        if let Ok(dir_handle) = fs::File::open(&dead_dir) {
            let _ = dir_handle.sync_all();
        }
        self.trace(
            correlation,
            "dispatch-dead",
            format!("dead-lettered: {}", opaque_reason(reason)),
        );
        Ok(())
    }

    fn remove_outbox(&self, outbox_path: &std::path::Path) -> Result<(), DispatchPlaneError> {
        match fs::remove_file(outbox_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(DispatchPlaneError::Io {
                context: format!("remove {}", outbox_path.display()),
                source,
            }),
        }
    }

    /// Best-effort sled trace under the requesting identity derived from
    /// the correlation (commission-0 key space, like the mail plane).
    /// Detail carries ids/tokens only, never payloads (CONVENTIONS §6).
    fn trace(&self, correlation: &CorrelationId, event: &str, detail: impl Into<String>) {
        let record = TraceRecord {
            at_unix_ms: unix_now_ms(),
            event: event.to_owned(),
            detail: detail.into(),
        };
        let agent = AgentKey {
            commission: 0,
            uid: correlation_uid(correlation),
        };
        if let Err(e) = self.supervisor.telemetry().trace_event(agent, record) {
            eprintln!("hamlet-daemon: dispatch trace {event} failed: {e}");
        }
    }
}

/// Bring up the dispatch plane: recovery scan first (restart-resumability,
/// CONVENTIONS §3 r6), then handler registration on the shared dispatcher.
/// The daemon keeps ONE tree watcher; this plane only registers a handler.
///
/// # Errors
///
/// Recovery-scan failures.
pub async fn spawn_dispatch_plane(
    supervisor: Arc<Supervisor>,
    client: Arc<dyn HttpClient>,
    dispatcher: &Dispatcher,
) -> Result<(Arc<DispatchPlane>, JoinHandle<()>), DispatchPlaneError> {
    let plane = Arc::new(DispatchPlane::new(supervisor, client));
    let report = plane.recover_outbox().await?;
    if report.scanned > 0 {
        eprintln!(
            "hamlet-daemon: dispatch recovery scanned={} delivered={} dead={} pending={}",
            report.scanned, report.delivered, report.dead, report.pending
        );
    }
    let handle = plane.attach(dispatcher);
    Ok((plane, handle))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse `.env` into a sorted key → [`Secret`] map via dotenvy's pure
/// iterator parser (never mutates the process environment). A missing
/// `.env` resolves empty (every `headers_env` reference then fails closed,
/// ADR-0020 cl. 6). Shared by this plane and the inbound webhook auth
/// (ADR-0022 cl. 1).
///
/// # Errors
///
/// [`DispatchPlaneError::Env`] on malformed syntax (message never carries
/// values), [`DispatchPlaneError::Io`] when the file exists but is
/// unreadable.
pub fn load_env_secrets(tree: &HamletTree) -> Result<BTreeMap<String, Secret>, DispatchPlaneError> {
    let path = tree.env_file();
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => {
            return Err(DispatchPlaneError::Io {
                context: format!("open {}", path.display()),
                source,
            })
        }
    };
    let mut secrets = BTreeMap::new();
    for item in dotenvy::from_read_iter(file) {
        let (key, value) = item.map_err(|e| DispatchPlaneError::Env(e.to_string()))?;
        if !value.is_empty() {
            secrets.insert(key, Secret::new(value));
        }
    }
    Ok(secrets)
}

/// The requesting slot uid behind a dispatch correlation
/// (`d-<seq>-<uid>.<inc>`); falls back to the Mayor for foreign shapes
/// (trace attribution only — never authorization).
fn correlation_uid(correlation: &CorrelationId) -> u32 {
    correlation
        .as_str()
        .rsplit_once('-')
        .and_then(|(_, token)| token.split_once('.'))
        .and_then(|(uid, _)| uid.parse::<u32>().ok())
        .unwrap_or(MAYOR_UID)
}

/// `host[:port]` of an `https://` URL for trace detail — path and query
/// never reach telemetry (CONVENTIONS §6).
fn url_authority(url: &str) -> &str {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    let end = rest.find(['/', '?', '#']).map_or(rest.len(), |i| i);
    &rest[..end]
}

/// Clip a failure reason to an opaque single-line ASCII token for traces.
fn opaque_reason(reason: &str) -> String {
    reason
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .take(128)
        .collect()
}

/// The error token of a checkpoint, for recovery reporting.
fn checkpoint_reason(response: &InboxResponse) -> String {
    match &response.outcome {
        Outcome::Error { error } => error.clone(),
        Outcome::Http { status, .. } => format!("http {status}"),
    }
}

/// Sorted visible (non-hidden) entry names of one directory; a missing
/// directory scans empty. Deterministic order (CONVENTIONS §3 r1).
fn sorted_dir_names(dir: &std::path::Path) -> Result<Vec<String>, DispatchPlaneError> {
    let mut names = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(names),
        Err(source) => {
            return Err(DispatchPlaneError::Io {
                context: format!("scan {}", dir.display()),
                source,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| DispatchPlaneError::Io {
            context: format!("scan {}", dir.display()),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
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
    fn correlation_uid_extracts_requester() {
        let corr = CorrelationId::parse("d-3-1004.2").unwrap();
        assert_eq!(correlation_uid(&corr), 1004);
        let foreign = CorrelationId::parse("weird-shape").unwrap();
        assert_eq!(correlation_uid(&foreign), MAYOR_UID);
        let no_inc = CorrelationId::parse("d-3-1004").unwrap();
        assert_eq!(correlation_uid(&no_inc), MAYOR_UID);
    }

    #[test]
    fn url_authority_strips_path_and_query() {
        assert_eq!(
            url_authority("https://hooks.example.invalid:8443/v1/x?token=abc"),
            "hooks.example.invalid:8443"
        );
        assert_eq!(url_authority("https://example.invalid"), "example.invalid");
        assert_eq!(
            url_authority("https://example.invalid#frag"),
            "example.invalid"
        );
    }

    #[test]
    fn transport_tokens_are_opaque() {
        // The classifier exists so reqwest's URL-embedding Display never
        // reaches artifacts; assert the token vocabulary is fixed.
        for token in [
            "transport timeout",
            "transport connect failure",
            "transport body failure",
            "transport failure",
        ] {
            assert!(token.chars().all(|c| c.is_ascii_graphic() || c == ' '));
            assert!(!token.contains("https"));
        }
    }

    #[test]
    fn capped_utf8_respects_char_boundaries() {
        let text = "é".repeat(100); // 2 bytes each
        let capped = capped_utf8(text.as_bytes(), 101);
        assert_eq!(capped.len(), 100);
        assert!(capped.chars().all(|c| c == 'é'));
        assert_eq!(capped_utf8(b"short", 100), "short");
    }

    #[test]
    fn opaque_reason_sanitizes() {
        assert_eq!(opaque_reason("bad\x00\nvalue"), "bad??value");
        let long: String = "x".repeat(300);
        assert_eq!(opaque_reason(&long).len(), 128);
    }
}
