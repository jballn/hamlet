//! Local API wire contract types and schemas (SPEC §12.1–§12.2; ADR-0041).
//!
//! This module defines the pure wire contracts shared by `hamlet-daemon` (the server),
//! `hamlet-cli` (the CLI client), `hamlet-tui` (the MUD-style terminal frontend),
//! and later `hamlet-web` (the WASM frontend).
//!
//! # Invariants (CONVENTIONS §2, §3; ADR-0041)
//!
//! 1. **Purity**: Zero I/O or network side effects in this crate.
//! 2. **Canonical JSON**: Sorted keys (`BTreeMap` representation where dynamic maps occur).
//! 3. **Stable Wire Protocol**: Prefixed under [`API_V1_PREFIX`], versioned, strongly typed.
//! 4. **Auth**: Local-only secret token via `Authorization: Bearer <token>` or [`LOCAL_TOKEN_HEADER`].

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::sanitize::SafeName;

// ---------------------------------------------------------------------------
// Protocol & URL Constants
// ---------------------------------------------------------------------------

/// Route prefix for v1 API endpoints.
pub const API_V1_PREFIX: &str = "/api/v1";

/// Standard HTTP Authorization header.
pub const AUTH_HEADER: &str = "authorization";

/// Custom header for local token authentication.
pub const LOCAL_TOKEN_HEADER: &str = "x-hamlet-local-token";

/// Bearer scheme prefix in the Authorization header.
pub const BEARER_PREFIX: &str = "Bearer ";

/// Name of the daemon auth token file under `.hamlet/` (mode 0600).
pub const TOKEN_FILE_NAME: &str = "daemon.token";

/// Name of the daemon PID file under `.hamlet/`.
pub const PID_FILE_NAME: &str = "daemon.pid";

/// Name of the daemon endpoint descriptor under `.hamlet/`.
pub const ENDPOINT_FILE_NAME: &str = "daemon.endpoint";

/// Default listen port for the localhost daemon API.
pub const DEFAULT_LOCAL_PORT: u16 = 8765;

// Route endpoints
pub const ENDPOINT_STATUS: &str = "/api/v1/daemon/status";
pub const ENDPOINT_STOP: &str = "/api/v1/daemon/stop";
pub const ENDPOINT_CENSUS: &str = "/api/v1/census";
pub const ENDPOINT_COMMISSIONS: &str = "/api/v1/ledger/commissions";
pub const ENDPOINT_COMMISSION_DETAIL: &str = "/api/v1/ledger/commissions/:slug";
pub const ENDPOINT_COMMISSION_NEW: &str = "/api/v1/commissions/new";
pub const ENDPOINT_COMMISSION_CLOSE: &str = "/api/v1/commissions/:slug/close";
pub const ENDPOINT_DECREES: &str = "/api/v1/decrees";
pub const ENDPOINT_DECREE_DETAIL: &str = "/api/v1/decrees/:id";
pub const ENDPOINT_VAULT: &str = "/api/v1/vault";
pub const ENDPOINT_VAULT_EXPORT: &str = "/api/v1/vault/export";
pub const ENDPOINT_METRICS: &str = "/api/v1/metrics";
pub const ENDPOINT_MAIL: &str = "/api/v1/mail/:uid";
pub const ENDPOINT_MAIL_MESSAGE: &str = "/api/v1/mail/:uid/:file";
pub const ENDPOINT_MAIL_SEND: &str = "/api/v1/mail/send";
pub const ENDPOINT_AGENT_HIBERNATE: &str = "/api/v1/agents/:uid/hibernate";
pub const ENDPOINT_AGENT_WAKE: &str = "/api/v1/agents/wake";
pub const ENDPOINT_AGENT_STEER: &str = "/api/v1/agents/:uid/steer";
pub const ENDPOINT_WS_EVENTS: &str = "/api/v1/events/ws";
/// Per-landmark SPEC §2 projection (ADR-0043 cl. 2).
pub const ENDPOINT_LANDMARKS: &str = "/api/v1/landmarks";
/// Escalation inbox listing (ADR-0043 cl. 3).
pub const ENDPOINT_ESCALATIONS: &str = "/api/v1/escalations";
/// Operator resolution of one escalation notice (ADR-0043 cl. 3).
pub const ENDPOINT_ESCALATION_RESOLVE: &str = "/api/v1/escalations/:id/resolve";

/// Substitutes `:name` placeholders in an endpoint template, in order.
///
/// Pure string helper shared by the CLI/TUI clients so route templates are
/// never re-spelled ad hoc. Values are inserted verbatim; callers pass
/// sanitized names or integers.
#[must_use]
pub fn endpoint_path(template: &str, params: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in params {
        out = out.replace(&format!(":{key}"), value);
    }
    out
}

// ---------------------------------------------------------------------------
// Daemon Discovery & Control Models
// ---------------------------------------------------------------------------

/// Daemon endpoint descriptor written to `.hamlet/daemon.endpoint` (ADR-0041).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonEndpoint {
    /// Localhost bind IP or hostname (strictly `127.0.0.1` or `localhost`).
    pub addr: String,
    /// TCP listen port.
    pub port: u16,
    /// Process ID of the running daemon.
    pub pid: u32,
    /// Timestamp (unix ms) when the daemon process started.
    pub started_at_unix_ms: u64,
}

impl DaemonEndpoint {
    /// Constructs a new endpoint record.
    #[must_use]
    pub fn new(addr: impl Into<String>, port: u16, pid: u32, started_at_unix_ms: u64) -> Self {
        Self {
            addr: addr.into(),
            port,
            pid,
            started_at_unix_ms,
        }
    }

    /// Base HTTP URL for the local API.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.addr, self.port)
    }

    /// WebSocket URL for the event stream.
    #[must_use]
    pub fn ws_url(&self) -> String {
        format!("ws://{}:{}{}", self.addr, self.port, ENDPOINT_WS_EVENTS)
    }
}

/// Strongly-typed local authentication token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalToken(String);

impl LocalToken {
    /// Wraps an existing token string.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Returns the raw token string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison against a presented secret to avoid timing leaks.
    #[must_use]
    pub fn constant_time_eq(&self, presented: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), presented.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut acc = 0u8;
        for (x, y) in a.iter().zip(b) {
            acc |= x ^ y;
        }
        acc == 0
    }
}

impl fmt::Display for LocalToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Error Response Model
// ---------------------------------------------------------------------------

/// Standard API error envelope returned on 4xx/5xx responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiErrorResponse {
    /// Machine-readable error code.
    pub error: String,
    /// Human-readable error description.
    pub message: String,
    /// Optional contextual details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiErrorResponse {
    /// Constructs a new error response.
    #[must_use]
    pub fn new(error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            message: message.into(),
            details: None,
        }
    }

    /// Adds context details.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// 401 Unauthorized factory.
    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new("unauthorized", message)
    }

    /// 400 Bad Request factory.
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }

    /// 404 Not Found factory.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    /// 409 Conflict factory.
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new("conflict", message)
    }

    /// 500 Internal Server Error factory.
    #[must_use]
    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }
}

