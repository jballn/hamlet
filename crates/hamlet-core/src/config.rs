//! `config.json` + `.env` schemas (SPEC §2 tree roots, §11.4 secrets).
//!
//! Typed schema for orchestrator/engine settings (`config.json`) and the
//! `.env` secret surface (mode 0600, dotenvy-hydrated out-of-band). Secrets
//! never serialize into charter.json, archives, logs, or test fixtures
//! (CONVENTIONS §6); schema types make leakage unrepresentable:
//! [`Secret`] has no [`serde::Serialize`] impl and a redacting `Debug`, so
//! [`Secrets`] cannot be marshalled into any hamlet artifact by accident.
//!
//! Everything here is pure (ADR-0001): parsing/rendering operate on strings
//! and injected readers; file I/O belongs to the caller (`hamlet init`,
//! daemon).
//!
//! # `config.json` versioning
//!
//! `config_version` is mandatory; migrations are additive-only with explicit
//! version steps (CONVENTIONS §3). Unknown fields are rejected at parse time
//! so operator typos fail loudly instead of silently dropping settings.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Schema version this build writes and accepts (CONVENTIONS §3).
///
/// v1 → v2 (Sprint 02): additive `resources` section (container resource
/// limit placeholders). v2 → v3 (Sprint 03, ADR-0015 cl. 4): additive
/// `sessions` section (session supervision: pi model selection, readiness/
/// stop timing, delivery-queue bound, restart policy). v3 → v4 (Sprint 03
/// step 04, ADR-0016): additive `sessions.latency_spike_ms` (stream
/// telemetry spike threshold). v4 → v5 (Sprint 04, ADR-0020): additive
/// `dispatch` keys — `url_allowlist`, `max_attempts`, `backoff_initial_ms`,
/// `backoff_max_ms` (dispatch protocol policy). Older documents still parse
/// — new fields carry `#[serde(default)]`, so migration is "parse old,
/// write new"; no rewrite of existing trees is required.
pub const CONFIG_VERSION: u32 = 6;

/// Default daemon data dir (sled telemetry location). Relative paths
/// resolve against the *working directory containing* the village root —
/// see [`Config::resolve_daemon_data_dir`] — so the default lands at
/// `<working-dir>/.hamlet-data/sled`, a sibling of `.hamlet/`, never inside
/// the tree: telemetry is out-of-band (ADR-0005).
const DEFAULT_DAEMON_DATA_DIR: &str = ".hamlet-data/sled";
/// Default podman binary; resolved via `PATH` by the daemon (SPEC §4:
/// rootless podman CLI only).
const DEFAULT_PODMAN_BIN: &str = "podman";
/// Default base image (SPEC §4: slim Alpine + uutils). Sprint 02 builds the
/// real hamlet base image; until then the vanilla reference is the default
/// and operators may override it in `config.json`.
const DEFAULT_BASE_IMAGE: &str = "docker.io/library/alpine:3.20";
/// Default web listen address (SPEC §12.3): loopback-only until an operator
/// explicitly widens it.
const DEFAULT_WEB_LISTEN_ADDR: &str = "127.0.0.1:8787";
/// Default per-request webhook dispatch timeout (SPEC §8.4).
const DEFAULT_DISPATCH_TIMEOUT_SECS: u64 = 30;
/// Default dispatch retry budget (ADR-0020 cl. 4).
const DEFAULT_DISPATCH_MAX_ATTEMPTS: u32 = 5;
/// Default dispatch backoff initial delay, ms (ADR-0020 cl. 4).
const DEFAULT_DISPATCH_BACKOFF_INITIAL_MS: u64 = 5_000;
/// Default dispatch backoff cap, ms (ADR-0020 cl. 4).
const DEFAULT_DISPATCH_BACKOFF_MAX_MS: u64 = 900_000;
/// Default `.env` key holding the inbound-webhook shared secret (ADR-0022).
const DEFAULT_WEBHOOK_AUTH_ENV_KEY: &str = "HAMLET_WEBHOOK_AUTH";
/// Default inbound-webhook body cap, bytes (ADR-0022 cl. 2).
const DEFAULT_WEBHOOK_MAX_BODY_BYTES: u64 = 64 * 1024;
/// Default container memory limit (conservative until the Sprint 07 policy
/// ADR fixes the real curve).
const DEFAULT_CONTAINER_MEMORY: &str = "512m";
/// Default container CPU limit (`--cpus` value).
const DEFAULT_CONTAINER_CPUS: &str = "1.0";
/// Default container PID limit (`--pids-limit`); fork-bomb brace.
const DEFAULT_CONTAINER_PIDS_LIMIT: u64 = 256;
/// Default readiness-probe timeout for spawned sessions (ADR-0015 cl. 4).
const DEFAULT_SESSION_READY_TIMEOUT_MS: u64 = 30_000;
/// Default graceful-stop window before SIGKILL (ADR-0015 cl. 4).
const DEFAULT_SESSION_STOP_GRACE_MS: u64 = 10_000;
/// Default per-session delivery-queue bound (Sprint 03 step 03).
const DEFAULT_SESSION_QUEUE_DEPTH: u32 = 64;
/// Default stream latency-spike threshold (Sprint 03 step 04, ADR-0016):
/// any measured stream span (agent run/turn, tool call, delivery or
/// interrupt acknowledgment, mid-run frame gap) at or above this many ms is
/// recorded as a `latency.v1` spike.
const DEFAULT_SESSION_LATENCY_SPIKE_MS: u64 = 30_000;
/// Default consecutive auto-restart budget (ADR-0015 cl. 4).
const DEFAULT_SESSION_MAX_RESTARTS: u32 = 5;
/// Default first restart-backoff delay (ADR-0015 cl. 4).
const DEFAULT_SESSION_BACKOFF_INITIAL_MS: u64 = 1_000;
/// Default restart-backoff cap (ADR-0015 cl. 4).
const DEFAULT_SESSION_BACKOFF_MAX_MS: u64 = 60_000;

