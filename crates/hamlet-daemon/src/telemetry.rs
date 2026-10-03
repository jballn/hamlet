//! sled telemetry sink (SPEC §11.2; CONVENTIONS §2 file-as-truth; ADR-0005).
//!
//! Captures token tallies, API costs, latency spikes, and system event
//! traces into an embedded sled db — **out-of-band, never in charter.json
//! and never orchestration state**. The db lives under `daemon_data_dir`
//! from `config.json`, resolved *outside* the `.hamlet/` tree
//! ([`Telemetry::open_for_village`] rejects any location inside it); the
//! durable record is the JSON summary exported into `keep/archives/` at
//! commission close, so sled itself may be pruned (ADR-0005 retention).
//!
//! # Key layout (ADR-0005)
//!
//! Five versioned namespaces (sled trees): `tokens.v1`, `costs.v1`,
//! `latency.v1`, `traces.v1`, `counters.v1`. Keys are fixed-width
//! big-endian concatenations — `commission: u64 | uid: u32 | session: u64`
//! (20 bytes), agent-scoped keys drop `session` (12 bytes), append-only
//! records append a `seq: u64` — so byte-ordered sled scans iterate
//! commission → uid → session → seq and prefix scans need no delimiters.
//! Values are canonical JSON records (`serde`, `deny_unknown_fields`)
//! except raw `u64` big-endian counters (costs, seq allocation).
//!
//! # Clock policy (CONVENTIONS §3 rule 4)
//!
//! This store never reads the wall clock. Every timestamp is a
//! caller-supplied `*_unix_ms` field on the record; summaries and exports
//! are therefore deterministic and golden-testable.
//!
//! # Boundary
//!
//! Only telemetry trees exist; the allowlist is [`TELEMETRY_TREES`] and the
//! test suite fails if any other tree — or any charter-shaped key/value —
//! appears in the db. Slots, commissions, decrees, parcels: charter.json
//! only, never here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use hamlet_core::config::Config;
use hamlet_core::tree::HamletTree;

/// Namespace: cumulative token tallies per agent session.
/// Key = [`SessionKey`] (20 B), value = JSON [`TokenTally`].
pub const TREE_TOKENS: &str = "tokens.v1";
/// Namespace: cumulative API cost per agent session.
/// Key = [`SessionKey`] (20 B), value = `u64` BE micro-USD.
pub const TREE_COSTS: &str = "costs.v1";
/// Namespace: append-only latency spike records.
/// Key = [`SessionKey`] + `seq: u64` BE (28 B), value = JSON
/// [`LatencySpike`].
pub const TREE_LATENCY: &str = "latency.v1";
/// Namespace: append-only system event traces.
/// Key = [`AgentKey`] + `seq: u64` BE (20 B), value = JSON [`TraceRecord`].
pub const TREE_TRACES: &str = "traces.v1";
/// Namespace: monotonic sequence allocators for the append-only trees.
/// Key = domain `u8` + [`AgentKey`] (13 B), value = `u64` BE.
const TREE_COUNTERS: &str = "counters.v1";

/// The complete allowlist of sled trees this store may create. Anything
/// outside this list is a boundary violation (no orchestration state in
/// sled — CONVENTIONS §2) and fails the test suite.
pub const TELEMETRY_TREES: [&str; 5] = [
    TREE_TOKENS,
    TREE_COSTS,
    TREE_LATENCY,
    TREE_TRACES,
    TREE_COUNTERS,
];

/// Counter domain: trace sequence allocation ([`TREE_TRACES`]).
const DOMAIN_TRACE_SEQ: u8 = 1;
/// Counter domain: latency spike sequence allocation ([`TREE_LATENCY`]).
const DOMAIN_LATENCY_SEQ: u8 = 2;
/// Counter domain: session correlation-id sequence allocation (ADR-0015
/// cl. 2: `<kind>-<seq>-<uid>.<inc>` seqs come from sled counters —
/// additive use of the `counters.v1` key layout, ADR-0005 cl. 3).
const DOMAIN_CORRELATION_SEQ: u8 = 3;
/// Counter domain: mail spool sequence allocation (ADR-0019 cl. 2 — the
/// `{seq:012}` writer FIFO prefix of daemon-composed spool file names).
/// Keyed by writer uid with commission 0; durable across daemon restarts
/// so per-writer lexicographic FIFO never inverts on a seq reset.
const DOMAIN_SPOOL_SEQ: u8 = 4;

/// Schema version of exported archive summaries (`keep/archives/`).
pub const SUMMARY_VERSION: u32 = 1;

// --- keys -------------------------------------------------------------------

/// Agent scope: one commissioned identity. Encodes as 12 fixed-width
/// big-endian bytes (`commission | uid`) — a prefix of [`SessionKey`], so
/// agent-scoped scans share the same ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentKey {
    /// Commission id (`charter.json` `commissions` key — telemetry only
    /// references it, never stores its state).
    pub commission: u64,
    /// Village UID slot (SPEC §4: 1000 Mayor, 1001–2000 agents).
    pub uid: u32,
}

/// Encoded key width of [`AgentKey`].
pub const AGENT_KEY_LEN: usize = 12;

impl AgentKey {
    /// Fixed-width big-endian encoding (ADR-0005).
    #[must_use]
    pub fn encode(self) -> [u8; AGENT_KEY_LEN] {
        let mut out = [0u8; AGENT_KEY_LEN];
        out[..8].copy_from_slice(&self.commission.to_be_bytes());
        out[8..].copy_from_slice(&self.uid.to_be_bytes());
        out
    }

    fn decode(bytes: &[u8], tree: &str) -> Result<Self, TelemetryError> {
        if bytes.len() < AGENT_KEY_LEN {
            return Err(corrupt(
                tree,
                format!("key shorter than {AGENT_KEY_LEN} bytes"),
            ));
        }
        Ok(Self {
            commission: u64::from_be_bytes(bytes[..8].try_into().expect("8-byte slice")),
            uid: u32::from_be_bytes(bytes[8..AGENT_KEY_LEN].try_into().expect("4-byte slice")),
        })
    }
}

/// Session scope: one agent session inside a commission. Encodes as 20
/// fixed-width big-endian bytes (`commission | uid | session`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionKey {
    /// Agent scope this session belongs to.
    pub agent: AgentKey,
    /// Session id (daemon-allocated, monotonic per agent; telemetry only).
    pub session: u64,
}

/// Encoded key width of [`SessionKey`].
pub const SESSION_KEY_LEN: usize = 20;

impl SessionKey {
    /// Convenience constructor.
    #[must_use]
    pub const fn new(commission: u64, uid: u32, session: u64) -> Self {
        Self {
            agent: AgentKey { commission, uid },
            session,
        }
    }

    /// Fixed-width big-endian encoding (ADR-0005).
    #[must_use]
    pub fn encode(self) -> [u8; SESSION_KEY_LEN] {
        let mut out = [0u8; SESSION_KEY_LEN];
        out[..AGENT_KEY_LEN].copy_from_slice(&self.agent.encode());
        out[AGENT_KEY_LEN..].copy_from_slice(&self.session.to_be_bytes());
        out
    }

    fn decode(bytes: &[u8], tree: &str) -> Result<Self, TelemetryError> {
        if bytes.len() < SESSION_KEY_LEN {
            return Err(corrupt(
                tree,
                format!("key shorter than {SESSION_KEY_LEN} bytes"),
            ));
        }
        Ok(Self {
            agent: AgentKey::decode(bytes, tree)?,
            session: u64::from_be_bytes(
                bytes[AGENT_KEY_LEN..SESSION_KEY_LEN]
                    .try_into()
                    .expect("8-byte slice"),
            ),
        })
    }
}

// --- records ----------------------------------------------------------------

/// Cumulative token tally (and increment unit — [`Telemetry::add_tokens`]
/// accumulates deltas field-wise).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenTally {
    /// Prompt/input tokens.
    pub input_tokens: u64,
    /// Completion/output tokens.
    pub output_tokens: u64,
}

/// One latency spike observation (append-only, `latency.v1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatencySpike {
    /// Observation time, caller-supplied (CONVENTIONS §3 rule 4).
    pub at_unix_ms: u64,
    /// Observed duration in milliseconds.
    pub duration_ms: u64,
    /// What was slow (e.g. `mail-delivery`, `podman-exec`). Internal
    /// vocabulary — never used as a filesystem name.
    pub operation: String,
}