impl fmt::Display for ApiErrorResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.error, self.message)
    }
}

impl std::error::Error for ApiErrorResponse {}

// ---------------------------------------------------------------------------
// Read-Only State Projections
// ---------------------------------------------------------------------------

/// Village daemon status projection (`GET /api/v1/daemon/status`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatusResponse {
    /// Daemon crate package version.
    pub version: String,
    /// Process ID.
    pub pid: u32,
    /// Uptime in seconds.
    pub uptime_secs: u64,
    /// Absolute path to the village root directory (`.hamlet/`).
    pub village_root: String,
    /// Number of active agents currently supervised.
    pub active_agents: usize,
    /// Total registered commissions in charter.
    pub commissions_count: usize,
    /// Number of open/queued decrees awaiting execution.
    pub queued_decrees: usize,
    /// Number of queued or unread mail messages.
    #[serde(default)]
    pub queued_mail: usize,
}

/// Sled telemetry counters summary in API projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApiCensusCounters {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: u64,
    pub traces: u64,
}

/// Projection of one parcel census record (`GET /api/v1/census`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CensusRecordView {
    /// Slot UID (1000 for manor, 1001..=2000 for agents).
    pub uid: u32,
    /// Registered archetype or identity.
    pub identity: SafeName,
    /// Assigned commission number, if bound.
    pub commission: Option<u64>,
    /// Assigned commission slug, if resolved.
    pub commission_slug: Option<SafeName>,
    /// State string ("live", "idle", "crashed", "stasis", "stopped", "exempt").
    pub state: String,
    /// Whether parked in patron stasis.
    pub stasis: bool,
    /// Cumulative sessions run.
    pub session_count: u32,
    /// High-velocity counters from telemetry.
    pub counters: ApiCensusCounters,
    /// Last event kind recorded in TITLE.md.
    pub last_event: Option<String>,
    /// Last event timestamp string from TITLE.md.
    pub last_event_ts: Option<String>,
    /// FNV-1a content digest of TITLE.md.
    pub title_digest: String,
}

/// Summary view of one commission (`GET /api/v1/ledger/commissions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissionSummaryView {
    pub commission_id: u64,
    pub slug: SafeName,
    pub state: String,
    pub roster: Vec<u32>,
    pub deliverables_count: usize,
    #[serde(default)]
    pub brief: bool,
    #[serde(default)]
    pub indenture: bool,
    #[serde(default)]
    pub git: String,
}

/// Task item from `INDENTURE.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiIndentureTask {
    pub uid: u32,
    pub task: String,
    pub done: bool,
    #[serde(default)]
    pub identity: Option<SafeName>,
    #[serde(default)]
    pub status: String,
}

/// Dual-state convergence summary on close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiConvergenceSummary {
    pub deed_promotions: usize,
    pub patron_promotions: usize,
    pub escalation_required: bool,
}

/// Detailed view of one commission (`GET /api/v1/ledger/commissions/:slug`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissionDetailView {
    pub summary: CommissionSummaryView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_oid: Option<String>,
    pub indenture_tasks: Vec<ApiIndentureTask>,
    pub deliverables: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub convergence: Option<ApiConvergenceSummary>,
}

/// Summary view of one decree (`GET /api/v1/decrees`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecreeSummaryView {
    pub decree_id: String,
    pub seq: u64,
    pub action: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Detailed view of one decree (`GET /api/v1/decrees/:id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecreeDetailView {
    pub summary: DecreeSummaryView,
    pub raw_json: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ack_verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ack_reason: Option<String>,
}

/// Promoted deliverable item in keep/vault/ (`GET /api/v1/vault`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultItemView {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commission: Option<SafeName>,
    pub size_bytes: u64,
    pub sha256: String,
}

/// Per-agent telemetry summary in metrics view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMetricsItem {
    pub uid: u32,
    pub identity: SafeName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commission: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: u64,
    pub crashes: u32,
    pub uptime_secs: u64,
}

/// Global metrics summary projection (`GET /api/v1/metrics`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsSummaryView {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_micro_usd: u64,
    pub active_sessions: usize,
    pub agents: Vec<AgentMetricsItem>,
}

/// Mail message summary (`GET /api/v1/mail/:uid`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailMessageSummary {
    pub file_name: String,
    pub from: String,
    pub to: u32,
    pub subject: String,
    pub read: bool,
}

/// Mail message with full body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailMessageDetail {
    pub summary: MailMessageSummary,
    pub body: String,
}

// ---------------------------------------------------------------------------
// Landmark Projection (SPEC §2; ADR-0043 cl. 2)
// ---------------------------------------------------------------------------

/// `guild/` — register, blueprints, ledger.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GuildLandmark {
    /// Entry names under `guild/register/` (sorted).
    pub register: Vec<String>,
    /// Entry names under `guild/blueprints/` (sorted).
    pub blueprints: Vec<String>,
    /// Number of `commission<n>/` directories under `guild/ledger/`.
    pub ledger_commissions: usize,
}

/// `tavern/` — gossip and patrons in stasis.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TavernLandmark {
    /// Entry names under `tavern/gossip/` (sorted).
    pub gossip: Vec<String>,
    /// Patron directory names under `tavern/patrons/` (sorted).
    pub patrons: Vec<String>,
}

/// One per-UID mail spool under `courier/mail/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MailSpoolView {
    pub uid: u32,
    pub unread: usize,
    pub read: usize,
}

/// `courier/` — mail spools, freight, webhook dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CourierLandmark {
    /// Per-UID spools, sorted by uid.
    pub spools: Vec<MailSpoolView>,
    /// Sandbox directories under `courier/freight/`.
    pub freight_sandboxes: usize,
    /// Files waiting in `courier/dispatch/outbox/`.
    pub dispatch_outbox: usize,
    /// Response files in `courier/dispatch/inbox/`.
    pub dispatch_inbox: usize,
    /// Dead-lettered dispatch files.
    pub dispatch_dead: usize,
}

/// `market/` — skills and the package cache.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MarketLandmark {
    /// Entry names under `market/skills/` (sorted).
    pub skills: Vec<String>,
    /// Regular files anywhere under `market/pkgcache/`.
    pub pkgcache_entries: usize,
    /// Total bytes of those files.
    pub pkgcache_bytes: u64,
}

/// `keep/` — vault, archives, census.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeepLandmark {
    /// Regular files anywhere under `keep/vault/`.
    pub vault_items: usize,
    /// Entry names under `keep/archives/` (sorted).
    pub archives: Vec<String>,
    /// `parcel<uid>/` leases under `keep/census/` (sorted).
    pub census_parcels: Vec<u32>,
}