/// Orchestrator/engine settings — the typed form of `.hamlet/config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema version; must equal [`CONFIG_VERSION`] (migrations are
    /// additive-only with explicit version steps).
    pub config_version: u32,
    /// Daemon data dir (sled telemetry location, SPEC §11.2). Relative
    /// paths resolve against the working directory containing the village
    /// root, never inside the `.hamlet/` tree (ADR-0005); telemetry is
    /// never orchestration state (CONVENTIONS §2).
    pub daemon_data_dir: PathBuf,
    /// Podman binary path (rootless CLI only, SPEC §4). `PATH`-resolved when
    /// relative.
    pub podman_bin: PathBuf,
    /// Base image reference for agent containers (SPEC §4).
    pub base_image: String,
    /// Web/WS listen address (`socketaddr` form, SPEC §12.3).
    pub web_listen_addr: String,
    /// Daemon log level.
    pub log_level: LogLevel,
    /// External webhook dispatch settings (SPEC §8.4; policy per ADR-0020
    /// — allowlist, deterministic retry, dead-letter; endpoint auth lives
    /// in `.env`, never here).
    pub dispatch: DispatchConfig,
    /// Container resource-limit placeholders (SPEC §4; the binding policy
    /// ADR is Sprint 07 — values here are wired into every `podman run` as
    /// `--memory` / `--cpus` / `--pids-limit`). Absent in v1 documents;
    /// defaults apply.
    #[serde(default)]
    pub resources: ResourcesConfig,
    /// Agent-session supervision (SPEC §6; keys fixed by ADR-0015 cl. 4):
    /// pi model selection, readiness/stop timing, delivery-queue bound,
    /// restart policy. Absent in v1/v2 documents; defaults apply.
    #[serde(default)]
    pub sessions: SessionsConfig,
    /// Inbound webhook endpoints (SPEC §8.5; policy per ADR-0022): shared-
    /// secret auth by `.env` key name, body cap, topic → sink routing
    /// table. The secret itself never appears here. Absent in ≤v5
    /// documents; defaults apply.
    #[serde(default)]
    pub webhooks: WebhooksConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            daemon_data_dir: PathBuf::from(DEFAULT_DAEMON_DATA_DIR),
            podman_bin: PathBuf::from(DEFAULT_PODMAN_BIN),
            base_image: DEFAULT_BASE_IMAGE.to_string(),
            web_listen_addr: DEFAULT_WEB_LISTEN_ADDR.to_string(),
            log_level: LogLevel::default(),
            dispatch: DispatchConfig::default(),
            resources: ResourcesConfig::default(),
            sessions: SessionsConfig::default(),
            webhooks: WebhooksConfig::default(),
        }
    }
}

impl Config {
    /// Parse + validate `config.json` contents.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Syntax`] on malformed JSON or unknown fields, then
    /// whatever [`Config::validate`] reports.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self =
            serde_json::from_str(text).map_err(|e| ConfigError::Syntax(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Canonical human-readable rendering (pretty JSON, trailing newline) —
    /// what `hamlet init` writes to `config.json`.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = serde_json::to_string_pretty(self)
            .expect("plain-struct Config serialization is infallible");
        out.push('\n');
        out
    }

    /// Validate every field. Called by [`Config::parse`]; exposed for daemon
    /// re-validation after edits.
    ///
    /// # Errors
    ///
    /// [`ConfigError::UnsupportedVersion`] or [`ConfigError::Invalid`] with a
    /// field-qualified message.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.config_version == 0 || self.config_version > CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.config_version));
        }
        if self.daemon_data_dir.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "daemon_data_dir must not be empty".to_string(),
            ));
        }
        if self.podman_bin.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "podman_bin must not be empty".to_string(),
            ));
        }
        if self.base_image.is_empty() {
            return Err(ConfigError::Invalid(
                "base_image must not be empty".to_string(),
            ));
        }
        if self
            .base_image
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(ConfigError::Invalid(
                "base_image must not contain whitespace or control characters".to_string(),
            ));
        }
        if self
            .web_listen_addr
            .parse::<std::net::SocketAddr>()
            .is_err()
        {
            return Err(ConfigError::Invalid(format!(
                "web_listen_addr must be a socket address (host:port), got {:?}",
                self.web_listen_addr
            )));
        }
        self.dispatch.validate()?;
        self.resources.validate()?;
        self.sessions.validate()?;
        self.webhooks.validate()
    }

    /// Resolve [`Config::daemon_data_dir`] for the village rooted at
    /// `village_root` (the `.hamlet/` dir): absolute paths pass through;
    /// relative paths resolve against the *working directory containing*
    /// the village root, so telemetry never lands inside the tree
    /// (out-of-band per SPEC §11.2; ratified by ADR-0005). Pure — the
    /// caller performs any I/O.
    #[must_use]
    pub fn resolve_daemon_data_dir(&self, village_root: &Path) -> PathBuf {
        if self.daemon_data_dir.is_relative() {
            village_root
                .parent()
                .unwrap_or(village_root)
                .join(&self.daemon_data_dir)
        } else {
            self.daemon_data_dir.clone()
        }
    }
}

/// Daemon log level (`config.json` field `log_level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        };
        f.write_str(name)
    }
}

/// Webhook dispatch settings (SPEC §8.4; policy fixed by ADR-0020).
/// Endpoint auth comes from `.env` secrets referenced by key name only —
/// never from `config.json` and never inline in outbox files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchConfig {
    /// Whether the daemon POSTs `courier/dispatch/outbox/` files at all.
    pub enabled: bool,
    /// Per-request HTTP timeout, seconds (1..=3600).
    pub timeout_secs: u64,
    /// URL allowlist entries `https://host[:port][/prefix]` (ADR-0020
    /// cl. 3). Empty (the default) denies every dispatch.
    #[serde(default)]
    pub url_allowlist: Vec<String>,
    /// Total attempts per request before dead-letter, 1..=16 (ADR-0020
    /// cl. 4). Absent in v4 documents; default applies.
    #[serde(default = "default_dispatch_max_attempts")]
    pub max_attempts: u32,
    /// Backoff delay before attempt 2, ms (ADR-0020 cl. 4).
    #[serde(default = "default_dispatch_backoff_initial_ms")]
    pub backoff_initial_ms: u64,
    /// Backoff delay cap, ms (ADR-0020 cl. 4).
    #[serde(default = "default_dispatch_backoff_max_ms")]
    pub backoff_max_ms: u64,
}

fn default_dispatch_max_attempts() -> u32 {
    DEFAULT_DISPATCH_MAX_ATTEMPTS
}
fn default_dispatch_backoff_initial_ms() -> u64 {
    DEFAULT_DISPATCH_BACKOFF_INITIAL_MS
}
fn default_dispatch_backoff_max_ms() -> u64 {
    DEFAULT_DISPATCH_BACKOFF_MAX_MS
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: DEFAULT_DISPATCH_TIMEOUT_SECS,
            url_allowlist: Vec::new(),
            max_attempts: DEFAULT_DISPATCH_MAX_ATTEMPTS,
            backoff_initial_ms: DEFAULT_DISPATCH_BACKOFF_INITIAL_MS,
            backoff_max_ms: DEFAULT_DISPATCH_BACKOFF_MAX_MS,
        }
    }
}