/// One system event trace (append-only, `traces.v1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceRecord {
    /// Event time, caller-supplied (CONVENTIONS §3 rule 4).
    pub at_unix_ms: u64,
    /// Event name from the daemon's internal vocabulary.
    pub event: String,
    /// Free-form detail (no secrets — CONVENTIONS §6).
    pub detail: String,
}

// --- live query surface -----------------------------------------------------

/// Scope of a [`Telemetry::metrics_for`] query. `Session`, `Agent` and
/// `Commission` are pure prefix scans; `Uid` crosses commissions and
/// therefore walks the telemetry trees in full — prefer `Agent` whenever the
/// commission is known (the CLI resolves it from the charter slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsScope {
    /// One session of one agent.
    Session(SessionKey),
    /// One commissioned identity — all of its sessions.
    Agent(AgentKey),
    /// Every agent in one commission.
    Commission(u64),
    /// One UID slot across every commission that recorded it.
    Uid(u32),
}

impl MetricsScope {
    /// The scan bounds this scope reduces to.
    fn scan_bounds(self) -> ScanBounds {
        match self {
            Self::Session(key) => ScanBounds {
                prefix: key.agent.encode().to_vec(),
                uid: Some(key.agent.uid),
                session: Some(key.session),
            },
            Self::Agent(agent) => ScanBounds {
                prefix: agent.encode().to_vec(),
                uid: Some(agent.uid),
                session: None,
            },
            Self::Commission(commission) => ScanBounds {
                prefix: commission.to_be_bytes().to_vec(),
                uid: None,
                session: None,
            },
            Self::Uid(uid) => ScanBounds {
                prefix: Vec::new(),
                uid: Some(uid),
                session: None,
            },
        }
    }
}

/// A resolved [`MetricsScope`]: the sled key prefix to scan plus the
/// filters the prefix cannot express (a `Uid` scope is not a prefix, and a
/// `Session` scope narrows below the agent prefix).
struct ScanBounds {
    prefix: Vec<u8>,
    uid: Option<u32>,
    session: Option<u64>,
}

impl ScanBounds {
    fn matches(&self, key: SessionKey) -> bool {
        self.matches_agent(key.agent) && self.session.is_none_or(|want| want == key.session)
    }

    fn matches_agent(&self, key: AgentKey) -> bool {
        self.uid.is_none_or(|want| want == key.uid)
    }
}

/// One session's rolled-up metrics ([`SessionKey`] flattened so the row is
/// directly renderable by the CLI/WS stages).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTotals {
    /// Commission the session belongs to.
    pub commission: u64,
    /// UID slot.
    pub uid: u32,
    /// Session id (`SessionId::as_u64`: `incarnation << 32 | uid`).
    pub session: u64,
    /// Token tally.
    pub tokens: TokenTally,
    /// Accumulated API cost in micro-USD.
    pub cost_micro_usd: u64,
    /// Latency spikes recorded against this session.
    pub latency_spikes: u64,
}

impl From<SessionKey> for SessionTotals {
    fn from(key: SessionKey) -> Self {
        Self {
            commission: key.agent.commission,
            uid: key.agent.uid,
            session: key.session,
            tokens: TokenTally::default(),
            cost_micro_usd: 0,
            latency_spikes: 0,
        }
    }
}

impl SessionTotals {
    /// Does this row describe `key`?
    #[must_use]
    pub fn matches(&self, key: SessionKey) -> bool {
        self.commission == key.agent.commission
            && self.uid == key.agent.uid
            && self.session == key.session
    }
}

/// One agent's rollup inside a [`MetricsSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRow {
    /// Commission (the lowest one when a uid spans several — `Uid` scope).
    pub commission: u64,
    /// UID slot.
    pub uid: u32,
    /// Rollup totals.
    pub totals: AgentTotals,
}

/// A live metrics snapshot — the read model the CLI (Sprint 08) and the WS
/// metric streams (Sprint 09) render. Serializable, deterministic and
/// clock-free (CONVENTIONS §3 r4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsSnapshot {
    /// Rollup over the whole scope.
    pub totals: AgentTotals,
    /// Per-session rows, ascending.
    pub sessions: Vec<SessionTotals>,
    /// Per-agent rows, ascending by uid.
    pub agents: Vec<AgentRow>,
    /// The most recent spikes in scope (key order).
    pub recent_spikes: Vec<LatencySpike>,
    /// The most recent traces in scope (key order).
    pub recent_traces: Vec<TraceRecord>,
}

// --- summaries --------------------------------------------------------------

/// Per-agent rollup (sums over all of an agent's sessions).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTotals {
    /// Total prompt/input tokens.
    pub input_tokens: u64,
    /// Total completion/output tokens.
    pub output_tokens: u64,
    /// Total API cost in micro-USD (1e-6 USD; integer math only).
    pub cost_micro_usd: u64,
    /// Number of recorded latency spikes.
    pub latency_spikes: u64,
    /// Number of recorded trace events.
    pub traces: u64,
}

impl AgentTotals {
    /// Fold one agent's totals into another (used for rollups).
    fn accumulate(&mut self, other: &Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cost_micro_usd += other.cost_micro_usd;
        self.latency_spikes += other.latency_spikes;
        self.traces += other.traces;
    }
}

/// Per-commission rollup over all participating agents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommissionTotals {
    /// Number of agents with any telemetry in this commission.
    pub agents: u64,
    /// Summed [`AgentTotals::input_tokens`].
    pub input_tokens: u64,
    /// Summed [`AgentTotals::output_tokens`].
    pub output_tokens: u64,
    /// Summed [`AgentTotals::cost_micro_usd`].
    pub cost_micro_usd: u64,
    /// Summed [`AgentTotals::latency_spikes`].
    pub latency_spikes: u64,
    /// Summed [`AgentTotals::traces`].
    pub traces: u64,
}

/// Full per-agent detail for the archive export (the durable record —
/// ADR-0005 retention: after export the sled data may be pruned).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSummary {
    /// Rollup totals.
    pub totals: AgentTotals,
    /// Every latency spike, ascending by seq.
    pub latency_spikes: Vec<LatencySpike>,
    /// Every trace event, ascending by seq.
    pub traces: Vec<TraceRecord>,
}

/// Archive summary of one commission — the JSON document exported into
/// `keep/archives/` at commission close (SPEC §11.2; wired in Sprint 06).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommissionSummary {
    /// Schema version of this document.
    pub summary_version: u32,
    /// Commission id.
    pub commission: u64,
    /// Export time, caller-supplied (CONVENTIONS §3 rule 4).
    pub generated_at_unix_ms: u64,
    /// Commission-wide rollup.
    pub totals: CommissionTotals,
    /// Per-agent detail keyed by UID (sorted — canonical JSON).
    pub agents: BTreeMap<u32, AgentSummary>,
}

impl CommissionSummary {
    /// Canonical rendering: pretty JSON, trailing newline. Deterministic —
    /// `BTreeMap` agents sorted by uid, struct fields in declaration order,
    /// no wall-clock input.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = serde_json::to_string_pretty(self)
            .expect("plain-struct CommissionSummary serialization is infallible");
        out.push('\n');
        out
    }

    /// Archive file name for a commission summary. Derived from the
    /// commission id (`u64`) only — no external strings, no sanitizer
    /// surface (ADR-0002 applies to externally-derived names).
    #[must_use]
    pub fn archive_file_name(commission: u64) -> String {
        format!("commission-{commission}-telemetry.json")
    }
}

// --- errors -----------------------------------------------------------------