/// `keep/census/manor/` — the Mayor's seat.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ManorLandmark {
    /// Whether `MAYOR.md` exists.
    pub mayor_md: bool,
    /// Charter session status for UID 1000, if any (e.g. `"idle"`).
    pub mayor_session: Option<String>,
    /// Charter session incarnation for UID 1000 — one per wake (SPEC §7).
    pub mayor_incarnation: Option<u32>,
    /// Decree files quarantined in `decrees/.rejected/`.
    pub decrees_rejected: usize,
    /// Escalation notices without a resolution record.
    pub escalations_open: usize,
    /// Escalation notices with a resolution record.
    pub escalations_resolved: usize,
}

/// `GET /api/v1/landmarks` — one honest projection per SPEC §2 landmark.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LandmarksView {
    pub guild: GuildLandmark,
    pub tavern: TavernLandmark,
    pub courier: CourierLandmark,
    pub market: MarketLandmark,
    pub keep: KeepLandmark,
    pub manor: ManorLandmark,
}

// ---------------------------------------------------------------------------
// Escalation Inbox (SPEC §7; ADR-0029 cl. 4; ADR-0043 cl. 3)
// ---------------------------------------------------------------------------

/// One escalation notice in the operator inbox (`GET /api/v1/escalations`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationView {
    /// Notice file name (a SafeName) — the id used by the resolve route.
    pub id: String,
    /// Decree id the notice answers (rendered).
    pub decree_id: String,
    /// The Mayor's/daemon's single-line summary.
    pub summary: String,
    /// Village-relative evidence paths.
    pub evidence_refs: Vec<String>,
    /// Lineage timestamp of filing (display only).
    pub recorded_unix_ms: u64,
    /// Whether an operator resolution record exists.
    pub resolved: bool,
    /// The operator's resolution text, if resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

/// Request payload for `POST /api/v1/escalations/:id/resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationResolveRequest {
    /// Operator verdict (non-empty; mailed verbatim to the Mayor).
    pub resolution: String,
}

/// Response payload on escalation resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationResolveResponse {
    pub id: String,
    pub resolved: bool,
    /// Whether the verdict was spooled to the Mayor (UID 1000).
    pub mayor_notified: bool,
}

/// Version of the [`EscalationResolution`] record format.
pub const ESCALATION_RESOLUTION_VERSION: u32 = 1;

/// The daemon-written resolution record
/// `keep/census/manor/escalations/.resolved/<notice file>` (ADR-0043 cl. 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EscalationResolution {
    /// Must equal [`ESCALATION_RESOLUTION_VERSION`].
    pub resolution_version: u32,
    /// Notice file name this resolves.
    pub id: String,
    /// Operator verdict.
    pub resolution: String,
    /// Lineage timestamp only (CONVENTIONS §3 r4).
    pub recorded_unix_ms: u64,
}

impl EscalationResolution {
    /// Canonical rendering (sorted keys, pretty, trailing newline).
    #[must_use]
    pub fn render(&self) -> String {
        let value = serde_json::to_value(self).expect("plain struct serializes");
        let mut out = serde_json::to_string_pretty(&value).expect("value renders");
        out.push('\n');
        out
    }

    /// Parse a resolution record, enforcing the version.
    ///
    /// # Errors
    ///
    /// A human-readable message on schema or version mismatch.
    pub fn parse(contents: &str) -> Result<Self, String> {
        let rec: Self = serde_json::from_str(contents).map_err(|e| e.to_string())?;
        if rec.resolution_version != ESCALATION_RESOLUTION_VERSION {
            return Err(format!(
                "unsupported resolution_version {}",
                rec.resolution_version
            ));
        }
        Ok(rec)
    }
}

// ---------------------------------------------------------------------------
// Command Request & Response Models
// ---------------------------------------------------------------------------

/// Request payload for creating a new commission (`POST /api/v1/commissions/new`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissionNewRequest {
    pub slug: SafeName,
    pub brief: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_dir: Option<String>,
}

/// Response payload on commission creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissionNewResponse {
    pub commission_id: u64,
    pub slug: SafeName,
    pub message: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub ledger_dir: String,
    #[serde(default)]
    pub git_dir: String,
    #[serde(default)]
    pub brief_file: String,
    #[serde(default)]
    pub seed_desc: String,
    #[serde(default)]
    pub mayor_wake: String,
}

/// Request payload for closing a commission (`POST /api/v1/commissions/:slug/close`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissionCloseRequest {
    pub slug: SafeName,
}

/// Response payload on commission close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommissionCloseResponse {
    pub commission_id: u64,
    pub slug: SafeName,
    pub state: String,
    pub parcels_stopped: usize,
    pub escalation_filed: bool,
}

/// Request payload for hibernating an agent (`POST /api/v1/agents/:uid/hibernate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentHibernateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

/// Response payload on agent hibernation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHibernateResponse {
    pub uid: u32,
    pub patron: SafeName,
    pub status: String,
}

/// Request payload for awakening a patron (`POST /api/v1/agents/wake`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWakeRequest {
    pub patron: SafeName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commission: Option<SafeName>,
}

/// Response payload on patron awakening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWakeResponse {
    pub allocated_uid: u32,
    pub identity: SafeName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commission: Option<SafeName>,
}

/// Request payload for steering an agent (`POST /api/v1/agents/:uid/steer`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSteerRequest {
    pub directive: String,
}

/// Response payload on agent steer injection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSteerResponse {
    pub uid: u32,
    pub injected: bool,
    pub correlation: String,
}

/// Request payload for sending mail (`POST /api/v1/mail/send`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailSendRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: u32,
    pub subject: String,
    pub body: String,
}

/// Response payload on mail send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailSendResponse {
    pub delivered: bool,
    pub spool_file: String,
}

/// Request payload for exporting a vault artifact (`POST /api/v1/vault/export`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultExportRequest {
    pub artifact: String,
    pub destination: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commission: Option<SafeName>,
}

/// Response payload on vault export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultExportResponse {
    pub artifact: String,
    pub destination: String,
    pub bytes_written: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commission: Option<SafeName>,
}

/// Response payload on daemon stop command (`POST /api/v1/daemon/stop`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStopResponse {
    pub stopping: bool,
    pub pid: u32,
}

// ---------------------------------------------------------------------------
// WebSocket Event Stream Models
// ---------------------------------------------------------------------------

/// Event stream topic categories for WebSocket subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WsTopic {
    Mail,
    Decree,
    Deed,
    Git,
    Freight,
    Census,
    Commission,
    Telemetry,
    System,
}

impl WsTopic {
    /// Parses a topic string into the typed enum.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mail" => Some(Self::Mail),
            "decree" => Some(Self::Decree),
            "deed" => Some(Self::Deed),
            "git" => Some(Self::Git),
            "freight" => Some(Self::Freight),
            "census" => Some(Self::Census),
            "commission" => Some(Self::Commission),
            "telemetry" => Some(Self::Telemetry),
            "system" => Some(Self::System),
            _ => None,
        }
    }

    /// Stable string token for the topic.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Decree => "decree",
            Self::Deed => "deed",
            Self::Git => "git",
            Self::Freight => "freight",
            Self::Census => "census",
            Self::Commission => "commission",
            Self::Telemetry => "telemetry",
            Self::System => "system",
        }
    }
}