impl DispatchConfig {
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] when `timeout_secs` is outside 1..=3600,
    /// an allowlist entry is malformed, or the retry policy is out of the
    /// ADR-0020 cl. 4 ranges.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.timeout_secs == 0 || self.timeout_secs > 3600 {
            return Err(ConfigError::Invalid(format!(
                "dispatch.timeout_secs must be in 1..=3600, got {}",
                self.timeout_secs
            )));
        }
        crate::dispatch::UrlAllowlist::parse(&self.url_allowlist)
            .map_err(|e| ConfigError::Invalid(format!("dispatch.url_allowlist: {e}")))?;
        self.retry_policy()
            .validate()
            .map_err(|e| ConfigError::Invalid(format!("dispatch: {e}")))
    }

    /// The ADR-0020 cl. 4 retry policy as a typed value.
    #[must_use]
    pub fn retry_policy(&self) -> crate::dispatch::RetryPolicy {
        crate::dispatch::RetryPolicy {
            max_attempts: self.max_attempts,
            initial_ms: self.backoff_initial_ms,
            max_ms: self.backoff_max_ms,
        }
    }
}

/// Inbound webhook endpoint settings (SPEC §8.5; policy fixed by ADR-0022).
/// The shared secret comes from `.env` under [`WebhooksConfig::auth_env_key`]
/// — never from `config.json`. The routing table maps a sanitized topic
/// name to its sinks; topics outside the table are rejected (404).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhooksConfig {
    /// Whether the daemon serves `POST /webhook/<topic>` at all.
    pub enabled: bool,
    /// `.env` key holding the shared secret (`[A-Z][A-Z0-9_]{0,63}`;
    /// compared against the `X-Hamlet-Auth` request header — ADR-0022
    /// cl. 1). A referenced-but-missing key fails every request closed
    /// (503), never open.
    pub auth_env_key: String,
    /// Request body cap, bytes (1..=1048576; oversize → 413 — ADR-0022
    /// cl. 2).
    pub max_body_bytes: u64,
    /// Topic → sinks. Keys must pass the §4 sanitizer (they become file
    /// name components and mail subjects); an empty table (the default)
    /// rejects every topic.
    pub routes: BTreeMap<String, WebhookRoute>,
}

/// Sinks of one webhook topic route (ADR-0022 cl. 3). At least one sink
/// must be active — a route that does nothing is a config error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookRoute {
    /// Write the validated event as a dispatch **inbox** file
    /// (`courier/dispatch/inbox/<correlation>.json`, ADR-0022 cl. 4).
    #[serde(default)]
    pub dispatch_inbox: bool,
    /// Deliver the event as mail to these slot UIDs (1000–2000; the mail
    /// router applies the normal bounce rules — ADR-0022 cl. 5).
    #[serde(default)]
    pub mail_to: Vec<u32>,
}

impl Default for WebhooksConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auth_env_key: DEFAULT_WEBHOOK_AUTH_ENV_KEY.to_string(),
            max_body_bytes: DEFAULT_WEBHOOK_MAX_BODY_BYTES,
            routes: BTreeMap::new(),
        }
    }
}

impl WebhooksConfig {
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] when `auth_env_key` violates the `.env`
    /// key grammar, `max_body_bytes` is out of 1..=1 MiB, a topic fails
    /// the §4 sanitizer, a route has no sink, or a `mail_to` uid is not a
    /// slot uid / is duplicated.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !crate::dispatch::is_env_key(&self.auth_env_key) {
            return Err(ConfigError::Invalid(format!(
                "webhooks.auth_env_key must match [A-Z][A-Z0-9_]{{0,63}}, got {:?}",
                self.auth_env_key
            )));
        }
        if self.max_body_bytes == 0
            || self.max_body_bytes > crate::dispatch::MAX_OUTBOX_BYTES as u64
        {
            return Err(ConfigError::Invalid(format!(
                "webhooks.max_body_bytes must be in 1..={}, got {}",
                crate::dispatch::MAX_OUTBOX_BYTES,
                self.max_body_bytes
            )));
        }
        for (topic, route) in &self.routes {
            if crate::sanitize::sanitize_name(topic).is_err() {
                return Err(ConfigError::Invalid(format!(
                    "webhooks.routes topic {topic:?} fails the §4 sanitizer"
                )));
            }
            if !route.dispatch_inbox && route.mail_to.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "webhooks.routes[{topic:?}] must enable dispatch_inbox or list mail_to recipients"
                )));
            }
            let mut seen = std::collections::BTreeSet::new();
            for uid in &route.mail_to {
                if !crate::mail::is_slot_uid(*uid) {
                    return Err(ConfigError::Invalid(format!(
                        "webhooks.routes[{topic:?}].mail_to uid {uid} is not a slot uid (1000–2000)"
                    )));
                }
                if !seen.insert(uid) {
                    return Err(ConfigError::Invalid(format!(
                        "webhooks.routes[{topic:?}].mail_to lists uid {uid} twice"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn memory_re() -> &'static Regex {
    // podman --memory byte-unit form: digits + optional b/k/m/g suffix.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9]+[bkmgBKMG]?$").unwrap())
}

fn cpus_re() -> &'static Regex {
    // podman --cpus: decimal or percentage form.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9]+(\.[0-9]+)?%?$").unwrap())
}

/// Container resource-limit settings (SPEC §4 hardening; policy **[DEV]**
/// until the Sprint 07 ADR — these are the `config.json` placeholders the
/// daemon's podman layer wires into every `podman run`). Defaults are
/// deliberately conservative; values are charset-validated because they
/// land verbatim in argv (ADR-0010: typed sources only, never a shell).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesConfig {
    /// `--memory` value, e.g. `"512m"` (digits + optional b/k/m/g suffix).
    pub memory: String,
    /// `--cpus` value, e.g. `"1.0"` or `"50%"`.
    pub cpus: String,
    /// `--pids-limit` (1..=65535; unlimited is never a default).
    pub pids_limit: u64,
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            memory: DEFAULT_CONTAINER_MEMORY.to_string(),
            cpus: DEFAULT_CONTAINER_CPUS.to_string(),
            pids_limit: DEFAULT_CONTAINER_PIDS_LIMIT,
        }
    }
}

impl ResourcesConfig {
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] when a value falls outside the charset or
    /// range podman (and ADR-0010 argv safety) accepts.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !memory_re().is_match(&self.memory) {
            return Err(ConfigError::Invalid(format!(
                "resources.memory must match <digits>[bkmg], got {:?}",
                self.memory
            )));
        }
        if !cpus_re().is_match(&self.cpus) {
            return Err(ConfigError::Invalid(format!(
                "resources.cpus must be a decimal or percentage, got {:?}",
                self.cpus
            )));
        }
        if self.pids_limit == 0 || self.pids_limit > 65535 {
            return Err(ConfigError::Invalid(format!(
                "resources.pids_limit must be in 1..=65535, got {}",
                self.pids_limit
            )));
        }
        Ok(())
    }
}