/// Telemetry store failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryError {
    /// Underlying sled failure (open/insert/scan).
    Storage(String),
    /// Filesystem failure during archive export (message names the step).
    Io(String),
    /// A stored key/value did not decode. Fail-hard — telemetry is never
    /// silently repaired or skipped (house rule, ADR-0004 precedent).
    Corrupt {
        /// Tree the bad record was found in.
        tree: String,
        /// What failed to decode.
        detail: String,
    },
    /// The configured daemon data dir resolves inside the `.hamlet/` tree.
    /// Telemetry is out-of-band; the village tree is human-readable
    /// file-as-truth (ADR-0005).
    InsideVillageTree(PathBuf),
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(msg) => write!(f, "telemetry storage error: {msg}"),
            Self::Io(msg) => write!(f, "telemetry I/O error: {msg}"),
            Self::Corrupt { tree, detail } => {
                write!(f, "telemetry corruption in tree {tree}: {detail}")
            }
            Self::InsideVillageTree(path) => write!(
                f,
                "daemon data dir {} is inside the .hamlet/ tree — telemetry must be out-of-band (ADR-0005)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for TelemetryError {}

impl From<sled::Error> for TelemetryError {
    fn from(err: sled::Error) -> Self {
        Self::Storage(err.to_string())
    }
}

fn corrupt(tree: &str, detail: impl Into<String>) -> TelemetryError {
    TelemetryError::Corrupt {
        tree: tree.to_string(),
        detail: detail.into(),
    }
}

// --- store ------------------------------------------------------------------

/// Embedded sled telemetry store (SPEC §11.2). Single-daemon-owned like
/// every runtime resource (CONVENTIONS §2 singular governor); cheap to
/// query, never a source of orchestration truth.
#[derive(Debug)]
pub struct Telemetry {
    db: sled::Db,
    data_dir: PathBuf,
}

impl Telemetry {
    /// Open (or create) the sled db at `data_dir`. The directory and its
    /// parents are created by sled. Callers must ensure `data_dir` is
    /// outside any `.hamlet/` tree; [`Telemetry::open_for_village`] does
    /// that checking for config-driven opens.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] if sled cannot open the db.
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self, TelemetryError> {
        let data_dir = data_dir.into();
        let db = sled::open(&data_dir)?;
        Ok(Self { db, data_dir })
    }

    /// Open the telemetry db for a village: resolves
    /// [`Config::daemon_data_dir`] against `village_root` (`.hamlet/`) and
    /// **rejects any location inside the tree** — telemetry is out-of-band
    /// (ADR-0005).
    ///
    /// # Errors
    ///
    /// [`TelemetryError::InsideVillageTree`] or [`TelemetryError::Storage`].
    pub fn open_for_village(village_root: &Path, config: &Config) -> Result<Self, TelemetryError> {
        let data_dir = config.resolve_daemon_data_dir(village_root);
        if data_dir.starts_with(village_root) {
            return Err(TelemetryError::InsideVillageTree(data_dir));
        }
        Self::open(data_dir)
    }

    /// The directory the sled db lives in.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Flush all buffered writes to stable storage. The daemon's durability
    /// checkpoint: after `flush`, recorded telemetry survives `kill -9`
    /// (see the kill/restart test).
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] on sled I/O failure.
    pub fn flush(&self) -> Result<(), TelemetryError> {
        self.db.flush()?;
        Ok(())
    }

    // --- recording (caller-supplied timestamps only) ---

    /// Accumulate a token-usage delta onto one agent session.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`] if a
    /// stored tally no longer decodes (fail-hard, never silent repair).
    pub fn add_tokens(&self, session: SessionKey, delta: TokenTally) -> Result<(), TelemetryError> {
        let tree = self.db.open_tree(TREE_TOKENS)?;
        let key = session.encode();
        // Typed fail-hard pre-check: corruption surfaces as Corrupt, and
        // justifies the expect inside the CAS closure below (any value the
        // loop re-reads was written by this same validated path).
        if let Some(existing) = tree.get(key)? {
            decode_json::<TokenTally>(TREE_TOKENS, &existing)?;
        }
        tree.fetch_and_update(key, |old: Option<&[u8]>| {
            let mut tally = old.map_or(TokenTally::default(), |bytes| {
                decode_json::<TokenTally>(TREE_TOKENS, bytes)
                    .expect("stored tally validated by the pre-check above")
            });
            tally.input_tokens += delta.input_tokens;
            tally.output_tokens += delta.output_tokens;
            Some(encode_json(&tally))
        })?;
        Ok(())
    }

    /// Accumulate API cost (micro-USD, integer math only) onto one agent
    /// session.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn add_cost_micro_usd(
        &self,
        session: SessionKey,
        micro_usd: u64,
    ) -> Result<(), TelemetryError> {
        self.bump_counter(TREE_COSTS, &session.encode(), micro_usd)?;
        Ok(())
    }

    /// Append a latency spike record; returns its assigned sequence number
    /// (1-based, monotonic per agent).
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn record_latency_spike(
        &self,
        session: SessionKey,
        spike: LatencySpike,
    ) -> Result<u64, TelemetryError> {
        let seq = self.next_seq(DOMAIN_LATENCY_SEQ, session.agent)?;
        let tree = self.db.open_tree(TREE_LATENCY)?;
        let mut key = Vec::with_capacity(SESSION_KEY_LEN + 8);
        key.extend_from_slice(&session.encode());
        key.extend_from_slice(&seq.to_be_bytes());
        tree.insert(key, encode_json(&spike))?;
        Ok(seq)
    }

    /// Append a system event trace; returns its assigned sequence number
    /// (1-based, monotonic per agent).
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn trace_event(&self, agent: AgentKey, record: TraceRecord) -> Result<u64, TelemetryError> {
        let seq = self.next_seq(DOMAIN_TRACE_SEQ, agent)?;
        let tree = self.db.open_tree(TREE_TRACES)?;
        let mut key = Vec::with_capacity(AGENT_KEY_LEN + 8);
        key.extend_from_slice(&agent.encode());
        key.extend_from_slice(&seq.to_be_bytes());
        tree.insert(key, encode_json(&record))?;
        Ok(seq)
    }

    fn next_seq(&self, domain: u8, agent: AgentKey) -> Result<u64, TelemetryError> {
        let mut key = Vec::with_capacity(1 + AGENT_KEY_LEN);
        key.push(domain);
        key.extend_from_slice(&agent.encode());
        self.bump_counter(TREE_COUNTERS, &key, 1)
    }

    /// Allocate the next session correlation sequence for one agent
    /// (ADR-0015 cl. 2 — envelope correlation ids `m|t|n|i-<seq>-<uid>.<inc>`).
    /// 1-based, monotonic per agent, durable across daemon restarts.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn next_correlation_seq(&self, agent: AgentKey) -> Result<u64, TelemetryError> {
        self.next_seq(DOMAIN_CORRELATION_SEQ, agent)
    }

    /// Allocate the next mail spool sequence for one writer uid (ADR-0019
    /// cl. 2 spool file-name grammar). 1-based, monotonic per writer,
    /// durable across daemon restarts. Non-slot writers (system notices,
    /// patrons) key under uid 0 — the counter only orders file names, it is
    /// never orchestration state (CONVENTIONS §2).
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn next_spool_seq(&self, writer_uid: u32) -> Result<u64, TelemetryError> {
        self.next_seq(
            DOMAIN_SPOOL_SEQ,
            AgentKey {
                commission: 0,
                uid: writer_uid,
            },
        )
    }

    /// Atomically add `delta` to a raw `u64` BE counter; returns the new
    /// value. Fail-hard: a malformed stored counter is
    /// [`TelemetryError::Corrupt`], never silently re-zeroed.
    fn bump_counter(&self, tree_name: &str, key: &[u8], delta: u64) -> Result<u64, TelemetryError> {
        let tree = self.db.open_tree(tree_name)?;
        // Typed pre-check: surface corruption as Corrupt, and justify the
        // expect inside the CAS closure (re-reads are values this same
        // validated path wrote).
        if let Some(existing) = tree.get(key)? {
            decode_counter(tree_name, &existing)?;
        }
        let updated = std::cell::Cell::new(delta);
        tree.fetch_and_update(key, |old: Option<&[u8]>| {
            let current = old.map_or(0, |bytes| {
                u64::from_be_bytes(bytes.try_into().expect("width pre-checked above"))
            });
            updated.set(current + delta);
            Some(updated.get().to_be_bytes().to_vec())
        })?;
        Ok(updated.get())
    }

    // --- queries ---

    /// Token tally for one session (`None` if it never recorded usage).
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn session_tokens(
        &self,
        session: SessionKey,
    ) -> Result<Option<TokenTally>, TelemetryError> {
        let tree = self.db.open_tree(TREE_TOKENS)?;
        match tree.get(session.encode())? {
            Some(bytes) => Ok(Some(decode_json::<TokenTally>(TREE_TOKENS, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Accumulated cost (micro-USD) for one session; 0 if none recorded.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`].
    pub fn session_cost_micro_usd(&self, session: SessionKey) -> Result<u64, TelemetryError> {
        let tree = self.db.open_tree(TREE_COSTS)?;
        Ok(match tree.get(session.encode())? {
            Some(bytes) => decode_counter(TREE_COSTS, &bytes)?,
            None => 0,
        })
    }

    /// Per-agent totals summed over all of the agent's sessions.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn agent_totals(&self, agent: AgentKey) -> Result<AgentTotals, TelemetryError> {
        let mut totals = AgentTotals {
            traces: self.count_prefix(TREE_TRACES, &agent.encode())?,
            latency_spikes: self.count_prefix(TREE_LATENCY, &agent.encode())?,
            ..AgentTotals::default()
        };
        let tokens = self.db.open_tree(TREE_TOKENS)?;
        for entry in tokens.scan_prefix(agent.encode()) {
            let (key, value) = entry?;
            // Validate the full key shape even when only summing values.
            SessionKey::decode(&key, TREE_TOKENS)?;
            let tally = decode_json::<TokenTally>(TREE_TOKENS, &value)?;
            totals.input_tokens += tally.input_tokens;
            totals.output_tokens += tally.output_tokens;
        }
        let costs = self.db.open_tree(TREE_COSTS)?;
        for entry in costs.scan_prefix(agent.encode()) {
            let (key, value) = entry?;
            SessionKey::decode(&key, TREE_COSTS)?;
            totals.cost_micro_usd += decode_counter(TREE_COSTS, &value)?;
        }
        Ok(totals)
    }

    /// Per-commission totals plus the per-agent breakdown (sorted by uid).
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn commission_totals(
        &self,
        commission: u64,
    ) -> Result<(CommissionTotals, BTreeMap<u32, AgentTotals>), TelemetryError> {
        let mut per_agent = BTreeMap::new();
        for uid in self.commission_uids(commission)? {
            let agent = AgentKey { commission, uid };
            per_agent.insert(uid, self.agent_totals(agent)?);
        }
        let mut rolled = AgentTotals::default();
        for agent in per_agent.values() {
            rolled.accumulate(agent);
        }
        let totals = CommissionTotals {
            agents: u64::try_from(per_agent.len()).expect("usize fits u64 on 64-bit targets"),
            input_tokens: rolled.input_tokens,
            output_tokens: rolled.output_tokens,
            cost_micro_usd: rolled.cost_micro_usd,
            latency_spikes: rolled.latency_spikes,
            traces: rolled.traces,
        };
        Ok((totals, per_agent))
    }

    /// The most recent `window` trace events for one agent, oldest first
    /// (ascending seq). Returns fewer (possibly zero) if the agent traced
    /// less.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn recent_traces(
        &self,
        agent: AgentKey,
        window: usize,
    ) -> Result<Vec<TraceRecord>, TelemetryError> {
        let tree = self.db.open_tree(TREE_TRACES)?;
        let mut all = Vec::new();
        for entry in tree.scan_prefix(agent.encode()) {
            let (key, value) = entry?;
            if key.len() != AGENT_KEY_LEN + 8 {
                return Err(corrupt(TREE_TRACES, "trace key is not agent+seq width"));
            }
            all.push(decode_json::<TraceRecord>(TREE_TRACES, &value)?);
        }
        let start = all.len().saturating_sub(window);
        Ok(all.split_off(start))
    }

    // --- live query surface (Sprint 03 step 04; consumed by the CLI in
    //     Sprint 08 and the WS metric streams in Sprint 09) ---

    /// One live metrics snapshot for a scope: rolled totals, the per-session
    /// and per-agent rows, and the most recent spikes/traces. Pure reads —
    /// no clock, no writes, deterministic ordering (sled's byte order equals
    /// commission → uid → session → seq, ADR-0005 cl. 3), so a snapshot is
    /// golden-testable and safe to serialize straight into a WS frame.
    ///
    /// `window` bounds `recent_spikes` / `recent_traces` (the *last* N in
    /// key order — not wall-clock order across agents).
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn metrics_for(
        &self,
        scope: MetricsScope,
        window: usize,
    ) -> Result<MetricsSnapshot, TelemetryError> {
        let bounds = scope.scan_bounds();
        let sessions = self.collect_sessions(&bounds)?;

        // A session scope is session-exact: no agent rollup (traces are
        // agent-keyed, so an agent row would be broader than the question).
        let agents = match scope {
            MetricsScope::Session(_) => Vec::new(),
            _ => self.collect_agents(&bounds, &sessions)?,
        };

        let totals = match scope {
            MetricsScope::Session(key) => sessions.iter().find(|row| row.matches(key)).map_or_else(
                AgentTotals::default,
                |row| AgentTotals {
                    input_tokens: row.tokens.input_tokens,
                    output_tokens: row.tokens.output_tokens,
                    cost_micro_usd: row.cost_micro_usd,
                    latency_spikes: row.latency_spikes,
                    // Traces are agent-scoped: a session reports none here
                    // (see `recent_traces`, which is the agent's window).
                    traces: 0,
                },
            ),
            MetricsScope::Agent(agent) => self.agent_totals(agent)?,
            _ => {
                let mut rolled = AgentTotals::default();
                for row in &agents {
                    rolled.accumulate(&row.totals);
                }
                rolled
            }
        };

        Ok(MetricsSnapshot {
            totals,
            sessions,
            agents,
            recent_spikes: self.collect_spikes(&bounds, window)?,
            recent_traces: self.collect_traces(&bounds, window)?,
        })
    }

    /// Per-session rows in scope, ascending by (commission, uid, session).
    /// A session appears once it recorded tokens, cost, or a spike.
    fn collect_sessions(&self, bounds: &ScanBounds) -> Result<Vec<SessionTotals>, TelemetryError> {
        let mut rows: BTreeMap<SessionKey, SessionTotals> = BTreeMap::new();

        let tokens = self.db.open_tree(TREE_TOKENS)?;
        for entry in tokens.scan_prefix(&bounds.prefix) {
            let (key, value) = entry?;
            if key.len() != SESSION_KEY_LEN {
                return Err(corrupt(TREE_TOKENS, "token key is not session width"));
            }
            let session = SessionKey::decode(&key, TREE_TOKENS)?;
            if !bounds.matches(session) {
                continue;
            }
            rows.entry(session)
                .or_insert_with(|| SessionTotals::from(session))
                .tokens = decode_json::<TokenTally>(TREE_TOKENS, &value)?;
        }

        let costs = self.db.open_tree(TREE_COSTS)?;
        for entry in costs.scan_prefix(&bounds.prefix) {
            let (key, value) = entry?;
            if key.len() != SESSION_KEY_LEN {
                return Err(corrupt(TREE_COSTS, "cost key is not session width"));
            }
            let session = SessionKey::decode(&key, TREE_COSTS)?;
            if !bounds.matches(session) {
                continue;
            }
            rows.entry(session)
                .or_insert_with(|| SessionTotals::from(session))
                .cost_micro_usd = decode_counter(TREE_COSTS, &value)?;
        }

        let latency = self.db.open_tree(TREE_LATENCY)?;
        for entry in latency.scan_prefix(&bounds.prefix) {
            let (key, _) = entry?;
            if key.len() != SESSION_KEY_LEN + 8 {
                return Err(corrupt(TREE_LATENCY, "spike key is not session+seq width"));
            }
            let session = SessionKey::decode(&key, TREE_LATENCY)?;
            if !bounds.matches(session) {
                continue;
            }
            rows.entry(session)
                .or_insert_with(|| SessionTotals::from(session))
                .latency_spikes += 1;
        }

        Ok(rows.into_values().collect())
    }

    /// Per-agent rollups in scope, ascending by uid. A uid seen under
    /// several commissions (`Uid` scope) merges into one row — that is what
    /// the scope means.
    fn collect_agents(
        &self,
        bounds: &ScanBounds,
        sessions: &[SessionTotals],
    ) -> Result<Vec<AgentRow>, TelemetryError> {
        // Distinct agents from the session rows *and* the trace tree (an
        // agent that only ever traced has no session rows).
        let mut agents: BTreeSet<AgentKey> = sessions
            .iter()
            .map(|row| AgentKey {
                commission: row.commission,
                uid: row.uid,
            })
            .collect();
        let traces = self.db.open_tree(TREE_TRACES)?;
        for entry in traces.scan_prefix(&bounds.prefix) {
            let (key, _) = entry?;
            let agent = AgentKey::decode(&key, TREE_TRACES)?;
            if bounds.matches_agent(agent) {
                agents.insert(agent);
            }
        }

        let mut merged: BTreeMap<u32, AgentRow> = BTreeMap::new();
        for agent in agents {
            let totals = self.agent_totals(agent)?;
            match merged.get_mut(&agent.uid) {
                Some(row) => {
                    row.commission = row.commission.min(agent.commission);
                    row.totals.accumulate(&totals);
                }
                None => {
                    merged.insert(
                        agent.uid,
                        AgentRow {
                            commission: agent.commission,
                            uid: agent.uid,
                            totals,
                        },
                    );
                }
            }
        }
        Ok(merged.into_values().collect())
    }

    /// The last `window` latency spikes in scope, ascending by key.
    fn collect_spikes(
        &self,
        bounds: &ScanBounds,
        window: usize,
    ) -> Result<Vec<LatencySpike>, TelemetryError> {
        let tree = self.db.open_tree(TREE_LATENCY)?;
        let mut all = Vec::new();
        for entry in tree.scan_prefix(&bounds.prefix) {
            let (key, value) = entry?;
            if key.len() != SESSION_KEY_LEN + 8 {
                return Err(corrupt(TREE_LATENCY, "spike key is not session+seq width"));
            }
            let session = SessionKey::decode(&key, TREE_LATENCY)?;
            if !bounds.matches(session) {
                continue;
            }
            all.push(decode_json::<LatencySpike>(TREE_LATENCY, &value)?);
        }
        let start = all.len().saturating_sub(window);
        Ok(all.split_off(start))
    }

    /// The last `window` traces in scope, ascending by (commission, uid,
    /// seq) — key order, which is seq order per agent, not a global clock.
    fn collect_traces(
        &self,
        bounds: &ScanBounds,
        window: usize,
    ) -> Result<Vec<TraceRecord>, TelemetryError> {
        let tree = self.db.open_tree(TREE_TRACES)?;
        let mut all = Vec::new();
        for entry in tree.scan_prefix(&bounds.prefix) {
            let (key, value) = entry?;
            if key.len() != AGENT_KEY_LEN + 8 {
                return Err(corrupt(TREE_TRACES, "trace key is not agent+seq width"));
            }
            let agent = AgentKey::decode(&key, TREE_TRACES)?;
            if !bounds.matches_agent(agent) {
                continue;
            }
            all.push(decode_json::<TraceRecord>(TREE_TRACES, &value)?);
        }
        let start = all.len().saturating_sub(window);
        Ok(all.split_off(start))
    }

    fn commission_uids(&self, commission: u64) -> Result<BTreeSet<u32>, TelemetryError> {
        let prefix = commission.to_be_bytes();
        let mut uids = BTreeSet::new();
        for name in [TREE_TOKENS, TREE_COSTS, TREE_LATENCY, TREE_TRACES] {
            let tree = self.db.open_tree(name)?;
            for entry in tree.scan_prefix(prefix) {
                let (key, _) = entry?;
                uids.insert(AgentKey::decode(&key, name)?.uid);
            }
        }
        Ok(uids)
    }

    fn count_prefix(&self, tree_name: &str, prefix: &[u8]) -> Result<u64, TelemetryError> {
        let tree = self.db.open_tree(tree_name)?;
        let mut count = 0u64;
        for entry in tree.scan_prefix(prefix) {
            let _ = entry?;
            count += 1;
        }
        Ok(count)
    }

    // --- export & retention ---

    /// Build the full archive summary for one commission (all agents, all
    /// latency spikes, all traces — the export is the durable record).
    /// `generated_at_unix_ms` is caller-supplied (CONVENTIONS §3 rule 4).
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`] or [`TelemetryError::Corrupt`].
    pub fn commission_summary(
        &self,
        commission: u64,
        generated_at_unix_ms: u64,
    ) -> Result<CommissionSummary, TelemetryError> {
        let (totals, _) = self.commission_totals(commission)?;
        let mut agents = BTreeMap::new();
        for uid in self.commission_uids(commission)? {
            let agent = AgentKey { commission, uid };
            agents.insert(
                uid,
                AgentSummary {
                    totals: self.agent_totals(agent)?,
                    latency_spikes: self.all_latency_spikes(agent)?,
                    traces: self.recent_traces(agent, usize::MAX)?,
                },
            );
        }
        Ok(CommissionSummary {
            summary_version: SUMMARY_VERSION,
            commission,
            generated_at_unix_ms,
            totals,
            agents,
        })
    }

    /// Export the commission summary as canonical JSON into
    /// `keep/archives/commission-<n>-telemetry.json` (typed tree accessor —
    /// ADR-0003; atomic temp → fsync → rename — ADR-0004 protocol). Used at
    /// commission close (wired in Sprint 06). Returns the archive path.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`], [`TelemetryError::Corrupt`], or
    /// [`TelemetryError::Io`].
    pub fn export_commission_summary(
        &self,
        tree: &HamletTree,
        commission: u64,
        generated_at_unix_ms: u64,
    ) -> Result<PathBuf, TelemetryError> {
        let summary = self.commission_summary(commission, generated_at_unix_ms)?;
        let archives = tree.keep_archives();
        fs::create_dir_all(&archives)
            .map_err(|err| TelemetryError::Io(format!("create {}: {err}", archives.display())))?;
        let path = archives.join(CommissionSummary::archive_file_name(commission));
        write_atomic(&path, summary.render().as_bytes())
            .map_err(|err| TelemetryError::Io(format!("export {}: {err}", path.display())))?;
        Ok(path)
    }

    /// Drop every telemetry record for one commission (all four data trees
    /// plus its sequence counters); returns the number of keys removed.
    /// Retention policy (ADR-0005): the `keep/archives/` export is the
    /// durable record, so sled may be pruned after commission close.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::Storage`].
    pub fn prune_commission(&self, commission: u64) -> Result<u64, TelemetryError> {
        let prefix = commission.to_be_bytes();
        let mut removed = 0u64;
        for name in [TREE_TOKENS, TREE_COSTS, TREE_LATENCY, TREE_TRACES] {
            removed += self.prune_prefix(name, &prefix)?;
        }
        // Counters are domain-prefixed: [domain][commission][uid].
        for domain in [DOMAIN_TRACE_SEQ, DOMAIN_LATENCY_SEQ] {
            let mut key = Vec::with_capacity(9);
            key.push(domain);
            key.extend_from_slice(&prefix);
            removed += self.prune_prefix(TREE_COUNTERS, &key)?;
        }
        Ok(removed)
    }

    fn prune_prefix(&self, tree_name: &str, prefix: &[u8]) -> Result<u64, TelemetryError> {
        let tree = self.db.open_tree(tree_name)?;
        let keys: Vec<sled::IVec> = tree
            .scan_prefix(prefix)
            .keys()
            .collect::<Result<Vec<_>, _>>()?;
        let count = u64::try_from(keys.len()).expect("usize fits u64 on 64-bit targets");
        for key in keys {
            tree.remove(key)?;
        }
        Ok(count)
    }

    fn all_latency_spikes(&self, agent: AgentKey) -> Result<Vec<LatencySpike>, TelemetryError> {
        let tree = self.db.open_tree(TREE_LATENCY)?;
        let mut out = Vec::new();
        for entry in tree.scan_prefix(agent.encode()) {
            let (key, value) = entry?;
            if key.len() != SESSION_KEY_LEN + 8 {
                return Err(corrupt(TREE_LATENCY, "spike key is not session+seq width"));
            }
            out.push(decode_json::<LatencySpike>(TREE_LATENCY, &value)?);
        }
        Ok(out)
    }
}

// --- value codecs -----------------------------------------------------------

/// Plain-struct records (String/u64 fields only): serde_json serialization
/// cannot fail.
fn encode_json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("telemetry record serialization is infallible")
}

fn decode_json<T: serde::de::DeserializeOwned>(
    tree: &str,
    bytes: &[u8],
) -> Result<T, TelemetryError> {
    serde_json::from_slice(bytes)
        .map_err(|err| corrupt(tree, format!("value does not decode: {err}")))
}

/// Raw `u64` big-endian counters (costs, sequences). A wrong-width counter
/// is corruption, never zero — fail-hard like every other decode.
fn decode_counter(tree: &str, bytes: &[u8]) -> Result<u64, TelemetryError> {
    bytes
        .try_into()
        .map(u64::from_be_bytes)
        .map_err(|_| corrupt(tree, "counter is not 8-byte big-endian"))
}

/// Atomic file write, ADR-0004 protocol: same-dir temp `.<name>.tmp` →
/// write → fsync → chmod 0644 → rename → dir fsync. Never in place.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(
        path.file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?,
    );
    tmp_name.push(".tmp");
    let tmp = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?
        .join(tmp_name);
    let result = (|| -> io::Result<()> {
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        set_canonical_mode(&tmp)?;
        fs::rename(&tmp, path)?;
        if let Ok(dir) = fs::File::open(path.parent().expect("parent checked above")) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    result.inspect_err(|_| {
        let _ = fs::remove_file(&tmp); // best-effort cleanup
    })
}

#[cfg(unix)]
fn set_canonical_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
}

#[cfg(not(unix))]
fn set_canonical_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// sled's implicit default tree. The store never writes to it; the
    /// boundary test asserts it stays empty (nothing hides in the one tree
    /// the allowlist doesn't own).
    const SLED_DEFAULT_TREE: &str = "__sled__default";

    const COMMISSION: u64 = 7;
    const MAYOR: u32 = 1000;
    const AGENT: u32 = 1001;
    const GENERATED_AT: u64 = 1_700_000_000_999;

    fn session(uid: u32, n: u64) -> SessionKey {
        SessionKey::new(COMMISSION, uid, n)
    }

    fn agent(uid: u32) -> AgentKey {
        AgentKey {
            commission: COMMISSION,
            uid,
        }
    }

    fn trace(at: u64, event: &str, detail: &str) -> TraceRecord {
        TraceRecord {
            at_unix_ms: at,
            event: event.to_string(),
            detail: detail.to_string(),
        }
    }

    fn spike(at: u64, ms: u64, op: &str) -> LatencySpike {
        LatencySpike {
            at_unix_ms: at,
            duration_ms: ms,
            operation: op.to_string(),
        }
    }

    /// Reopen a db directory whose previous handle was dropped in-process.
    /// sled releases its file lock on a background shutdown thread, so an
    /// immediate reopen can race the release — retry briefly instead of
    /// flaking. (After a real `kill -9` no retry is needed: the kernel
    /// drops the lock with the process.)
    fn reopen(dir: &Path) -> Telemetry {
        for _ in 0..100 {
            match Telemetry::open(dir) {
                Ok(t) => return t,
                Err(TelemetryError::Storage(_)) => {
                    std::thread::sleep(std::time::Duration::from_millis(25))
                }
                Err(err) => panic!("reopen failed: {err}"),
            }
        }
        Telemetry::open(dir).unwrap()
    }

    /// Deterministic fixture battery used by the summary-math, export-golden
    /// and boundary tests. Fixed timestamps only (caller-supplied clock).
    fn populate(t: &Telemetry) {
        // Mayor: one session, a little usage, one trace.
        t.add_tokens(
            session(MAYOR, 9),
            TokenTally {
                input_tokens: 10,
                output_tokens: 5,
            },
        )
        .unwrap();
        t.add_cost_micro_usd(session(MAYOR, 9), 100).unwrap();
        t.trace_event(
            agent(MAYOR),
            trace(1_700_000_000_050, "commission-open", "commission 7 opened"),
        )
        .unwrap();
        // Agent 1001: two sessions, accumulated tokens/cost, one spike,
        // two traces.
        t.add_tokens(
            session(AGENT, 1),
            TokenTally {
                input_tokens: 100,
                output_tokens: 40,
            },
        )
        .unwrap();
        t.add_tokens(
            session(AGENT, 1),
            TokenTally {
                input_tokens: 0,
                output_tokens: 1,
            },
        )
        .unwrap();
        t.add_tokens(
            session(AGENT, 2),
            TokenTally {
                input_tokens: 7,
                output_tokens: 2,
            },
        )
        .unwrap();
        t.add_cost_micro_usd(session(AGENT, 1), 250).unwrap();
        t.add_cost_micro_usd(session(AGENT, 1), 1).unwrap();
        t.add_cost_micro_usd(session(AGENT, 2), 4).unwrap();
        t.record_latency_spike(
            session(AGENT, 1),
            spike(1_700_000_000_100, 1200, "mail-delivery"),
        )
        .unwrap();
        t.trace_event(
            agent(AGENT),
            trace(1_700_000_000_200, "session-start", "session 1 spawned"),
        )
        .unwrap();
        t.trace_event(
            agent(AGENT),
            trace(1_700_000_000_300, "session-start", "session 2 spawned"),
        )
        .unwrap();
        t.flush().unwrap();
    }

    #[test]
    fn key_encodings_are_fixed_width_sorted() {
        // Golden vectors for the ADR-0005 key layout.
        assert_eq!(
            SessionKey::new(1, 1001, 2).encode().to_vec(),
            [
                0, 0, 0, 0, 0, 0, 0, 1, // commission = 1
                0, 0, 3, 233, // uid = 1001
                0, 0, 0, 0, 0, 0, 0, 2, // session = 2
            ]
        );
        assert_eq!(
            AgentKey {
                commission: 1,
                uid: 1001
            }
            .encode()
            .to_vec(),
            [0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 3, 233]
        );
        // BE ordering means byte order == logical order.
        let a = SessionKey::new(1, 1001, 2).encode();
        let b = SessionKey::new(1, 1001, 10).encode();
        assert!(a < b);
        // Agent key is a session key prefix → shared scan ordering.
        assert_eq!(
            &a[..AGENT_KEY_LEN],
            &AgentKey {
                commission: 1,
                uid: 1001
            }
            .encode()
        );
    }

    #[test]
    fn write_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        assert_eq!(t.session_tokens(session(AGENT, 1)).unwrap(), None);
        assert_eq!(t.session_cost_micro_usd(session(AGENT, 1)).unwrap(), 0);

        t.add_tokens(
            session(AGENT, 1),
            TokenTally {
                input_tokens: 5,
                output_tokens: 2,
            },
        )
        .unwrap();
        t.add_tokens(
            session(AGENT, 1),
            TokenTally {
                input_tokens: 3,
                output_tokens: 0,
            },
        )
        .unwrap();
        t.add_cost_micro_usd(session(AGENT, 1), 42).unwrap();
        t.add_cost_micro_usd(session(AGENT, 1), 8).unwrap();

        assert_eq!(
            t.session_tokens(session(AGENT, 1)).unwrap(),
            Some(TokenTally {
                input_tokens: 8,
                output_tokens: 2
            })
        );
        assert_eq!(t.session_cost_micro_usd(session(AGENT, 1)).unwrap(), 50);
        // A different session is untouched.
        assert_eq!(t.session_tokens(session(AGENT, 2)).unwrap(), None);
    }

    #[test]
    fn append_only_seqs_and_trace_window() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        for i in 0..5 {
            assert_eq!(
                t.trace_event(agent(AGENT), trace(1_700_000_000_000 + i, "tick", "x"))
                    .unwrap(),
                i + 1, // 1-based, monotonic
            );
        }
        assert_eq!(
            t.record_latency_spike(session(AGENT, 1), spike(1, 900, "podman-exec"))
                .unwrap(),
            1
        );
        assert_eq!(
            t.record_latency_spike(session(AGENT, 1), spike(2, 950, "podman-exec"))
                .unwrap(),
            2
        );
        // Window returns the most recent N, oldest first.
        let recent = t.recent_traces(agent(AGENT), 3).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].at_unix_ms, 1_700_000_000_002);
        assert_eq!(recent[2].at_unix_ms, 1_700_000_000_004);
        // Window larger than the log returns everything; zero returns none.
        assert_eq!(t.recent_traces(agent(AGENT), 99).unwrap().len(), 5);
        assert!(t.recent_traces(agent(AGENT), 0).unwrap().is_empty());
        // A different agent has its own sequence space.
        assert_eq!(
            t.trace_event(agent(MAYOR), trace(1, "tick", "y")).unwrap(),
            1
        );
    }

    #[test]
    fn summary_math() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);

        assert_eq!(
            t.agent_totals(agent(AGENT)).unwrap(),
            AgentTotals {
                input_tokens: 107,
                output_tokens: 43,
                cost_micro_usd: 255,
                latency_spikes: 1,
                traces: 2,
            }
        );
        assert_eq!(
            t.agent_totals(agent(MAYOR)).unwrap(),
            AgentTotals {
                input_tokens: 10,
                output_tokens: 5,
                cost_micro_usd: 100,
                latency_spikes: 0,
                traces: 1,
            }
        );

        let (totals, per_agent) = t.commission_totals(COMMISSION).unwrap();
        assert_eq!(
            totals,
            CommissionTotals {
                agents: 2,
                input_tokens: 117,
                output_tokens: 48,
                cost_micro_usd: 355,
                latency_spikes: 1,
                traces: 3,
            }
        );
        assert_eq!(
            per_agent.keys().copied().collect::<Vec<_>>(),
            vec![MAYOR, AGENT]
        );
        // Unknown commission: empty, not an error.
        let (empty, agents) = t.commission_totals(999).unwrap();
        assert_eq!(empty, CommissionTotals::default());
        assert!(agents.is_empty());
    }

    // --- live query surface (Sprint 03 step 04) ----------------------------

    /// `Session` scope is session-exact: one row, no agent rollup (traces
    /// are agent-keyed, so an agent row would answer a broader question).
    #[test]
    fn metrics_for_session_scope_is_session_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);

        let snapshot = t
            .metrics_for(MetricsScope::Session(session(AGENT, 1)), 10)
            .unwrap();
        assert_eq!(
            snapshot.totals,
            AgentTotals {
                input_tokens: 100,
                output_tokens: 41,
                cost_micro_usd: 251,
                latency_spikes: 1,
                traces: 0,
            }
        );
        assert_eq!(snapshot.sessions.len(), 1, "sibling sessions excluded");
        assert_eq!(snapshot.sessions[0].session, 1);
        assert_eq!(snapshot.sessions[0].uid, AGENT);
        assert!(snapshot.agents.is_empty());
        // Spikes are session-scoped; traces are the agent's window.
        assert_eq!(snapshot.recent_spikes.len(), 1);
        assert_eq!(snapshot.recent_spikes[0].operation, "mail-delivery");
        assert_eq!(snapshot.recent_traces.len(), 2);
    }

    /// `Agent` scope lists every session of the identity and rolls up.
    #[test]
    fn metrics_for_agent_scope_rolls_up_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);

        let snapshot = t
            .metrics_for(MetricsScope::Agent(agent(AGENT)), 10)
            .unwrap();
        assert_eq!(snapshot.totals, t.agent_totals(agent(AGENT)).unwrap());
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|row| (row.session, row.cost_micro_usd))
                .collect::<Vec<_>>(),
            vec![(1, 251), (2, 4)],
            "sessions ascending by id"
        );
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(snapshot.agents[0].uid, AGENT);
        assert_eq!(snapshot.agents[0].totals, snapshot.totals);
    }

    /// `Commission` scope rolls every agent up, sorted by uid.
    #[test]
    fn metrics_for_commission_scope_rolls_up_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);

        let snapshot = t
            .metrics_for(MetricsScope::Commission(COMMISSION), 10)
            .unwrap();
        assert_eq!(
            snapshot
                .agents
                .iter()
                .map(|row| row.uid)
                .collect::<Vec<_>>(),
            vec![MAYOR, AGENT]
        );
        assert_eq!(
            snapshot.totals,
            AgentTotals {
                input_tokens: 117,
                output_tokens: 48,
                cost_micro_usd: 355,
                latency_spikes: 1,
                traces: 3,
            }
        );
        assert_eq!(snapshot.sessions.len(), 3);
        // The commission rollup must agree with the Sprint-01 API.
        let (totals, _) = t.commission_totals(COMMISSION).unwrap();
        assert_eq!(snapshot.totals.input_tokens, totals.input_tokens);
        assert_eq!(snapshot.totals.cost_micro_usd, totals.cost_micro_usd);
        assert_eq!(snapshot.totals.traces, totals.traces);
    }

    /// `Uid` scope crosses commissions: one merged agent row, sessions from
    /// both, and no other uid's data.
    #[test]
    fn metrics_for_uid_scope_crosses_commissions() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);
        // The same uid slot under a different commission, plus a different
        // uid that must not appear.
        let other = SessionKey::new(42, AGENT, 1);
        t.add_tokens(
            other,
            TokenTally {
                input_tokens: 5,
                output_tokens: 5,
            },
        )
        .unwrap();
        t.add_tokens(
            SessionKey::new(42, 1500, 1),
            TokenTally {
                input_tokens: 99,
                output_tokens: 99,
            },
        )
        .unwrap();

        let snapshot = t.metrics_for(MetricsScope::Uid(AGENT), 10).unwrap();
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(snapshot.agents[0].uid, AGENT);
        assert_eq!(snapshot.agents[0].commission, 7, "lowest commission");
        assert_eq!(
            snapshot.agents[0].totals.input_tokens,
            107 + 5,
            "both commissions merged"
        );
        assert_eq!(snapshot.totals.input_tokens, 112);
        assert!(
            snapshot.sessions.iter().all(|row| row.uid == AGENT),
            "uid filter must exclude other slots: {:?}",
            snapshot.sessions
        );
        assert_eq!(snapshot.sessions.len(), 3);
    }

    /// `window` bounds the recent lists from the end; an unknown scope is
    /// empty, never an error.
    #[test]
    fn metrics_for_window_and_unknown_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);

        let one = t.metrics_for(MetricsScope::Agent(agent(AGENT)), 1).unwrap();
        assert_eq!(one.recent_traces.len(), 1);
        assert_eq!(one.recent_traces[0].detail, "session 2 spawned");
        let all = t
            .metrics_for(MetricsScope::Agent(agent(AGENT)), usize::MAX)
            .unwrap();
        assert_eq!(all.recent_traces.len(), 2);

        let empty = t.metrics_for(MetricsScope::Commission(999), 10).unwrap();
        assert_eq!(empty.totals, AgentTotals::default());
        assert!(empty.sessions.is_empty() && empty.agents.is_empty());
        assert!(empty.recent_traces.is_empty() && empty.recent_spikes.is_empty());
    }

    /// The snapshot is the WS/CLI read model: it round-trips through
    /// canonical JSON and renders sessions/agents in sorted order.
    #[test]
    fn metrics_snapshot_serializes_for_the_wire() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);

        let snapshot = t
            .metrics_for(MetricsScope::Commission(COMMISSION), 5)
            .unwrap();
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["totals"]["input_tokens"], 117);
        assert_eq!(json["agents"][0]["uid"], MAYOR);
        assert_eq!(json["sessions"][0]["commission"], COMMISSION);
        let decoded: MetricsSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn export_golden_file() {
        let tmp = tempfile::tempdir().unwrap();
        let village = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);

        let path = t
            .export_commission_summary(&village, COMMISSION, GENERATED_AT)
            .unwrap();
        assert_eq!(
            path,
            village.keep_archives().join("commission-7-telemetry.json")
        );
        let bytes = fs::read(&path).unwrap();
        let rendered = String::from_utf8(bytes.clone()).unwrap();
        // Golden vector: committed fixture, byte-for-byte. Regeneration is
        // deliberate (CONVENTIONS §9): HAMLET_UPDATE_GOLDEN=1 rewrites the
        // fixture so the diff shows up in review.
        if std::env::var_os("HAMLET_UPDATE_GOLDEN").is_some() {
            let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden/commission-7-telemetry.json");
            fs::write(golden, &rendered).unwrap();
        }
        assert_eq!(
            rendered,
            include_str!("../tests/golden/commission-7-telemetry.json")
        );
        // Export is deterministic: re-export is byte-identical, and the
        // atomic write leaves no temp file behind.
        let path2 = t
            .export_commission_summary(&village, COMMISSION, GENERATED_AT)
            .unwrap();
        assert_eq!(path2, path);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let leftovers: Vec<_> = fs::read_dir(village.keep_archives())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().starts_with('.'))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        // Round-trips back through the typed API.
        let parsed: CommissionSummary = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            parsed,
            t.commission_summary(COMMISSION, GENERATED_AT).unwrap()
        );
    }

    #[test]
    fn prune_after_export_retains_only_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let village = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);
        // Second commission must survive pruning of the first.
        t.add_tokens(
            SessionKey::new(8, AGENT, 1),
            TokenTally {
                input_tokens: 1,
                output_tokens: 1,
            },
        )
        .unwrap();
        t.trace_event(
            AgentKey {
                commission: 8,
                uid: AGENT,
            },
            trace(1, "tick", "other"),
        )
        .unwrap();

        let path = t
            .export_commission_summary(&village, COMMISSION, GENERATED_AT)
            .unwrap();
        let removed = t.prune_commission(COMMISSION).unwrap();
        // tokens: mayor(9), agent(1), agent(2) = 3; costs: same 3;
        // latency: 1; traces: 3; counters: trace-domain × 2 agents +
        // latency-domain × 1 agent = 3.
        assert_eq!(removed, 3 + 3 + 1 + 3 + 3);

        let (totals, agents) = t.commission_totals(COMMISSION).unwrap();
        assert_eq!(totals, CommissionTotals::default());
        assert!(agents.is_empty());
        assert!(t.recent_traces(agent(AGENT), 10).unwrap().is_empty());
        // Commission 8 untouched; the archive remains the durable record.
        assert_eq!(t.commission_totals(8).unwrap().0.traces, 1);
        assert!(path.exists());
        let summary: CommissionSummary = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(summary.totals.cost_micro_usd, 355);
        // Sequences restart after pruning — acceptable: seqs order records
        // within a live commission, and the archive is the durable record.
        assert_eq!(
            t.trace_event(agent(AGENT), trace(2, "tick", "fresh"))
                .unwrap(),
            1
        );
    }

    /// Boundary contract (CONVENTIONS §2): sled is telemetry ONLY. Fails if
    /// any tree outside the allowlist exists, if any key doesn't match the
    /// fixed-width ADR-0005 layout, or if any charter-shaped token appears
    /// in a key or value.
    #[test]
    fn no_orchestration_state_in_sled() {
        let tmp = tempfile::tempdir().unwrap();
        let t = Telemetry::open(tmp.path().join("db")).unwrap();
        populate(&t);

        // 1. Tree allowlist: exactly the telemetry namespaces, plus sled's
        // internal default tree which must stay empty.
        let names: BTreeSet<String> =
            t.db.tree_names()
                .iter()
                .map(|n| String::from_utf8(n.to_vec()).unwrap())
                .collect();
        for name in &names {
            if name == SLED_DEFAULT_TREE {
                let tree = t.db.open_tree(name).unwrap();
                assert_eq!(tree.len(), 0, "sled default tree must hold no records");
                continue;
            }
            assert!(
                TELEMETRY_TREES.contains(&name.as_str()),
                "sled tree {name:?} is not a telemetry namespace — orchestration state belongs in charter.json"
            );
        }

        // 2. Fixed-width keys + no charter-shaped bytes anywhere.
        const FORBIDDEN: &[&[u8]] = &[
            b"charter",
            b"charter_version",
            b"slot",
            b"status",
            b"decree",
            b"parcel",
            b"webhook",
            b"identity",
            b"stasis",
        ];
        let mut seen = 0usize;
        for name in names.iter().filter(|n| n.as_str() != SLED_DEFAULT_TREE) {
            let tree = t.db.open_tree(name).unwrap();
            let width = match name.as_str() {
                TREE_TOKENS | TREE_COSTS => SESSION_KEY_LEN,
                TREE_TRACES => AGENT_KEY_LEN + 8,
                TREE_LATENCY => SESSION_KEY_LEN + 8,
                TREE_COUNTERS => 1 + AGENT_KEY_LEN,
                _ => unreachable!("allowlisted above"),
            };
            for entry in tree.iter() {
                let (key, value) = entry.unwrap();
                assert_eq!(
                    key.len(),
                    width,
                    "key in {name} is not ADR-0005 fixed-width ({} bytes)",
                    key.len()
                );
                for token in FORBIDDEN {
                    assert!(
                        !key.windows(token.len()).any(|w| w == *token),
                        "charter-shaped key in {name}"
                    );
                    assert!(
                        !value.windows(token.len()).any(|w| w == *token),
                        "charter-shaped value in {name}"
                    );
                }
                seen += 1;
            }
        }
        assert!(seen > 0, "test battery must have written something");

        // 3. A charter itself must not round-trip through any value: values
        //    are telemetry records with deny_unknown_fields, so feeding a
        //    charter document to the decoders fails.
        let charter = hamlet_core::charter::Charter::empty();
        let rendered = charter.render();
        assert!(decode_json::<TokenTally>(TREE_TOKENS, rendered.as_bytes()).is_err());
        assert!(decode_json::<TraceRecord>(TREE_TRACES, rendered.as_bytes()).is_err());
    }

    #[test]
    fn open_for_village_places_db_outside_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let village_root = tmp.path().join(".hamlet");
        HamletTree::ensure(&village_root).unwrap();

        // Default config → sibling .hamlet-data/sled, outside the tree.
        let t = Telemetry::open_for_village(&village_root, &Config::default()).unwrap();
        assert_eq!(
            t.data_dir(),
            tmp.path().join(".hamlet-data").join("sled").as_path()
        );
        assert!(!t.data_dir().starts_with(&village_root));
        drop(t);

        // Operator pointing daemon_data_dir INSIDE the tree is rejected.
        let bad = Config {
            daemon_data_dir: PathBuf::from(".hamlet/sled"),
            ..Default::default()
        };
        let err = Telemetry::open_for_village(&village_root, &bad).unwrap_err();
        assert!(
            matches!(&err, TelemetryError::InsideVillageTree(p) if p == &village_root.join("sled")),
            "unexpected error: {err}"
        );
        // Absolute paths pass through (operator override), still guarded.
        let abs = Config {
            daemon_data_dir: tmp.path().join("elsewhere"),
            ..Default::default()
        };
        let t = Telemetry::open_for_village(&village_root, &abs).unwrap();
        assert_eq!(t.data_dir(), tmp.path().join("elsewhere"));
    }

    #[test]
    fn corrupt_value_fails_hard() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("db");
        {
            let t = Telemetry::open(&dir).unwrap();
            // Raw sled write of a charter-shaped document into a telemetry
            // tree — exactly the boundary violation the store must refuse to
            // interpret.
            let raw = t.db.open_tree(TREE_TOKENS).unwrap();
            raw.insert(
                session(AGENT, 1).encode(),
                br#"{"charter_version": 1}"#.to_vec(),
            )
            .unwrap();
        }
        let t = reopen(&dir);
        let err = t.session_tokens(session(AGENT, 1)).unwrap_err();
        assert!(
            matches!(err, TelemetryError::Corrupt { .. }),
            "unexpected: {err}"
        );
        let err = t.agent_totals(agent(AGENT)).unwrap_err();
        assert!(
            matches!(err, TelemetryError::Corrupt { .. }),
            "unexpected: {err}"
        );
    }

    /// Kill/restart resumability (CONVENTIONS §3 rule 6, telemetry flavor):
    /// a child process opens the db, writes, flushes a checkpoint, writes
    /// more *unflushed*, then `exit(9)`s without running destructors —
    /// `kill -9` semantics. The parent reopens against the same directory:
    /// the db must recover cleanly, checkpointed data must be intact, and
    /// the store must remain writable.
    #[test]
    fn kill_restart_resumability() {
        if std::env::var_os("HAMLET_CRASH_CHILD").is_some() {
            // Child phase: no asserts that could pollute the exit code —
            // any failure panics, and the parent sees a non-9 exit.
            let dir = PathBuf::from(std::env::var_os("HAMLET_CRASH_DIR").unwrap());
            let t = Telemetry::open(dir).unwrap();
            t.add_tokens(
                session(AGENT, 1),
                TokenTally {
                    input_tokens: 11,
                    output_tokens: 22,
                },
            )
            .unwrap();
            t.trace_event(
                agent(AGENT),
                trace(1_700_000_000_000, "boot", "checkpointed"),
            )
            .unwrap();
            t.flush().unwrap(); // durability checkpoint
                                // Unflushed tail — must neither corrupt the db nor be assumed
                                // present after the crash.
            t.trace_event(agent(AGENT), trace(1_700_000_000_001, "boot", "unflushed"))
                .unwrap();
            std::process::exit(9);
        }

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("db");
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "telemetry::tests::kill_restart_resumability",
                "--exact",
                "--nocapture",
            ])
            .env("HAMLET_CRASH_CHILD", "1")
            .env("HAMLET_CRASH_DIR", &dir)
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(9),
            "child must die via exit(9), not fail"
        );

        // Restart against the same tree → identical recovered state for
        // everything checkpointed; db is sound and writable.
        let t = Telemetry::open(&dir).unwrap();
        assert_eq!(
            t.session_tokens(session(AGENT, 1)).unwrap(),
            Some(TokenTally {
                input_tokens: 11,
                output_tokens: 22
            })
        );
        let traces = t.recent_traces(agent(AGENT), 10).unwrap();
        assert!(traces.iter().any(|r| r.detail == "checkpointed"));
        // Continuing to write after recovery works and sequences stay sane.
        t.add_tokens(
            session(AGENT, 1),
            TokenTally {
                input_tokens: 1,
                output_tokens: 0,
            },
        )
        .unwrap();
        t.flush().unwrap();
        assert_eq!(
            t.session_tokens(session(AGENT, 1)).unwrap(),
            Some(TokenTally {
                input_tokens: 12,
                output_tokens: 22
            })
        );
        drop(t);
        // Clean restart too: full close + reopen → byte-stable summaries.
        let t = reopen(&dir);
        let before = t.agent_totals(agent(AGENT)).unwrap();
        drop(t);
        let t = reopen(&dir);
        assert_eq!(t.agent_totals(agent(AGENT)).unwrap(), before);
    }
}