impl fmt::Display for WsTopic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Event frame streamed over the WebSocket endpoint (`GET /api/v1/events/ws`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsEvent {
    /// Topic category.
    pub topic: WsTopic,
    /// Specific event identifier (e.g. "mail_received", "decree_executed", "agent_crashed").
    pub event_kind: String,
    /// Monotonic sequence number.
    pub seq: u64,
    /// Timestamp (unix ms) when the event occurred.
    pub timestamp_unix_ms: u64,
    /// Typed event payload.
    pub payload: serde_json::Value,
}

impl WsEvent {
    /// Constructs a new WebSocket event frame.
    #[must_use]
    pub fn new(
        topic: WsTopic,
        event_kind: impl Into<String>,
        seq: u64,
        timestamp_unix_ms: u64,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            topic,
            event_kind: event_kind.into(),
            seq,
            timestamp_unix_ms,
            payload,
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket Bidirectional Framing Protocol (SPEC §12.3, §14; ADR-0044)
// ---------------------------------------------------------------------------

/// Maximum allowed length for spectator chat bubble text (SPEC §12.3 honesty rule).
pub const MAX_BUBBLE_TEXT_LEN: usize = 140;

/// Maximum allowed length for spectator mail snippet (SPEC §12.3 honesty rule).
pub const MAX_MAIL_SNIPPET_LEN: usize = 64;

mod string_or_int_map {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;
    use std::str::FromStr;

    pub fn serialize<S>(
        map: &Option<BTreeMap<u32, usize>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        map.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<BTreeMap<u32, usize>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<BTreeMap<String, usize>> = Option::deserialize(deserializer)?;
        match opt {
            None => Ok(None),
            Some(m) => {
                let mut out = BTreeMap::new();
                for (k, v) in m {
                    let id = u32::from_str(&k).map_err(serde::de::Error::custom)?;
                    out.insert(id, v);
                }
                Ok(Some(out))
            }
        }
    }
}

/// Incremental or snapshot projection delta frame streamed from daemon to client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDeltaFrame {
    /// Ephemeral daemon instance identifier (changes on restart).
    pub daemon_instance_id: String,
    /// Monotonic sequence number for this connection.
    pub seq: u64,
    /// Telemetry/event timestamp (unix ms).
    pub timestamp_unix_ms: u64,
    /// True if this frame represents a full state snapshot (e.g. on connect or resync).
    pub is_snapshot: bool,
    /// Active working parcels view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub census: Option<Vec<CensusRecordView>>,
    /// Active commission summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commissions: Option<Vec<CommissionSummaryView>>,
    /// Decree summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decrees: Option<Vec<DecreeSummaryView>>,
    /// Vault deliverable items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<Vec<VaultItemView>>,
    /// Per-agent mailbox unread/total counts (UID -> count).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "string_or_int_map"
    )]
    pub mail_counts: Option<BTreeMap<u32, usize>>,
    /// Village landmark topology projections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landmarks: Option<LandmarksView>,
    /// Unresolved manor escalations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalations: Option<Vec<EscalationView>>,
}

impl StateDeltaFrame {
    /// Creates a full state snapshot frame (e.g. on initial connect or resync).
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn snapshot(
        daemon_instance_id: impl Into<String>,
        seq: u64,
        timestamp_unix_ms: u64,
        census: Vec<CensusRecordView>,
        commissions: Vec<CommissionSummaryView>,
        decrees: Vec<DecreeSummaryView>,
        vault: Vec<VaultItemView>,
        mail_counts: BTreeMap<u32, usize>,
        landmarks: LandmarksView,
        escalations: Vec<EscalationView>,
    ) -> Self {
        Self {
            daemon_instance_id: daemon_instance_id.into(),
            seq,
            timestamp_unix_ms,
            is_snapshot: true,
            census: Some(census),
            commissions: Some(commissions),
            decrees: Some(decrees),
            vault: Some(vault),
            mail_counts: Some(mail_counts),
            landmarks: Some(landmarks),
            escalations: Some(escalations),
        }
    }

    /// Creates an incremental diff frame with only specified domain projections updated.
    #[must_use]
    pub fn diff(daemon_instance_id: impl Into<String>, seq: u64, timestamp_unix_ms: u64) -> Self {
        Self {
            daemon_instance_id: daemon_instance_id.into(),
            seq,
            timestamp_unix_ms,
            is_snapshot: false,
            census: None,
            commissions: None,
            decrees: None,
            vault: None,
            mail_counts: None,
            landmarks: None,
            escalations: None,
        }
    }
}

/// Batched metric ticks streamed from the daemon sled projections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsBatchFrame {
    /// Monotonic sequence number.
    pub seq: u64,
    /// Timestamp (unix ms) when batch was gathered.
    pub timestamp_unix_ms: u64,
    /// Bounded batch of parcel metric samples.
    pub ticks: Vec<ParcelMetricTick>,
}

impl MetricsBatchFrame {
    /// Constructs a new metrics batch.
    #[must_use]
    pub fn new(seq: u64, timestamp_unix_ms: u64, ticks: Vec<ParcelMetricTick>) -> Self {
        Self {
            seq,
            timestamp_unix_ms,
            ticks,
        }
    }
}

/// Per-parcel/session resource telemetry sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParcelMetricTick {
    /// Agent UID slot.
    pub uid: u32,
    /// Active session incarnation identifier if running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Accumulated or delta input tokens.
    pub tokens_in: u64,
    /// Accumulated or delta output tokens.
    pub tokens_out: u64,
    /// Total cost in micro USD ($0.000001).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micro_usd: Option<u64>,
    /// Turn or model invocation latency in milliseconds.
    pub latency_ms: u64,
    /// Resident memory bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// CPU usage in tenths of a percent (e.g. 155 = 15.5%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent_x10: Option<u32>,
}

/// Visual target for an over-the-head chat bubble in the 8-bit village.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum BubbleTarget {
    /// Subagent parcel by UID (1001-2000).
    Agent(u32),
    /// Mayor orchestrator (UID 1000).
    Mayor,
    /// Village landmark building (e.g. "guild", "tavern", "courier", "market", "keep", "manor").
    Landmark(String),
    /// Village-wide broadcast (over central plaza).
    Broadcast,
}

/// Typed categories of speech/chat bubbles (SPEC §12.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BubbleKind {
    /// Mail delivery notice: subject and snippet only (body strictly suppressed per SPEC §12.3).
    MailSnippet {
        from_uid: u32,
        to_uid: u32,
        subject: String,
        snippet: String,
    },
    /// Confirmation of operator steer directive injection.
    SteerConfirmation { uid: u32, directive_snippet: String },
    /// Escalation notice surfacing a blocker to the Mayor/Operator.
    EscalationNotice {
        escalation_id: String,
        reason: String,
    },
    /// Decree execution verdict (applied, rejected, or replayed).
    DecreeAck {
        decree_id: String,
        success: bool,
        summary: String,
    },
    /// Agent conversational speech bubble.
    AgentSpeech { uid: u32, text: String },
}