/// Agent-session supervision settings (`config.json` `sessions`; keys fixed
/// by ADR-0015 cl. 4 — Sprint 03 step 02 consumes them). Timeouts are
/// daemon-side wall-clock *behavior* (probe/sleep durations), never state
/// inputs (CONVENTIONS §3 r4): the restart *schedule* derives purely from
/// the persisted `restart_count` via [`RestartConfig::backoff_ms`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsConfig {
    /// pi `--provider` value; `None` = pi's own default. Charset-validated:
    /// lands verbatim in argv (ADR-0010 cl. 2), never a shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// pi `--model` value; `None` = provider default. Same charset rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Readiness window after spawn: the `get_state` probe (ADR-0015 cl. 3)
    /// must be answered within this many ms, else the session is `crashed`
    /// and the restart policy runs. Range 1_000..=600_000.
    pub ready_timeout_ms: u64,
    /// Graceful-stop window: close stdin (pi's orderly-shutdown signal) +
    /// SIGTERM, then SIGKILL after this many ms. Range 500..=300_000.
    pub stop_grace_ms: u64,
    /// Per-session delivery-queue bound; overflow is a typed bounce back to
    /// the sender (Sprint 03 step 03). Range 1..=10_000.
    pub queue_depth: u32,
    /// Stream latency-spike threshold in ms (ADR-0016): a measured span at
    /// or above it is appended to `latency.v1`. Spans measured from the
    /// JSON-RPC stream: `agent-run`, `agent-turn`, `tool-call`,
    /// `stream-gap` (mid-run frame gap), `delivery-ack`, `interrupt-ack`.
    /// Range 1..=3_600_000. Absent in v1–v3 documents; the default applies.
    #[serde(default = "default_latency_spike_ms")]
    pub latency_spike_ms: u64,
    /// When true, restarted incarnations pass pi `--continue` so the agent
    /// resumes its own session history from the parcel home (context
    /// continuity across supervised restarts; ADR-0015 cl. 1).
    pub continue_on_restart: bool,
    /// Restart policy bounds + backoff.
    #[serde(default)]
    pub restart: RestartConfig,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            provider: None,
            model: None,
            ready_timeout_ms: DEFAULT_SESSION_READY_TIMEOUT_MS,
            stop_grace_ms: DEFAULT_SESSION_STOP_GRACE_MS,
            queue_depth: DEFAULT_SESSION_QUEUE_DEPTH,
            latency_spike_ms: DEFAULT_SESSION_LATENCY_SPIKE_MS,
            continue_on_restart: true,
            restart: RestartConfig::default(),
        }
    }
}

/// serde default for [`SessionsConfig::latency_spike_ms`] so v1–v3
/// documents keep parsing (additive migration, CONVENTIONS §3 r3).
fn default_latency_spike_ms() -> u64 {
    DEFAULT_SESSION_LATENCY_SPIKE_MS
}

impl SessionsConfig {
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the offending field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, value) in [
            ("sessions.provider", &self.provider),
            ("sessions.model", &self.model),
        ] {
            if let Some(value) = value {
                let ok = !value.is_empty()
                    && !value.starts_with('-')
                    && value.chars().all(|c| !c.is_whitespace() && !c.is_control());
                if !ok {
                    return Err(ConfigError::Invalid(format!(
                        "{field} must be a non-empty single argv token (no leading '-', no whitespace/control characters), got {value:?}"
                    )));
                }
            }
        }
        if !(1_000..=600_000).contains(&self.ready_timeout_ms) {
            return Err(ConfigError::Invalid(format!(
                "sessions.ready_timeout_ms must be in 1000..=600000, got {}",
                self.ready_timeout_ms
            )));
        }
        if !(500..=300_000).contains(&self.stop_grace_ms) {
            return Err(ConfigError::Invalid(format!(
                "sessions.stop_grace_ms must be in 500..=300000, got {}",
                self.stop_grace_ms
            )));
        }
        if self.queue_depth == 0 || self.queue_depth > 10_000 {
            return Err(ConfigError::Invalid(format!(
                "sessions.queue_depth must be in 1..=10000, got {}",
                self.queue_depth
            )));
        }
        if self.latency_spike_ms == 0 || self.latency_spike_ms > 3_600_000 {
            return Err(ConfigError::Invalid(format!(
                "sessions.latency_spike_ms must be in 1..=3600000, got {}",
                self.latency_spike_ms
            )));
        }
        self.restart.validate()
    }
}

/// Restart policy (SPEC §6 supervision; ADR-0015 cl. 4). After
/// `max_restarts` consecutive auto-restarts *without the session reaching a
/// task boundary*, the session stays `crashed` permanently (lineage stamp
/// to TITLE.md, trace to sled). The `restart_count` resets to 0 on the
/// first `agent_settled` of a life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartConfig {
    /// Consecutive auto-restart budget; 0 disables auto-restart. 0..=100.
    pub max_restarts: u32,
    /// First backoff delay, ms. 100..=600_000.
    pub backoff_initial_ms: u64,
    /// Backoff cap, ms. `backoff_initial_ms`..=3_600_000.
    pub backoff_max_ms: u64,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            max_restarts: DEFAULT_SESSION_MAX_RESTARTS,
            backoff_initial_ms: DEFAULT_SESSION_BACKOFF_INITIAL_MS,
            backoff_max_ms: DEFAULT_SESSION_BACKOFF_MAX_MS,
        }
    }
}

impl RestartConfig {
    /// Delay before auto-restart number `restart_count + 1`, or [`None`]
    /// when the budget is exhausted. Delegates to the pure schedule in
    /// [`crate::session::restart_backoff_ms`].
    #[must_use]
    pub fn backoff_ms(&self, restart_count: u32) -> Option<u64> {
        crate::session::restart_backoff_ms(
            self.backoff_initial_ms,
            self.backoff_max_ms,
            self.max_restarts,
            restart_count,
        )
    }

    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the offending field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_restarts > 100 {
            return Err(ConfigError::Invalid(format!(
                "sessions.restart.max_restarts must be in 0..=100, got {}",
                self.max_restarts
            )));
        }
        if !(100..=600_000).contains(&self.backoff_initial_ms) {
            return Err(ConfigError::Invalid(format!(
                "sessions.restart.backoff_initial_ms must be in 100..=600000, got {}",
                self.backoff_initial_ms
            )));
        }
        if self.backoff_max_ms < self.backoff_initial_ms || self.backoff_max_ms > 3_600_000 {
            return Err(ConfigError::Invalid(format!(
                "sessions.restart.backoff_max_ms must be in backoff_initial_ms..=3600000, got {}",
                self.backoff_max_ms
            )));
        }
        Ok(())
    }
}

