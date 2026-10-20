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
}