/// Chat-bubble event for over-the-head rendering above village sprites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BubbleFrame {
    /// Monotonic sequence number.
    pub seq: u64,
    /// Timestamp (unix ms) when event occurred.
    pub timestamp_unix_ms: u64,
    /// Target entity over whose head the bubble appears.
    pub target: BubbleTarget,
    /// Specific typed bubble variant.
    pub kind: BubbleKind,
    /// Sanitized, bounded display text for the bubble.
    pub text: String,
    /// Suggested on-screen display duration in milliseconds.
    pub duration_ms: u32,
}

impl BubbleFrame {
    /// Constructs a mail snippet bubble, enforcing the SPEC §12.3 honesty rule.
    #[must_use]
    pub fn mail_snippet(
        seq: u64,
        timestamp_unix_ms: u64,
        from_uid: u32,
        to_uid: u32,
        subject: impl Into<String>,
        raw_snippet: impl AsRef<str>,
    ) -> Self {
        let subject = subject.into();
        let snippet = Self::truncate_safe(raw_snippet.as_ref(), MAX_MAIL_SNIPPET_LEN);
        let text = format!("[Mail] {subject}: {snippet}");
        let text = Self::truncate_safe(&text, MAX_BUBBLE_TEXT_LEN);
        Self {
            seq,
            timestamp_unix_ms,
            target: BubbleTarget::Agent(to_uid),
            kind: BubbleKind::MailSnippet {
                from_uid,
                to_uid,
                subject,
                snippet,
            },
            text,
            duration_ms: 4000,
        }
    }

    /// Constructs a steer confirmation bubble.
    #[must_use]
    pub fn steer(seq: u64, timestamp_unix_ms: u64, uid: u32, directive: impl AsRef<str>) -> Self {
        let snippet = Self::truncate_safe(directive.as_ref(), 48);
        let text = format!("Steered: {snippet}");
        let text = Self::truncate_safe(&text, MAX_BUBBLE_TEXT_LEN);
        Self {
            seq,
            timestamp_unix_ms,
            target: BubbleTarget::Agent(uid),
            kind: BubbleKind::SteerConfirmation {
                uid,
                directive_snippet: snippet,
            },
            text,
            duration_ms: 3000,
        }
    }

    /// Constructs an escalation notice bubble.
    #[must_use]
    pub fn escalation(
        seq: u64,
        timestamp_unix_ms: u64,
        escalation_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        let escalation_id = escalation_id.into();
        let reason = reason.into();
        let text = format!("! Escalation: {reason}");
        let text = Self::truncate_safe(&text, MAX_BUBBLE_TEXT_LEN);
        Self {
            seq,
            timestamp_unix_ms,
            target: BubbleTarget::Mayor,
            kind: BubbleKind::EscalationNotice {
                escalation_id,
                reason,
            },
            text,
            duration_ms: 5000,
        }
    }

    /// Constructs a decree ack bubble.
    #[must_use]
    pub fn decree_ack(
        seq: u64,
        timestamp_unix_ms: u64,
        decree_id: impl Into<String>,
        success: bool,
        summary: impl Into<String>,
    ) -> Self {
        let decree_id = decree_id.into();
        let summary = summary.into();
        let status = if success { "Ack" } else { "Rejected" };
        let text = format!("Decree {status}: {summary}");
        let text = Self::truncate_safe(&text, MAX_BUBBLE_TEXT_LEN);
        Self {
            seq,
            timestamp_unix_ms,
            target: BubbleTarget::Landmark("manor".to_string()),
            kind: BubbleKind::DecreeAck {
                decree_id,
                success,
                summary,
            },
            text,
            duration_ms: 4000,
        }
    }

    /// Constructs an agent speech bubble.
    #[must_use]
    pub fn agent_speech(
        seq: u64,
        timestamp_unix_ms: u64,
        uid: u32,
        speech: impl AsRef<str>,
    ) -> Self {
        let text = Self::truncate_safe(speech.as_ref(), MAX_BUBBLE_TEXT_LEN);
        Self {
            seq,
            timestamp_unix_ms,
            target: BubbleTarget::Agent(uid),
            kind: BubbleKind::AgentSpeech {
                uid,
                text: text.clone(),
            },
            text,
            duration_ms: 3500,
        }
    }

    fn truncate_safe(s: &str, max_len: usize) -> String {
        let mut chars = s.chars();
        let truncated: String = chars.by_ref().take(max_len).collect();
        if chars.next().is_some() && max_len >= 3 {
            let mut prefix: String = truncated.chars().take(max_len - 3).collect();
            prefix.push_str("...");
            prefix
        } else {
            truncated
        }
    }
}

/// Acknowledgment frame returned for client-initiated mutations over WebSocket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAckFrame {
    /// Monotonic sequence number.
    pub seq: u64,
    /// Timestamp (unix ms) when acknowledgment was generated.
    pub timestamp_unix_ms: u64,
    /// Correlation ID supplied by client in [`WsCommandFrame`].
    pub correlation_id: String,
    /// REST endpoint path corresponding to the mutation.
    pub command_endpoint: String,
    /// True if command executed successfully.
    pub success: bool,
    /// HTTP-equivalent status code (e.g. 200, 400, 404, 409).
    pub status_code: u16,
    /// Optional response payload (e.g. `CommissionNewResponse`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Optional error description when `success` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CommandAckFrame {
    /// Constructs a successful command acknowledgment.
    #[must_use]
    pub fn success(
        seq: u64,
        timestamp_unix_ms: u64,
        correlation_id: impl Into<String>,
        command_endpoint: impl Into<String>,
        payload: Option<serde_json::Value>,
    ) -> Self {
        Self {
            seq,
            timestamp_unix_ms,
            correlation_id: correlation_id.into(),
            command_endpoint: command_endpoint.into(),
            success: true,
            status_code: 200,
            payload,
            error: None,
        }
    }

    /// Constructs an error command acknowledgment.
    #[must_use]
    pub fn error(
        seq: u64,
        timestamp_unix_ms: u64,
        correlation_id: impl Into<String>,
        command_endpoint: impl Into<String>,
        status_code: u16,
        error: impl Into<String>,
    ) -> Self {
        Self {
            seq,
            timestamp_unix_ms,
            correlation_id: correlation_id.into(),
            command_endpoint: command_endpoint.into(),
            success: false,
            status_code,
            payload: None,
            error: Some(error.into()),
        }
    }
}

/// Server-to-client frames streamed over the WebSocket connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsServerFrame {
    /// Village projection state snapshot or diff.
    StateDelta(Box<StateDeltaFrame>),
    /// Rate-limited batch of parcel telemetry ticks.
    Metrics(MetricsBatchFrame),
    /// Over-the-head chat bubble event.
    Bubble(BubbleFrame),
    /// Acknowledgment of a client mutation command.
    CommandAck(CommandAckFrame),
    /// Notice requiring client to clear local state and resync.
    ResyncRequired {
        seq: u64,
        timestamp_unix_ms: u64,
        daemon_instance_id: String,
        reason: String,
    },
    /// Heartbeat response to client ping.
    Pong { timestamp_unix_ms: u64 },
    /// Backward-compatible legacy event frame.
    Event(WsEvent),
}