/// `config.json` parse/validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Malformed JSON, wrong field types, or unknown fields.
    Syntax(String),
    /// `config_version` is 0 or newer than this build supports.
    UnsupportedVersion(u32),
    /// A field failed validation (message names the field).
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(msg) => write!(f, "config.json syntax error: {msg}"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "unsupported config_version {v} (this build writes/accepts {CONFIG_VERSION})"
            ),
            Self::Invalid(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

// --- .env secrets (SPEC §11.4; CONVENTIONS §6) -----------------------------

/// `.env` variable: model endpoint used by agent sessions.
pub const ENV_MODEL_ENDPOINT: &str = "HAMLET_MODEL_ENDPOINT";
/// `.env` variable: default model API key.
pub const ENV_MODEL_API_KEY: &str = "HAMLET_MODEL_API_KEY";
/// `.env` prefix: additional provider keys (`HAMLET_API_KEY_<PROVIDER>`).
pub const ENV_API_KEY_PREFIX: &str = "HAMLET_API_KEY_";

/// The `.env` template written by `hamlet init` (mode 0600). Every entry is
/// commented out and every key value is an obvious fake
/// (`test-key-do-not-use`) so a freshly initialized tree contains no
/// material that could ever be mistaken for a real secret.
pub const ENV_TEMPLATE: &str = "\
# hamlet .env — secrets (SPEC §11.4; CONVENTIONS §6).
# Mode 0600. Never commit this file. Hydrated via dotenvy into the daemon
# and injected into agent sessions out-of-band; values here are NEVER
# written to charter.json, archives, or logs.
#
# Uncomment and replace the placeholders below. All sample values are
# obvious fakes.

# Model endpoint used by agent sessions:
#HAMLET_MODEL_ENDPOINT=https://model.example.invalid/v1

# Default model API key:
#HAMLET_MODEL_API_KEY=test-key-do-not-use

# Additional provider keys (HAMLET_API_KEY_<PROVIDER>):
#HAMLET_API_KEY_ANTHROPIC=test-key-do-not-use
#HAMLET_API_KEY_OPENAI=test-key-do-not-use

# Shared secret for inbound webhook auth (ADR-0022; sent as the
# X-Hamlet-Auth header on POST /webhook/<topic>):
#HAMLET_WEBHOOK_AUTH=test-key-do-not-use

# Endpoint credentials referenced BY KEY NAME from dispatch outbox files
# (ADR-0020 cl. 1/6 `headers_env` — values live only here), for example:
#HAMLET_DISPATCH_AUTH=test-key-do-not-use
";

/// A single secret value. Deliberately has **no** [`serde::Serialize`] impl
/// and no [`fmt::Display`]: writing a secret into charter.json, logs, or
/// archives does not type-check. The only read path is [`Secret::expose`],
/// which is greppable at review time. `Debug` redacts.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The only accessor to the raw value (out-of-band injection points
    /// only — never logging, never charter serialization).
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Typed secret surface of `.env` (SPEC §11.4). Only the variables hamlet
/// understands are collected; the rest of the environment is never mirrored.
/// No `Serialize` derive — see [`Secret`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Secrets {
    /// `HAMLET_MODEL_ENDPOINT`
    pub model_endpoint: Option<Secret>,
    /// `HAMLET_MODEL_API_KEY`
    pub model_api_key: Option<Secret>,
    /// `HAMLET_API_KEY_<PROVIDER>` → provider name (suffix as written).
    /// `BTreeMap`: sorted iteration, always (CONVENTIONS §3).
    pub provider_keys: BTreeMap<String, Secret>,
}

impl Secrets {
    /// Collect secrets from already-parsed `(key, value)` pairs. Empty
    /// values are treated as unset. Unknown variables are ignored.
    #[must_use]
    pub fn from_vars<K, V, I>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut secrets = Self::default();
        for (key, value) in vars {
            let (key, value) = (key.as_ref(), value.as_ref());
            if value.is_empty() {
                continue;
            }
            if key == ENV_MODEL_ENDPOINT {
                secrets.model_endpoint = Some(Secret::new(value));
            } else if key == ENV_MODEL_API_KEY {
                secrets.model_api_key = Some(Secret::new(value));
            } else if let Some(provider) = key.strip_prefix(ENV_API_KEY_PREFIX) {
                if !provider.is_empty() {
                    secrets
                        .provider_keys
                        .insert(provider.to_string(), Secret::new(value));
                }
            }
        }
        secrets
    }

    /// Parse `.env` contents from an injected reader via dotenvy's pure
    /// iterator parser (`from_read_iter` — never `from_read`, which would
    /// mutate the process environment and breach the ADR-0001 purity seam),
    /// then collect via [`Secrets::from_vars`]. The caller opens the file.
    ///
    /// # Errors
    ///
    /// [`SecretsError::Parse`] on malformed `.env` syntax or read failure.
    pub fn from_read(reader: impl std::io::Read) -> Result<Self, SecretsError> {
        // BTreeMap first: duplicate keys resolve to the last *non-empty*
        // value in sorted, deterministic order regardless of file ordering
        // (an empty value means unset and must not shadow a real one).
        let mut vars = BTreeMap::new();
        for item in dotenvy::from_read_iter(reader) {
            let (key, value) = item.map_err(|e| SecretsError::Parse(e.to_string()))?;
            if !value.is_empty() {
                vars.insert(key, value);
            }
        }
        Ok(Self::from_vars(
            vars.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        ))
    }

    /// True when no secret material was collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.model_endpoint.is_none()
            && self.model_api_key.is_none()
            && self.provider_keys.is_empty()
    }
}

/// `.env` parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsError {
    /// Malformed `.env` syntax or underlying read error (message only — the
    /// error never carries secret values).
    Parse(String),
}

impl fmt::Display for SecretsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, ".env parse error: {msg}"),
        }
    }
}