/// Client-to-server frames received over the WebSocket connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientFrame {
    /// Mutation command mapped 1:1 onto local API endpoints.
    Command(WsCommandFrame),
    /// Request full state snapshot resync.
    Resync {
        last_seen_seq: Option<u64>,
        daemon_instance_id: Option<String>,
    },
    /// Heartbeat ping.
    Ping { timestamp_unix_ms: u64 },
    /// Heartbeat pong.
    Pong { timestamp_unix_ms: u64 },
}

/// Client command container with correlation identifier for asynchronous response correlation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsCommandFrame {
    /// Client-generated correlation ID.
    pub correlation_id: String,
    /// Typed command action and parameters.
    pub action: WsCommandAction,
}

impl WsCommandFrame {
    /// Constructs a new command frame.
    #[must_use]
    pub fn new(correlation_id: impl Into<String>, action: WsCommandAction) -> Self {
        Self {
            correlation_id: correlation_id.into(),
            action,
        }
    }
}

/// Typed client mutation commands mapping 1:1 onto ADR-0041/ADR-0043 daemon endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "payload", rename_all = "snake_case")]
pub enum WsCommandAction {
    /// Create new commission (`POST /api/v1/commissions/new`).
    CommissionNew(CommissionNewRequest),
    /// Close active commission (`POST /api/v1/commissions/:slug/close`).
    CommissionClose {
        slug: SafeName,
        request: CommissionCloseRequest,
    },
    /// Hibernate active parcel (`POST /api/v1/agents/:uid/hibernate`).
    AgentHibernate {
        uid: u32,
        request: AgentHibernateRequest,
    },
    /// Wake patron into parcel (`POST /api/v1/agents/wake`).
    AgentWake(AgentWakeRequest),
    /// Steer active agent session (`POST /api/v1/agents/:uid/steer`).
    AgentSteer {
        uid: u32,
        request: AgentSteerRequest,
    },
    /// Send mail message (`POST /api/v1/mail/send`).
    MailSend(MailSendRequest),
    /// Export vault artifact (`POST /api/v1/vault/export`).
    VaultExport(VaultExportRequest),
    /// Resolve manor escalation (`POST /api/v1/escalations/:id/resolve`).
    EscalationResolve {
        id: SafeName,
        request: EscalationResolveRequest,
    },
    /// Stop daemon (`POST /api/v1/daemon/stop`).
    DaemonStop,
}

impl WsCommandAction {
    /// Returns the exact REST endpoint path string for this command action.
    #[must_use]
    pub fn endpoint_path(&self) -> String {
        match self {
            Self::CommissionNew(_) => ENDPOINT_COMMISSION_NEW.to_string(),
            Self::CommissionClose { slug, .. } => {
                endpoint_path(ENDPOINT_COMMISSION_CLOSE, &[("slug", slug.as_str())])
            }
            Self::AgentHibernate { uid, .. } => {
                let uid_str = uid.to_string();
                endpoint_path(ENDPOINT_AGENT_HIBERNATE, &[("uid", &uid_str)])
            }
            Self::AgentWake(_) => ENDPOINT_AGENT_WAKE.to_string(),
            Self::AgentSteer { uid, .. } => {
                let uid_str = uid.to_string();
                endpoint_path(ENDPOINT_AGENT_STEER, &[("uid", &uid_str)])
            }
            Self::MailSend(_) => ENDPOINT_MAIL_SEND.to_string(),
            Self::VaultExport(_) => ENDPOINT_VAULT_EXPORT.to_string(),
            Self::EscalationResolve { id, .. } => {
                endpoint_path(ENDPOINT_ESCALATION_RESOLVE, &[("id", id.as_str())])
            }
            Self::DaemonStop => ENDPOINT_STOP.to_string(),
        }
    }