impl std::error::Error for SecretsError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Committed golden vector: the exact bytes `hamlet init` writes to
    /// `config.json` (defaults, `config_version` 6).
    #[test]
    fn default_config_render_golden() {
        let expected = "\
{
  \"config_version\": 6,
  \"daemon_data_dir\": \".hamlet-data/sled\",
  \"podman_bin\": \"podman\",
  \"base_image\": \"docker.io/library/alpine:3.20\",
  \"web_listen_addr\": \"127.0.0.1:8787\",
  \"log_level\": \"info\",
  \"dispatch\": {
    \"enabled\": false,
    \"timeout_secs\": 30,
    \"url_allowlist\": [],
    \"max_attempts\": 5,
    \"backoff_initial_ms\": 5000,
    \"backoff_max_ms\": 900000
  },
  \"resources\": {
    \"memory\": \"512m\",
    \"cpus\": \"1.0\",
    \"pids_limit\": 256
  },
  \"sessions\": {
    \"ready_timeout_ms\": 30000,
    \"stop_grace_ms\": 10000,
    \"queue_depth\": 64,
    \"latency_spike_ms\": 30000,
    \"continue_on_restart\": true,
    \"restart\": {
      \"max_restarts\": 5,
      \"backoff_initial_ms\": 1000,
      \"backoff_max_ms\": 60000
    }
  },
  \"webhooks\": {
    \"enabled\": false,
    \"auth_env_key\": \"HAMLET_WEBHOOK_AUTH\",
    \"max_body_bytes\": 65536,
    \"routes\": {}
  }
}
";
        assert_eq!(Config::default().render(), expected);
    }

    /// v1 → v2 migration is additive: a v1 document (no `resources` block)
    /// still parses, defaults fill in, and the declared version is left
    /// untouched (CONVENTIONS §3.3 — no silent rewrites).
    #[test]
    fn v1_config_parses_with_default_resources() {
        let v1 = "\
{
  \"config_version\": 1,
  \"daemon_data_dir\": \".hamlet-data/sled\",
  \"podman_bin\": \"podman\",
  \"base_image\": \"docker.io/library/alpine:3.20\",
  \"web_listen_addr\": \"127.0.0.1:8787\",
  \"log_level\": \"info\",
  \"dispatch\": {
    \"enabled\": false,
    \"timeout_secs\": 30
  }
}
";
        let parsed = Config::parse(v1).expect("v1 must still parse");
        assert_eq!(parsed.config_version, 1);
        assert_eq!(parsed.resources, ResourcesConfig::default());
    }

    /// v2 → v3 migration is additive: a v2 document (no `sessions` block)
    /// still parses, defaults fill in, and the declared version is left
    /// untouched (CONVENTIONS §3.3 — no silent rewrites).
    #[test]
    fn v2_config_parses_with_default_sessions() {
        let v2 = "\
{
  \"config_version\": 2,
  \"daemon_data_dir\": \".hamlet-data/sled\",
  \"podman_bin\": \"podman\",
  \"base_image\": \"docker.io/library/alpine:3.20\",
  \"web_listen_addr\": \"127.0.0.1:8787\",
  \"log_level\": \"info\",
  \"dispatch\": {
    \"enabled\": false,
    \"timeout_secs\": 30
  },
  \"resources\": {
    \"memory\": \"512m\",
    \"cpus\": \"1.0\",
    \"pids_limit\": 256
  }
}
";
        let parsed = Config::parse(v2).expect("v2 must still parse");
        assert_eq!(parsed.config_version, 2);
        assert_eq!(parsed.sessions, SessionsConfig::default());
        assert_eq!(parsed.resources, ResourcesConfig::default());
    }

    /// v3 → v4 migration is additive: a v3 document (a `sessions` block
    /// without `latency_spike_ms`) still parses and the threshold takes its
    /// default (CONVENTIONS §3 r3 — no silent rewrites).
    #[test]
    fn v3_config_parses_with_default_latency_spike_threshold() {
        let v3 = "\
{
  \"config_version\": 3,
  \"daemon_data_dir\": \".hamlet-data/sled\",
  \"podman_bin\": \"podman\",
  \"base_image\": \"docker.io/library/alpine:3.20\",
  \"web_listen_addr\": \"127.0.0.1:8787\",
  \"log_level\": \"info\",
  \"dispatch\": {
    \"enabled\": false,
    \"timeout_secs\": 30
  },
  \"sessions\": {
    \"ready_timeout_ms\": 30000,
    \"stop_grace_ms\": 10000,
    \"queue_depth\": 64,
    \"continue_on_restart\": true
  }
}
";
        let parsed = Config::parse(v3).expect("v3 must still parse");
        assert_eq!(parsed.config_version, 3);
        assert_eq!(parsed.sessions.latency_spike_ms, 30_000);
        assert_eq!(parsed.sessions.queue_depth, 64);
    }

    /// v4 → v5 migration is additive: a v4 document (a `dispatch` block
    /// without the ADR-0020 policy keys) still parses; the retry policy
    /// takes its defaults (CONVENTIONS §3 r3 — no silent rewrites).
    #[test]
    fn v4_config_parses_with_default_dispatch_policy() {
        let v4 = "\
{
  \"config_version\": 4,
  \"daemon_data_dir\": \".hamlet-data/sled\",
  \"podman_bin\": \"podman\",
  \"base_image\": \"docker.io/library/alpine:3.20\",
  \"web_listen_addr\": \"127.0.0.1:8787\",
  \"log_level\": \"info\",
  \"dispatch\": {
    \"enabled\": false,
    \"timeout_secs\": 30
  }
}
";
        let parsed = Config::parse(v4).expect("v4 must still parse");
        assert_eq!(parsed.config_version, 4);
        assert_eq!(parsed.dispatch, DispatchConfig::default());
        // ADR-0020 cl. 4 golden schedule falls out of the defaults:
        assert_eq!(
            parsed.dispatch.retry_policy().schedule(),
            vec![5_000, 10_000, 20_000, 40_000]
        );
    }

    /// v5 → v6 migration is additive: a v5 document (no `webhooks` block)
    /// still parses; the inbound-webhook surface takes its deny-all
    /// defaults (CONVENTIONS §3 r3 — no silent rewrites).
    #[test]
    fn v5_config_parses_with_default_webhooks() {
        let v5 = "\
{
  \"config_version\": 5,
  \"daemon_data_dir\": \".hamlet-data/sled\",
  \"podman_bin\": \"podman\",
  \"base_image\": \"docker.io/library/alpine:3.20\",
  \"web_listen_addr\": \"127.0.0.1:8787\",
  \"log_level\": \"info\",
  \"dispatch\": {
    \"enabled\": false,
    \"timeout_secs\": 30
  }
}
";
        let parsed = Config::parse(v5).expect("v5 must still parse");
        assert_eq!(parsed.config_version, 5);
        assert_eq!(parsed.webhooks, WebhooksConfig::default());
        assert!(!parsed.webhooks.enabled); // default posture: closed
        assert!(parsed.webhooks.routes.is_empty()); // deny-all routing
    }

    /// Webhook routing-table validation (ADR-0022 cl. 1–3, 5).
    #[test]
    fn webhooks_config_validation() {
        let base = WebhooksConfig::default();
        assert!(base.validate().is_ok());

        let bad_key = WebhooksConfig {
            auth_env_key: "lower_case".to_string(),
            ..base.clone()
        };
        assert!(bad_key.validate().is_err());

        for bad_cap in [0, crate::dispatch::MAX_OUTBOX_BYTES as u64 + 1] {
            let capped = WebhooksConfig {
                max_body_bytes: bad_cap,
                ..base.clone()
            };
            assert!(capped.validate().is_err(), "cap {bad_cap} must fail");
        }

        // Topic names are §4-sanitizer-checked (they become file name
        // components and mail subject fragments).
        for bad_topic in ["../etc", "a/b", "", "-lead", "spaced topic"] {
            let mut routes = BTreeMap::new();
            routes.insert(
                bad_topic.to_string(),
                WebhookRoute {
                    dispatch_inbox: true,
                    mail_to: vec![],
                },
            );
            let routed = WebhooksConfig {
                routes,
                ..base.clone()
            };
            assert!(
                routed.validate().is_err(),
                "topic {bad_topic:?} must be rejected"
            );
        }

        // A route with no sink is a config error.
        let mut routes = BTreeMap::new();
        routes.insert(
            "deploy".to_string(),
            WebhookRoute {
                dispatch_inbox: false,
                mail_to: vec![],
            },
        );
        assert!(WebhooksConfig {
            routes,
            ..base.clone()
        }
        .validate()
        .is_err());

        // mail_to: slot uids only, no duplicates.
        let mut routes = BTreeMap::new();
        routes.insert(
            "deploy".to_string(),
            WebhookRoute {
                dispatch_inbox: false,
                mail_to: vec![4242],
            },
        );
        assert!(WebhooksConfig {
            routes,
            ..base.clone()
        }
        .validate()
        .is_err());
        let mut routes = BTreeMap::new();
        routes.insert(
            "deploy".to_string(),
            WebhookRoute {
                dispatch_inbox: false,
                mail_to: vec![1001, 1001],
            },
        );
        assert!(WebhooksConfig {
            routes,
            ..base.clone()
        }
        .validate()
        .is_err());

        // A valid mixed routing table passes.
        let mut routes = BTreeMap::new();
        routes.insert(
            "deploy.done".to_string(),
            WebhookRoute {
                dispatch_inbox: true,
                mail_to: vec![],
            },
        );
        routes.insert(
            "audit".to_string(),
            WebhookRoute {
                dispatch_inbox: true,
                mail_to: vec![1000, 1002],
            },
        );
        let ok = WebhooksConfig {
            enabled: true,
            routes,
            ..base
        };
        ok.validate().unwrap();
    }

    /// Dispatch policy validation (ADR-0020 cl. 3–4): malformed allowlist
    /// entries and out-of-range retry values are config errors.
    #[test]
    fn dispatch_config_validation() {
        let base = DispatchConfig::default();
        assert!(base.validate().is_ok());
        assert!(base.url_allowlist.is_empty()); // deny-all default posture

        let allowed = DispatchConfig {
            url_allowlist: vec!["https://hooks.example.invalid/v1".to_owned()],
            ..base.clone()
        };
        assert!(allowed.validate().is_ok());

        let http_entry = DispatchConfig {
            url_allowlist: vec!["http://hooks.example.invalid".to_owned()],
            ..base.clone()
        };
        assert!(matches!(
            http_entry.validate(),
            Err(ConfigError::Invalid(_))
        ));

        for bad in [
            DispatchConfig {
                max_attempts: 0,
                ..base.clone()
            },
            DispatchConfig {
                max_attempts: 17,
                ..base.clone()
            },
            DispatchConfig {
                backoff_initial_ms: 1,
                ..base.clone()
            },
            DispatchConfig {
                backoff_max_ms: 100,
                ..base.clone()
            }, // < initial
        ] {
            assert!(
                matches!(bad.validate(), Err(ConfigError::Invalid(_))),
                "{:?} must be rejected",
                bad
            );
        }
    }

    #[test]
    fn sessions_validation_matrix() {
        let bad = [
            SessionsConfig {
                provider: Some("-rf".to_string()),
                ..Default::default()
            },
            SessionsConfig {
                provider: Some("with space".to_string()),
                ..Default::default()
            },
            SessionsConfig {
                provider: Some("".to_string()),
                ..Default::default()
            },
            SessionsConfig {
                model: Some("bad\nmodel".to_string()),
                ..Default::default()
            },
            SessionsConfig {
                ready_timeout_ms: 999,
                ..Default::default()
            },
            SessionsConfig {
                ready_timeout_ms: 600_001,
                ..Default::default()
            },
            SessionsConfig {
                stop_grace_ms: 499,
                ..Default::default()
            },
            SessionsConfig {
                stop_grace_ms: 300_001,
                ..Default::default()
            },
            SessionsConfig {
                queue_depth: 0,
                ..Default::default()
            },
            SessionsConfig {
                queue_depth: 10_001,
                ..Default::default()
            },
            SessionsConfig {
                restart: RestartConfig {
                    max_restarts: 101,
                    ..Default::default()
                },
                ..Default::default()
            },
            SessionsConfig {
                restart: RestartConfig {
                    backoff_initial_ms: 99,
                    ..Default::default()
                },
                ..Default::default()
            },
            SessionsConfig {
                restart: RestartConfig {
                    backoff_max_ms: 999, // < initial (1000)
                    ..Default::default()
                },
                ..Default::default()
            },
            SessionsConfig {
                restart: RestartConfig {
                    backoff_max_ms: 3_600_001,
                    ..Default::default()
                },
                ..Default::default()
            },
            SessionsConfig {
                latency_spike_ms: 0,
                ..Default::default()
            },
            SessionsConfig {
                latency_spike_ms: 3_600_001,
                ..Default::default()
            },
        ];
        for cfg in bad {
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
                "must reject: {cfg:?}"
            );
        }
        let good = [
            SessionsConfig::default(),
            SessionsConfig {
                provider: Some("anthropic".to_string()),
                model: Some("anthropic/claude-sonnet-4-5".to_string()),
                ..Default::default()
            },
            SessionsConfig {
                restart: RestartConfig {
                    max_restarts: 0, // auto-restart disabled
                    ..Default::default()
                },
                ..Default::default()
            },
            SessionsConfig {
                latency_spike_ms: 1, // tightest legal threshold
                ..Default::default()
            },
        ];
        for cfg in good {
            assert!(cfg.validate().is_ok(), "must accept: {cfg:?}");
        }
    }

    #[test]
    fn restart_backoff_delegates_to_session_schedule() {
        let restart = RestartConfig::default();
        assert_eq!(restart.backoff_ms(0), Some(1_000));
        assert_eq!(restart.backoff_ms(4), Some(16_000));
        assert_eq!(restart.backoff_ms(5), None); // budget (5) exhausted
                                                 // Cap honored.
        let capped = RestartConfig {
            backoff_initial_ms: 1_000,
            backoff_max_ms: 3_000,
            max_restarts: 10,
        };
        assert_eq!(capped.backoff_ms(3), Some(3_000));
    }

    #[test]
    fn resources_validation_rejects_bad_values() {
        for bad in ["", "x", "-512m", "512 m", "512mb", "1;rm"] {
            let cfg = Config {
                resources: ResourcesConfig {
                    memory: bad.to_string(),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
                "memory {bad:?} must be rejected"
            );
        }
        for bad in ["", "one", "1.0.0", "--cpus", "50pct"] {
            let cfg = Config {
                resources: ResourcesConfig {
                    cpus: bad.to_string(),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
                "cpus {bad:?} must be rejected"
            );
        }
        for bad in [0u64, 65536, u64::MAX] {
            let cfg = Config {
                resources: ResourcesConfig {
                    pids_limit: bad,
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
                "pids_limit {bad} must be rejected"
            );
        }
        for memory in ["512m", "2g", "1024"] {
            for cpus in ["1", "1.0", "50%"] {
                let cfg = Config {
                    resources: ResourcesConfig {
                        memory: memory.to_string(),
                        cpus: cpus.to_string(),
                        pids_limit: 256,
                    },
                    ..Default::default()
                };
                assert!(cfg.validate().is_ok(), "{memory}/{cpus} must pass");
            }
        }
    }

    #[test]
    fn default_config_roundtrips_through_parse() {
        let rendered = Config::default().render();
        assert_eq!(Config::parse(&rendered).unwrap(), Config::default());
    }

    #[test]
    fn parse_rejects_bad_version() {
        for version in [0, CONFIG_VERSION + 1, u32::MAX] {
            let cfg = Config {
                config_version: version,
                ..Default::default()
            };
            assert_eq!(
                Config::parse(&cfg.render()),
                Err(ConfigError::UnsupportedVersion(version))
            );
        }
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        let text = Config::default().render().replace(
            "\"log_level\": \"info\",",
            "\"log_level\": \"info\",\n  \"surprise\": true,",
        );
        assert!(matches!(Config::parse(&text), Err(ConfigError::Syntax(_))));
    }

    #[test]
    fn validate_rejects_bad_fields() {
        let bad = [
            Config {
                daemon_data_dir: PathBuf::new(),
                ..Default::default()
            },
            Config {
                podman_bin: PathBuf::new(),
                ..Default::default()
            },
            Config {
                base_image: "alpine 3".to_string(),
                ..Default::default()
            },
            Config {
                web_listen_addr: "not-an-addr".to_string(),
                ..Default::default()
            },
            Config {
                dispatch: DispatchConfig {
                    enabled: false,
                    timeout_secs: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
            Config {
                dispatch: DispatchConfig {
                    enabled: false,
                    timeout_secs: 3601,
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        for cfg in bad {
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
                "must reject: {cfg:?}"
            );
        }
    }

    #[test]
    fn validate_accepts_socket_addr_forms() {
        for addr in ["127.0.0.1:8787", "[::1]:9000", "0.0.0.0:1"] {
            let cfg = Config {
                web_listen_addr: addr.to_string(),
                ..Default::default()
            };
            assert_eq!(cfg.validate(), Ok(()), "addr {addr} must be accepted");
        }
    }

    #[test]
    fn resolve_daemon_data_dir() {
        let cfg = Config::default();
        // Default resolves OUTSIDE the village tree (ADR-0005): sibling
        // `.hamlet-data/` in the working directory, never inside `.hamlet/`.
        let resolved = cfg.resolve_daemon_data_dir(Path::new("/w/.hamlet"));
        assert_eq!(resolved, PathBuf::from("/w/.hamlet-data/sled"));
        assert!(!resolved.starts_with("/w/.hamlet"));
        let cfg = Config {
            daemon_data_dir: PathBuf::from("/var/lib/hamlet"),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_daemon_data_dir(Path::new("/w/.hamlet")),
            PathBuf::from("/var/lib/hamlet")
        );
    }

    /// The committed init template must parse (via dotenvy) to *empty*
    /// secrets: every entry is commented out, every value an obvious fake.
    #[test]
    fn env_template_parses_to_empty_secrets() {
        let secrets = Secrets::from_read(ENV_TEMPLATE.as_bytes()).unwrap();
        assert!(secrets.is_empty());
        assert_eq!(secrets, Secrets::default());
    }

    #[test]
    fn env_template_holds_no_real_looking_values() {
        for line in ENV_TEMPLATE
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
        {
            assert!(
                line.trim().is_empty(),
                "template must be all comments: {line:?}"
            );
        }
        assert!(ENV_TEMPLATE.contains("test-key-do-not-use"));
    }

    #[test]
    fn secrets_from_read_collects_typed_vars() {
        let env = "\
HAMLET_MODEL_ENDPOINT=https://model.example.invalid/v1
HAMLET_MODEL_API_KEY=test-key-do-not-use
HAMLET_API_KEY_OPENAI=test-key-do-not-use
HAMLET_API_KEY_ANTHROPIC=test-key-do-not-use-2
UNRELATED=nope
HAMLET_API_KEY_=dangling-prefix-ignored
HAMLET_MODEL_API_KEY=
";
        let secrets = Secrets::from_read(env.as_bytes()).unwrap();
        assert_eq!(
            secrets.model_endpoint.as_ref().unwrap().expose(),
            "https://model.example.invalid/v1"
        );
        assert_eq!(
            secrets.model_api_key.as_ref().unwrap().expose(),
            "test-key-do-not-use"
        );
        let providers: Vec<(&str, &str)> = secrets
            .provider_keys
            .iter()
            .map(|(k, v)| (k.as_str(), v.expose()))
            .collect();
        assert_eq!(
            providers,
            vec![
                ("ANTHROPIC", "test-key-do-not-use-2"),
                ("OPENAI", "test-key-do-not-use")
            ]
        );
        assert!(!secrets.is_empty());
    }

    /// CONVENTIONS §6: secrets must never reach logs. The `Debug` impls
    /// redact every value; there is no `Serialize` impl to misuse.
    #[test]
    fn secrets_debug_redacts_values() {
        let secrets = Secrets::from_vars(vec![
            ("HAMLET_MODEL_ENDPOINT", "https://model.example.invalid/v1"),
            ("HAMLET_MODEL_API_KEY", "test-key-do-not-use"),
            ("HAMLET_API_KEY_OPENAI", "test-key-do-not-use-2"),
        ]);
        let dumped = format!("{secrets:?}");
        assert!(!dumped.contains("test-key-do-not-use"), "{dumped}");
        assert!(!dumped.contains("model.example.invalid"), "{dumped}");
        assert!(dumped.contains("***"), "{dumped}");
        // Provider *names* are not secret and stay visible for diagnostics.
        assert!(dumped.contains("OPENAI"), "{dumped}");
    }

    #[test]
    fn secrets_from_read_rejects_malformed_env() {
        // Unbalanced quote → dotenvy syntax error, message carries no value.
        let err = Secrets::from_read("HAMLET_MODEL_API_KEY=\"unterminated\n".as_bytes())
            .expect_err("malformed .env must fail");
        assert!(matches!(err, SecretsError::Parse(_)));
    }
}