    /// Stable identifier name of the action.
    #[must_use]
    pub fn action_name(&self) -> &'static str {
        match self {
            Self::CommissionNew(_) => "commission_new",
            Self::CommissionClose { .. } => "commission_close",
            Self::AgentHibernate { .. } => "agent_hibernate",
            Self::AgentWake(_) => "agent_wake",
            Self::AgentSteer { .. } => "agent_steer",
            Self::MailSend(_) => "mail_send",
            Self::VaultExport(_) => "vault_export",
            Self::EscalationResolve { .. } => "escalation_resolve",
            Self::DaemonStop => "daemon_stop",
        }
    }
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::sanitize_name;

    fn safe(s: &str) -> SafeName {
        sanitize_name(s).expect("valid safe name")
    }

    #[test]
    fn endpoint_urls_and_json_roundtrip() {
        let ep = DaemonEndpoint::new("127.0.0.1", 8765, 4242, 1729000000000);
        assert_eq!(ep.base_url(), "http://127.0.0.1:8765");
        assert_eq!(ep.ws_url(), "ws://127.0.0.1:8765/api/v1/events/ws");

        let json = serde_json::to_string(&ep).expect("serialize");
        let decoded: DaemonEndpoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, ep);
    }

    #[test]
    fn local_token_constant_time_equality() {
        let token = LocalToken::new("secret-token-1234567890abcdef");
        assert!(token.constant_time_eq("secret-token-1234567890abcdef"));
        assert!(!token.constant_time_eq("secret-token-1234567890abcdeg"));
        assert!(!token.constant_time_eq("short"));
        assert!(!token.constant_time_eq(""));
    }

    #[test]
    fn error_response_roundtrip() {
        let err = ApiErrorResponse::bad_request("invalid commission slug")
            .with_details(serde_json::json!({ "field": "slug" }));
        let json = serde_json::to_string(&err).expect("serialize");
        let decoded: ApiErrorResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, err);
        assert_eq!(err.error, "bad_request");
    }

    #[test]
    fn census_view_roundtrip() {
        let view = CensusRecordView {
            uid: 1001,
            identity: safe("scout"),
            commission: Some(1),
            commission_slug: Some(safe("alpha")),
            state: "live".into(),
            stasis: false,
            session_count: 2,
            counters: ApiCensusCounters {
                input_tokens: 1200,
                output_tokens: 800,
                cost_micro_usd: 50,
                traces: 10,
            },
            last_event: Some("session".into()),
            last_event_ts: Some("2026-10-19T10:00:00Z".into()),
            title_digest: "abcd1234".into(),
        };

        let json = serde_json::to_string(&view).expect("serialize");
        let decoded: CensusRecordView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, view);
    }

    #[test]
    fn ws_topic_parsing_and_display() {
        for s in [
            "mail",
            "decree",
            "deed",
            "git",
            "freight",
            "census",
            "commission",
            "telemetry",
            "system",
        ] {
            let topic = WsTopic::parse(s).expect("known topic");
            assert_eq!(topic.as_str(), s);
            assert_eq!(topic.to_string(), s);
        }
        assert_eq!(WsTopic::parse("unknown"), None);
    }

    #[test]
    fn ws_event_roundtrip() {
        let event = WsEvent::new(
            WsTopic::Mail,
            "mail_received",
            42,
            1729000000000,
            serde_json::json!({ "from": "mayor", "to": 1001 }),
        );
        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: WsEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, event);
    }

    #[test]
    fn endpoint_path_substitutes_placeholders() {
        assert_eq!(
            endpoint_path(ENDPOINT_AGENT_STEER, &[("uid", "1001")]),
            "/api/v1/agents/1001/steer"
        );
        assert_eq!(
            endpoint_path(
                ENDPOINT_MAIL_MESSAGE,
                &[("uid", "1000"), ("file", "a.mail")]
            ),
            "/api/v1/mail/1000/a.mail"
        );
        assert_eq!(
            endpoint_path(ENDPOINT_ESCALATION_RESOLVE, &[("id", "esc-1")]),
            "/api/v1/escalations/esc-1/resolve"
        );
    }

    #[test]
    fn landmarks_view_roundtrip_and_defaults() {
        let mut view = LandmarksView::default();
        view.courier.spools.push(MailSpoolView {
            uid: 1000,
            unread: 2,
            read: 1,
        });
        view.keep.census_parcels = vec![1001, 1002];
        view.manor.mayor_incarnation = Some(3);
        let json = serde_json::to_string(&view).expect("serialize");
        let decoded: LandmarksView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, view);
    }

    #[test]
    fn escalation_resolution_render_parse_and_version() {
        let rec = EscalationResolution {
            resolution_version: ESCALATION_RESOLUTION_VERSION,
            id: "d-1-abc".into(),
            resolution: "approve rework".into(),
            recorded_unix_ms: 1,
        };
        let text = rec.render();
        assert!(text.ends_with('\n'));
        // Sorted keys: canonical JSON (CONVENTIONS §3 r3).
        let id_at = text.find("\"id\"").unwrap();
        let ver_at = text.find("\"resolution_version\"").unwrap();
        assert!(id_at < ver_at);
        assert_eq!(EscalationResolution::parse(&text).unwrap(), rec);
        let bad = text.replace("\"resolution_version\": 1", "\"resolution_version\": 9");
        assert!(EscalationResolution::parse(&bad).is_err());
    }

    #[test]
    fn state_delta_frame_snapshot_and_diff_roundtrip() {
        let mut mail_counts = BTreeMap::new();
        mail_counts.insert(1000, 3);
        mail_counts.insert(1001, 1);

        let snap = StateDeltaFrame::snapshot(
            "inst-42",
            1,
            1729000000000,
            vec![CensusRecordView {
                uid: 1001,
                identity: safe("builder-bot"),
                commission: Some(42),
                commission_slug: Some(safe("world-slice")),
                state: "live".into(),
                stasis: false,
                session_count: 1,
                counters: ApiCensusCounters {
                    input_tokens: 120,
                    output_tokens: 30,
                    cost_micro_usd: 1500,
                    traces: 5,
                },
                last_event: Some("pi_tool_call".into()),
                last_event_ts: Some("1729000000000".into()),
                title_digest: "abc".into(),
            }],
            vec![CommissionSummaryView {
                commission_id: 1,
                slug: safe("world-slice"),
                state: "Active".into(),
                roster: vec![1001],
                deliverables_count: 1,
                brief: true,
                indenture: true,
                git: "init".into(),
            }],
            vec![DecreeSummaryView {
                decree_id: "d-1-init".into(),
                seq: 1,
                action: "charter_update".into(),
                status: "applied".into(),
                reason: Some("initial layout".into()),
            }],
            vec![VaultItemView {
                name: "deliverable.tar.gz".into(),
                commission: Some(safe("world-slice")),
                size_bytes: 1048576,
                sha256: "abcdef0123456789".into(),
            }],
            mail_counts,
            LandmarksView::default(),
            vec![],
        );

        let json = serde_json::to_string(&snap).expect("serialize snapshot");
        let decoded: StateDeltaFrame = serde_json::from_str(&json).expect("deserialize snapshot");
        assert_eq!(decoded, snap);
        assert!(decoded.is_snapshot);

        // Incremental diff
        let mut diff = StateDeltaFrame::diff("inst-42", 2, 1729000001000);
        let mut updated_counts = BTreeMap::new();
        updated_counts.insert(1001, 2);
        diff.mail_counts = Some(updated_counts);

        let diff_json = serde_json::to_string(&diff).expect("serialize diff");
        let diff_decoded: StateDeltaFrame =
            serde_json::from_str(&diff_json).expect("deserialize diff");
        assert_eq!(diff_decoded, diff);
        assert!(!diff_decoded.is_snapshot);
        assert!(diff_decoded.census.is_none());
    }

    #[test]
    fn metrics_batch_frame_roundtrip() {
        let frame = MetricsBatchFrame::new(
            10,
            1729000000000,
            vec![
                ParcelMetricTick {
                    uid: 1001,
                    session_id: Some("sess-1".into()),
                    tokens_in: 12000,
                    tokens_out: 450,
                    cost_micro_usd: Some(15000),
                    latency_ms: 320,
                    memory_bytes: Some(64 * 1024 * 1024),
                    cpu_percent_x10: Some(185), // 18.5%
                },
                ParcelMetricTick {
                    uid: 1000,
                    session_id: None,
                    tokens_in: 500,
                    tokens_out: 100,
                    cost_micro_usd: None,
                    latency_ms: 45,
                    memory_bytes: None,
                    cpu_percent_x10: None,
                },
            ],
        );

        let json = serde_json::to_string(&frame).expect("serialize metrics batch");
        let decoded: MetricsBatchFrame =
            serde_json::from_str(&json).expect("deserialize metrics batch");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn bubble_frame_honesty_rule_and_truncation() {
        // Honesty rule: bodies strictly suppressed, only snippet delivered
        let bubble = BubbleFrame::mail_snippet(
            12,
            1729000000000,
            1001,
            1000,
            "Re: Task Completion",
            "Artifact delivered at keep/vault/output.txt with verified hash",
        );
        assert_eq!(bubble.target, BubbleTarget::Agent(1000));
        assert!(bubble.text.starts_with("[Mail] Re: Task Completion:"));
        assert!(bubble.text.len() <= MAX_BUBBLE_TEXT_LEN);

        if let BubbleKind::MailSnippet { snippet, .. } = &bubble.kind {
            assert!(snippet.len() <= MAX_MAIL_SNIPPET_LEN);
        } else {
            panic!("expected MailSnippet kind");
        }

        // Steer bubble
        let steer_bubble =
            BubbleFrame::steer(13, 1729000000000, 1001, "Focus on fixing broken unit test");
        assert_eq!(steer_bubble.target, BubbleTarget::Agent(1001));
        assert!(steer_bubble.text.contains("Focus on fixing"));

        // Long text truncation test
        let oversized = "A".repeat(300);
        let truncated_speech = BubbleFrame::agent_speech(14, 1729000000000, 1001, &oversized);
        assert!(truncated_speech.text.len() <= MAX_BUBBLE_TEXT_LEN);
        assert!(truncated_speech.text.ends_with("..."));
    }

    #[test]
    fn command_ack_frame_roundtrip() {
        let ack_ok = CommandAckFrame::success(
            5,
            1729000000000,
            "corr-123",
            "/api/v1/commissions/new",
            Some(serde_json::json!({"slug": "test-comm", "created": true})),
        );
        let json = serde_json::to_string(&ack_ok).expect("serialize ack_ok");
        let decoded: CommandAckFrame = serde_json::from_str(&json).expect("deserialize ack_ok");
        assert_eq!(decoded, ack_ok);
        assert!(decoded.success);
        assert_eq!(decoded.status_code, 200);

        let ack_err = CommandAckFrame::error(
            6,
            1729000000000,
            "corr-124",
            "/api/v1/agents/1001/steer",
            404,
            "parcel 1001 is hibernated",
        );
        let err_json = serde_json::to_string(&ack_err).expect("serialize ack_err");
        let err_decoded: CommandAckFrame =
            serde_json::from_str(&err_json).expect("deserialize ack_err");
        assert_eq!(err_decoded, ack_err);
        assert!(!err_decoded.success);
        assert_eq!(err_decoded.status_code, 404);
        assert_eq!(
            err_decoded.error.as_deref(),
            Some("parcel 1001 is hibernated")
        );
    }

    #[test]
    fn ws_server_frame_tagged_matrix() {
        let snap_frame =
            WsServerFrame::StateDelta(Box::new(StateDeltaFrame::diff("inst-1", 1, 100)));
        let json = serde_json::to_string(&snap_frame).expect("serialize snap_frame");
        assert!(json.contains("\"type\":\"state_delta\""));
        let decoded: WsServerFrame = serde_json::from_str(&json).expect("deserialize snap_frame");
        assert_eq!(decoded, snap_frame);

        let resync = WsServerFrame::ResyncRequired {
            seq: 99,
            timestamp_unix_ms: 100,
            daemon_instance_id: "inst-2".into(),
            reason: "daemon restarted".into(),
        };
        let r_json = serde_json::to_string(&resync).expect("serialize resync");
        assert!(r_json.contains("\"type\":\"resync_required\""));
        let r_decoded: WsServerFrame = serde_json::from_str(&r_json).expect("deserialize resync");
        assert_eq!(r_decoded, resync);
    }

    #[test]
    fn ws_command_action_endpoint_mapping_and_roundtrip() {
        let cases: Vec<(WsCommandAction, &str, &str)> = vec![
            (
                WsCommandAction::CommissionNew(CommissionNewRequest {
                    slug: safe("world-slice"),
                    brief: "Make game".into(),
                    seed_dir: None,
                }),
                "/api/v1/commissions/new",
                "commission_new",
            ),
            (
                WsCommandAction::CommissionClose {
                    slug: safe("alpha"),
                    request: CommissionCloseRequest {
                        slug: safe("alpha"),
                    },
                },
                "/api/v1/commissions/alpha/close",
                "commission_close",
            ),
            (
                WsCommandAction::AgentHibernate {
                    uid: 1002,
                    request: AgentHibernateRequest {
                        trigger: Some("idle".into()),
                    },
                },
                "/api/v1/agents/1002/hibernate",
                "agent_hibernate",
            ),
            (
                WsCommandAction::AgentWake(AgentWakeRequest {
                    patron: safe("coder"),
                    commission: Some(safe("alpha")),
                }),
                "/api/v1/agents/wake",
                "agent_wake",
            ),
            (
                WsCommandAction::AgentSteer {
                    uid: 1003,
                    request: AgentSteerRequest {
                        directive: "pause and reflect".into(),
                    },
                },
                "/api/v1/agents/1003/steer",
                "agent_steer",
            ),
            (
                WsCommandAction::MailSend(MailSendRequest {
                    from: Some("1000".into()),
                    to: 1001,
                    subject: "Directive".into(),
                    body: "Hello".into(),
                }),
                "/api/v1/mail/send",
                "mail_send",
            ),
            (
                WsCommandAction::VaultExport(VaultExportRequest {
                    artifact: "bundle.tar.gz".into(),
                    destination: "/tmp/out".into(),
                    commission: Some(safe("alpha")),
                }),
                "/api/v1/vault/export",
                "vault_export",
            ),
            (
                WsCommandAction::EscalationResolve {
                    id: safe("esc-1"),
                    request: EscalationResolveRequest {
                        resolution: "Approved".into(),
                    },
                },
                "/api/v1/escalations/esc-1/resolve",
                "escalation_resolve",
            ),
            (
                WsCommandAction::DaemonStop,
                "/api/v1/daemon/stop",
                "daemon_stop",
            ),
        ];

        for (action, expected_endpoint, expected_name) in cases {
            assert_eq!(action.endpoint_path(), expected_endpoint);
            assert_eq!(action.action_name(), expected_name);

            let cmd_frame = WsClientFrame::Command(WsCommandFrame::new("corr-1", action));
            let json = serde_json::to_string(&cmd_frame).expect("serialize cmd_frame");
            assert!(json.contains("\"type\":\"command\""));
            let decoded: WsClientFrame =
                serde_json::from_str(&json).expect("deserialize cmd_frame");
            assert_eq!(decoded, cmd_frame);
        }
    }

    #[test]
    fn ws_protocol_golden_vectors() {
        // Golden vector 1: Resync frame
        let golden_resync =
            r#"{"type":"resync","last_seen_seq":42,"daemon_instance_id":"inst-prev"}"#;
        let client_frame: WsClientFrame =
            serde_json::from_str(golden_resync).expect("parse golden resync");
        assert_eq!(
            client_frame,
            WsClientFrame::Resync {
                last_seen_seq: Some(42),
                daemon_instance_id: Some("inst-prev".into()),
            }
        );

        // Golden vector 2: Server Bubble Frame
        let golden_bubble = r#"{
            "type": "bubble",
            "seq": 7,
            "timestamp_unix_ms": 1729000000000,
            "target": {"type": "agent", "id": 1001},
            "kind": {
                "kind": "mail_snippet",
                "from_uid": 1000,
                "to_uid": 1001,
                "subject": "Status check",
                "snippet": "Are tests passing?"
            },
            "text": "[Mail] Status check: Are tests passing?",
            "duration_ms": 4000
        }"#;
        let server_frame: WsServerFrame =
            serde_json::from_str(golden_bubble).expect("parse golden bubble");
        match server_frame {
            WsServerFrame::Bubble(b) => {
                assert_eq!(b.seq, 7);
                assert_eq!(b.target, BubbleTarget::Agent(1001));
                assert_eq!(b.text, "[Mail] Status check: Are tests passing?");
            }
            _ => panic!("expected bubble frame"),
        }
    }
}
