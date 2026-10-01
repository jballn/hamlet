//! Agent session protocol (SPEC §6; wire schema resolved by ADR-0015 —
//! cited as "ADR-0008" by the Sprint 03 prompts).
//!
//! Typed model of the daemon↔agent channel for pi coding-agent sessions
//! driven over `podman exec -i` stdio. Two layers, one wire:
//!
//! 1. **Hamlet envelopes** ([`SessionEnvelope`]): the typed, persistable
//!    units the delivery queue (Sprint 03 step 03) and the daemon API
//!    exchange. Queued deliveries ([`SessionEnvelope::Mail`],
//!    [`SessionEnvelope::Tasking`], [`SessionEnvelope::Notice`]) and the
//!    out-of-band [`SessionEnvelope::InterruptAndSteer`] are *distinguishable
//!    by the receiving session* (SPEC §6): they carry different `kind` tags,
//!    different correlation-id prefixes, and different pi-wire bindings.
//! 2. **pi wire binding** ([`pi_binding`], [`PiCommand`]): envelopes are
//!    projected onto pi's actual RPC dialect — strict JSONL (one JSON object
//!    per LF-terminated line; pi docs `rpc.md` "Framing") of commands on
//!    stdin and `response`/session-event records on stdout. The daemon has
//!    exactly **one** wire dialect: real pi and [`FakeAgent`](#fakeagent-contract)
//!    read the same bytes.
//!
//! # Session identity & status
//!
//! [`SessionId`] = permanent UID slot + incarnation counter (bumped on every
//! supervised restart; ADR-0015 cl. 4). [`SessionStatus`] is the charter's
//! session state with a closed transition table
//! ([`SessionStatus::can_transition_to`]); [`SessionRecord`] is the charter
//! `sessions` group entry (charter v2, ADR-0004 additive migration).
//!
//! # Task-boundary semantics (SPEC §6 delivery)
//!
//! Queued mail is written **only** at a task boundary: the session is
//! [`SessionStatus::Idle`] or [`SessionStatus::Running`] (never mid-task).
//! The boundary is the pi `agent_settled` event — not `agent_end`, which can
//! be followed by retries, compaction, or queued steering (pi docs
//! `json.md`). [`SessionStatus::on_agent_event`] is the pure status reducer
//! for stream events; process lifecycle transitions (spawn → ready,
//! exit → crashed/stopped, hibernation) are driven by the supervisor
//! (Sprint 03 step 02) through [`SessionStatus::transition`].
//! [`pi_binding`] *refuses* to project a queued envelope outside a boundary
//! ([`SessionError::NotAtTaskBoundary`]) — and pi itself independently
//! rejects a mid-stream `prompt` without `streamingBehavior`, so a boundary
//! bug can never silently interleave mail into a live turn.
//!
//! # Divine interruption (SPEC §6)
//!
//! `interrupt_and_steer` bypasses the queue and lands on the live pipe
//! immediately. pi has no single command with these semantics, so the
//! binding is a deterministic sequence (ADR-0015 cl. 3):
//!
//! - session **Busy**: `steer` (queues the steering message) followed by
//!   `abort` (interrupts the current turn; pi continues queued steering
//!   messages afterwards — pi docs `rpc-commands.md` `clear_queue`).
//!   Priority [`Critical`](InterruptPriority::Critical) prefixes
//!   `clear_queue` to drop previously queued steering/follow-ups first.
//! - session **Running/Idle** (nothing in flight): a single `prompt`
//!   carrying the same rendered interrupt header — there is no turn to
//!   interrupt, but the delivery stays visibly an interrupt.
//! - session **Starting**: [`SessionError::NotAtTaskBoundary`] — the
//!   supervisor must wait for readiness ([`readiness_probe`]) or stop.
//! - session **Crashed/Stopped/Hibernating**: [`SessionError::NotAlive`].
//!
//! Every interrupt record's `id` field carries the hamlet correlation id
//! (suffix-tagged per record), so pi's `response` records tie the interrupt
//! to its acknowledgment, and telemetry (Sprint 03 step 04) ties it to the
//! resulting run.
//!
//! # FakeAgent contract
//!
//! Normative spec for the scripted test binary (built by Sprint 03 step 02;
//! it *stands in for pi in all non-guarded tests*, CONVENTIONS §9):
//!
//! - **Location (recommended):** `crates/hamlet-daemon/src/bin/fakeagent.rs`
//!   — a second binary target of the daemon crate (no new workspace member;
//!   ADR-0001's 5-crate split is untouched). Step 02 records the final call.
//! - **Wire:** reads newline-delimited JSONL from stdin — exactly the
//!   [`PiCommand`] records [`pi_binding`] emits (plus test-only cue records,
//!   below) — and writes JSONL `response`/event records to stdout that
//!   [`parse_agent_event`] accepts. Stderr is diagnostics only, never
//!   protocol (pi contract). Framing: split input on LF only, strip one
//!   trailing CR; never treat U+2028/U+2029 as boundaries.
//! - **Parsing:** must use [`parse_pi_command`] (rejecting unknown commands
//!   with a pi-style `parse` failure response) so the fake and real pi agree
//!   on the accepted dialect.
//! - **Default (unscripted) behavior:** minimal faithful emulation —
//!   `prompt` → `response{disposition:"started"}` + `agent_start`,
//!   `turn_start`, one `message_update` (scripted usage), `turn_end`,
//!   `agent_end{willRetry:false}`, `agent_settled`; `steer` →
//!   `response{disposition:"queued"}` (delivered after the current turn when
//!   running); `abort` → end the run, `response` only once settled;
//!   `clear_queue` → `response` with the dropped texts; `get_state` →
//!   `response{success:true}` with `isStreaming` reflecting the run state.
//! - **Script:** a deterministic JSON scenario (path via `--script <path>`
//!   argv or `FAKEAGENT_SCRIPT` env; exact script schema owned by step 02)
//!   configures: token counts / cost values on emitted [`Usage`], per-event
//!   latency (ms of deliberate delay), crash on cue (exit with scripted
//!   code after N events/commands), slow-task hang (suppress `agent_settled`
//!   until cued), and malformed-frame injection for parser resilience tests.
//! - **Cues:** test harnesses may write `{"type":"fake_cue","cue":"<name>"}`
//!   records down the same stdin pipe; the fake consumes them and never
//!   echoes them. Cues are the only non-pi-dialect input.
//! - **Determinism:** no wall-clock or random inputs affect emitted record
//!   content or ordering; only the script and the received command sequence
//!   do (CONVENTIONS §3 rule 4 spirit).
//!
//! Everything here is pure (ADR-0001): no I/O, no clock, no process access.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::slots::{FIRST_AGENT_UID, LAST_AGENT_UID, MAYOR_UID};

// ---------------------------------------------------------------------------
// Session identity
// ---------------------------------------------------------------------------

/// Identity of one supervised agent-session lifetime: the permanent UID
/// slot plus an incarnation counter bumped on every supervised restart
/// (ADR-0015 cl. 4). Incarnation `0` is the session's first life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionId {
    /// Permanent UID slot: 1000 (Mayor) or 1001–2000 (SPEC §4).
    pub uid: u32,
    /// Restart counter for this slot; never reused downward.
    pub incarnation: u32,
}

impl SessionId {
    /// Construct + validate: `uid` must be the Mayor slot or an agent slot.
    ///
    /// # Errors
    ///
    /// [`SessionError::InvalidUid`] outside {1000, 1001–2000}.
    pub fn new(uid: u32, incarnation: u32) -> Result<Self, SessionError> {
        if uid != MAYOR_UID && !(FIRST_AGENT_UID..=LAST_AGENT_UID).contains(&uid) {
            return Err(SessionError::InvalidUid(uid));
        }
        Ok(Self { uid, incarnation })
    }

    /// First life of a slot.
    ///
    /// # Errors
    ///
    /// Same as [`SessionId::new`].
    pub fn first(uid: u32) -> Result<Self, SessionError> {
        Self::new(uid, 0)
    }

    /// The next incarnation (supervised restart).
    ///
    /// # Errors
    ///
    /// [`SessionError::IncarnationOverflow`] at `u32::MAX` — a hard error,
    /// never a wrap (same spirit as slot exhaustion, CONVENTIONS §3 r5).
    pub fn next_incarnation(self) -> Result<Self, SessionError> {
        Ok(Self {
            uid: self.uid,
            incarnation: self
                .incarnation
                .checked_add(1)
                .ok_or(SessionError::IncarnationOverflow(self))?,
        })
    }

    /// Canonical short token `"<uid>.<incarnation>"` — sanitizer-charset
    /// safe, used inside correlation ids and telemetry labels.
    #[must_use]
    pub fn token(&self) -> String {
        format!("{}.{}", self.uid, self.incarnation)
    }

    /// The `session u64` component of ADR-0005 sled keys:
    /// `(incarnation << 32) | uid` — monotonic per slot, prefix-scan safe.
    #[must_use]
    pub fn as_u64(&self) -> u64 {
        (u64::from(self.incarnation) << 32) | u64::from(self.uid)
    }

    /// Wire `id` of this session's readiness probe ([`readiness_probe`]).
    #[must_use]
    pub fn probe_id(&self) -> String {
        format!("rdy-{}", self.token())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.token())
    }
}

// ---------------------------------------------------------------------------
// Session status (charter state) + transition table
// ---------------------------------------------------------------------------

/// Lifecycle state of a supervised session, recorded in the charter
/// `sessions` group (SPEC §6 supervision; ADR-0015 cl. 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Process spawned; readiness probe not yet answered.
    Starting,
    /// Ready; no task run yet. Accepts queued delivery.
    Running,
    /// A task run is in flight (`agent_start` seen, no `agent_settled` yet).
    Busy,
    /// Settled at a task boundary after ≥1 run. Accepts queued delivery.
    Idle,
    /// Process died unexpectedly (or readiness timed out). Restart policy
    /// decides between a new incarnation and permanent failure.
    Crashed,
    /// Cleanly stopped by the daemon (graceful drain or hibernation end).
    Stopped,
    /// Hibernation in progress (Sprint 07): drain → home backup → kill.
    Hibernating,
}

impl SessionStatus {
    /// Every variant, declaration order — exhaustive-matrix tests iterate
    /// this instead of hard-coding.
    pub const ALL: [Self; 7] = [
        Self::Starting,
        Self::Running,
        Self::Busy,
        Self::Idle,
        Self::Crashed,
        Self::Stopped,
        Self::Hibernating,
    ];

    /// Is the pi process expected to be alive in this state?
    #[must_use]
    pub fn is_alive(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Busy | Self::Idle | Self::Hibernating
        )
    }

    /// Is a queued delivery writable right now (task boundary reached)?
    #[must_use]
    pub fn accepts_delivery(self) -> bool {
        matches!(self, Self::Running | Self::Idle)
    }

    /// Closed transition table (ADR-0015 cl. 4). Self-transitions are
    /// allowed everywhere (idempotent re-observations: duplicate
    /// `agent_start` while busy, a second settle event, …).
    ///
    /// ```text
    /// Starting    → Running | Crashed | Stopped
    /// Running     → Busy | Idle | Crashed | Stopped | Hibernating
    /// Busy        → Idle | Crashed | Stopped | Hibernating
    /// Idle        → Busy | Crashed | Stopped | Hibernating
    /// Crashed     → Starting        (restart; new incarnation)
    /// Stopped     → Starting        (explicit operator restart)
    /// Hibernating → Stopped | Crashed
    /// ```
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        matches!(
            (self, next),
            (
                Self::Starting,
                Self::Running | Self::Crashed | Self::Stopped
            ) | (
                Self::Running,
                Self::Busy | Self::Idle | Self::Crashed | Self::Stopped | Self::Hibernating
            ) | (
                Self::Busy,
                Self::Idle | Self::Crashed | Self::Stopped | Self::Hibernating
            ) | (
                Self::Idle,
                Self::Busy | Self::Crashed | Self::Stopped | Self::Hibernating
            ) | (Self::Crashed, Self::Starting)
                | (Self::Stopped, Self::Starting)
                | (Self::Hibernating, Self::Stopped | Self::Crashed)
        )
    }

    /// Checked transition for supervisors.
    ///
    /// # Errors
    ///
    /// [`SessionError::IllegalTransition`] when the table forbids it.
    pub fn transition(self, next: Self) -> Result<Self, SessionError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(SessionError::IllegalTransition {
                from: self,
                to: next,
            })
        }
    }

    /// Pure reducer for agent-stream events: `agent_start` moves a live
    /// boundary state to [`SessionStatus::Busy`]; `agent_settled` returns to
    /// [`SessionStatus::Idle`]. Everything else (including responses, which
    /// need id-correlation the reducer deliberately doesn't guess) leaves
    /// the status unchanged — the supervisor drives readiness, exit, and
    /// hibernation transitions through [`SessionStatus::transition`].
    #[must_use]
    pub fn on_agent_event(self, event: &AgentEvent) -> Self {
        match (self, event) {
            (Self::Running | Self::Idle, AgentEvent::AgentStart) => Self::Busy,
            (Self::Busy, AgentEvent::AgentSettled) => Self::Idle,
            (current, _) => current,
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Busy => "busy",
            Self::Idle => "idle",
            Self::Crashed => "crashed",
            Self::Stopped => "stopped",
            Self::Hibernating => "hibernating",
        };
        f.write_str(name)
    }
}

/// Charter `sessions` group entry (charter v2): one live-or-last session per
/// UID slot. Key invariant (charter `validate`): map key == `id.uid`, and
/// the slot must exist in the slot table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    /// Identity: slot + incarnation.
    pub id: SessionId,
    /// Lifecycle state.
    pub status: SessionStatus,
    /// Consecutive supervised auto-restarts of the current life chain;
    /// reset to 0 when a run reaches a task boundary (`agent_settled`).
    /// Never a wall-clock input (CONVENTIONS §3 r4) — the backoff schedule
    /// derives from this count alone ([`restart_backoff_ms`]).
    pub restart_count: u32,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed session-protocol failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// UID outside {1000, 1001–2000}.
    InvalidUid(u32),
    /// Incarnation counter exhausted — hard error, never a wrap.
    IncarnationOverflow(SessionId),
    /// The transition table forbids this status change.
    IllegalTransition {
        /// Current status.
        from: SessionStatus,
        /// Attempted status.
        to: SessionStatus,
    },
    /// A queued delivery was attempted off-boundary, or an interrupt while
    /// the session has not reached readiness.
    NotAtTaskBoundary(SessionStatus),
    /// The session process is not alive (crashed/stopped/hibernating).
    NotAlive(SessionStatus),
    /// Envelope text must not be empty.
    EmptyText,
    /// Correlation id failed charset/length validation.
    InvalidCorrelation(String),
    /// A wire line could not be parsed into the expected record.
    Protocol(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUid(uid) => write!(
                f,
                "UID {uid} outside session range (Mayor {MAYOR_UID} or {FIRST_AGENT_UID}-{LAST_AGENT_UID})"
            ),
            Self::IncarnationOverflow(id) => {
                write!(f, "incarnation counter exhausted for slot {}", id.uid)
            }
            Self::IllegalTransition { from, to } => {
                write!(f, "illegal session transition {from} → {to}")
            }
            Self::NotAtTaskBoundary(status) => {
                write!(f, "session status {status} is not a task boundary")
            }
            Self::NotAlive(status) => write!(f, "session not alive (status {status})"),
            Self::EmptyText => write!(f, "envelope text must not be empty"),
            Self::InvalidCorrelation(raw) => {
                write!(
                    f,
                    "correlation id {raw:?} must be 1–64 chars of [A-Za-z0-9._-]"
                )
            }
            Self::Protocol(msg) => write!(f, "session protocol error: {msg}"),
        }
    }
}

impl std::error::Error for SessionError {}

// ---------------------------------------------------------------------------
// Correlation ids
// ---------------------------------------------------------------------------

/// Opaque, charset-validated correlation id tying an envelope to its wire
/// records, pi responses, and telemetry rows. Canonical forms:
/// `m-<seq>-<uid>.<inc>` (mail), `t-…` (tasking), `n-…` (notice),
/// `i-…` (interrupt). `<seq>` is allocated per session by the daemon
/// (sled `counters.v1`, ADR-0005 cl. 2).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CorrelationId(String);

impl CorrelationId {
    /// Validate an existing id (charset `[A-Za-z0-9._-]`, length 1–64).
    ///
    /// # Errors
    ///
    /// [`SessionError::InvalidCorrelation`] on any other byte.
    pub fn parse(raw: &str) -> Result<Self, SessionError> {
        let ok = !raw.is_empty()
            && raw.len() <= 64
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if ok {
            Ok(Self(raw.to_owned()))
        } else {
            Err(SessionError::InvalidCorrelation(raw.to_owned()))
        }
    }

    /// Id for a queued delivery of `kind`, sequence `seq`.
    ///
    /// # Errors
    ///
    /// Propagates [`CorrelationId::parse`] (cannot fail for valid inputs).
    pub fn for_queued(
        kind: QueuedKind,
        session: &SessionId,
        seq: u64,
    ) -> Result<Self, SessionError> {
        Self::parse(&format!("{}-{seq}-{}", kind.prefix(), session.token()))
    }

    /// Id for an interrupt, sequence `seq`.
    ///
    /// # Errors
    ///
    /// Propagates [`CorrelationId::parse`] (cannot fail for valid inputs).
    pub fn for_interrupt(session: &SessionId, seq: u64) -> Result<Self, SessionError> {
        Self::parse(&format!("i-{seq}-{}", session.token()))
    }

    /// The validated id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wire `id` for one record of this envelope's binding: `"<corr>:<suffix>"`
    /// (`p` prompt, `s` steer, `a` abort, `q` clear-queue). pi treats `id`
    /// as an opaque string; the suffix keeps multi-record interrupts
    /// individually correlatable in responses.
    #[must_use]
    pub fn wire_id(&self, suffix: &str) -> String {
        format!("{}:{suffix}", self.0)
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for CorrelationId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CorrelationId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Envelopes (daemon ↔ agent channel units)
// ---------------------------------------------------------------------------

/// Queued-delivery class. All three bind to the same pi wire record
/// (`prompt` at a task boundary); the class travels in the correlation id
/// and in persisted queue entries so bounces (Sprint 03 step 03) and
/// telemetry can attribute them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedKind {
    /// Courier mail (SPEC §8.1; header rendering is Sprint 04's ADR — the
    /// envelope carries the fully rendered `text`).
    Mail,
    /// Mayor tasking (INDENTURE-bound work assignment, SPEC §7).
    Tasking,
    /// System notice (daemon-originated informational message).
    Notice,
}

impl fmt::Display for QueuedKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Mail => "mail",
            Self::Tasking => "tasking",
            Self::Notice => "notice",
        };
        f.write_str(name)
    }
}

impl QueuedKind {
    /// Correlation-id prefix character.
    #[must_use]
    pub fn prefix(self) -> char {
        match self {
            Self::Mail => 'm',
            Self::Tasking => 't',
            Self::Notice => 'n',
        }
    }
}

/// Who originates a Divine Interruption (SPEC §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptSource {
    /// Daemon/system event (resource pressure, decree enforcement).
    System,
    /// Operator via CLI/TUI (`hamlet agent steer`).
    Human,
    /// The Mayor (UID 1000, Sprint 05 consumes).
    Mayor,
}

impl fmt::Display for InterruptSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::System => "system",
            Self::Human => "human",
            Self::Mayor => "mayor",
        };
        f.write_str(name)
    }
}

/// Interrupt urgency. Both bypass the queue; the difference is wire-visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptPriority {
    /// Interrupt the current turn; previously queued pi steering survives.
    Normal,
    /// Also drops pi's queued steering/follow-up messages first
    /// (`clear_queue` before the steer/abort pair).
    Critical,
}

impl fmt::Display for InterruptPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Normal => "normal",
            Self::Critical => "critical",
        };
        f.write_str(name)
    }
}

/// The Divine Interruption envelope (SPEC §6; wire schema ADR-0015 cl. 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptAndSteer {
    /// Ties the interrupt to its pi responses and telemetry rows.
    pub correlation: CorrelationId,
    /// Originator class — rendered into the steering text so the *session*
    /// can tell a human override from a Mayor or system one.
    pub source: InterruptSource,
    /// Queue-dropping urgency.
    pub priority: InterruptPriority,
    /// The steering instruction (non-empty).
    pub steering_text: String,
}

impl InterruptAndSteer {
    /// Agent-visible rendering: a bracketed hamlet header line followed by
    /// the steering text. The header makes interrupts distinguishable from
    /// queued mail *inside the message content* too, not only by command
    /// type (SPEC §6 requirement).
    #[must_use]
    pub fn render_text(&self) -> String {
        format!(
            "[hamlet-interrupt source={} priority={} corr={}]\n{}",
            self.source, self.priority, self.correlation, self.steering_text
        )
    }
}

/// One unit on the daemon↔agent channel. Queued deliveries (mail/tasking/
/// notice) wait for task boundaries; [`SessionEnvelope::InterruptAndSteer`]
/// bypasses the queue (SPEC §6). JSON shape is internally tagged by `kind`
/// — the tag *is* the queued-vs-interrupt distinction on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEnvelope {
    /// Courier mail delivery (rendered text, Sprint 04 owns the header
    /// format that produces it).
    Mail {
        /// Queue correlation.
        correlation: CorrelationId,
        /// Fully rendered message (non-empty; may contain newlines — JSONL
        /// escaping keeps the record one line).
        text: String,
    },
    /// Mayor tasking.
    Tasking {
        /// Queue correlation.
        correlation: CorrelationId,
        /// Fully rendered instruction (non-empty).
        text: String,
    },
    /// System notice.
    Notice {
        /// Queue correlation.
        correlation: CorrelationId,
        /// Fully rendered notice (non-empty).
        text: String,
    },
    /// Out-of-band Divine Interruption.
    InterruptAndSteer(InterruptAndSteer),
}

impl SessionEnvelope {
    /// Build a queued-mail envelope (correlation derived from `session`+`seq`).
    ///
    /// # Errors
    ///
    /// [`SessionError::EmptyText`] or correlation construction failures.
    pub fn mail(session: &SessionId, seq: u64, text: &str) -> Result<Self, SessionError> {
        Self::queued(QueuedKind::Mail, session, seq, text)
    }

    /// Build a Mayor-tasking envelope.
    ///
    /// # Errors
    ///
    /// Same as [`SessionEnvelope::mail`].
    pub fn tasking(session: &SessionId, seq: u64, text: &str) -> Result<Self, SessionError> {
        Self::queued(QueuedKind::Tasking, session, seq, text)
    }

    /// Build a system-notice envelope.
    ///
    /// # Errors
    ///
    /// Same as [`SessionEnvelope::mail`].
    pub fn notice(session: &SessionId, seq: u64, text: &str) -> Result<Self, SessionError> {
        Self::queued(QueuedKind::Notice, session, seq, text)
    }

    /// Build an interrupt envelope.
    ///
    /// # Errors
    ///
    /// [`SessionError::EmptyText`] (also whitespace-only) or correlation
    /// construction failures.
    pub fn interrupt(
        session: &SessionId,
        seq: u64,
        source: InterruptSource,
        priority: InterruptPriority,
        steering_text: &str,
    ) -> Result<Self, SessionError> {
        if steering_text.trim().is_empty() {
            return Err(SessionError::EmptyText);
        }
        Ok(Self::InterruptAndSteer(InterruptAndSteer {
            correlation: CorrelationId::for_interrupt(session, seq)?,
            source,
            priority,
            steering_text: steering_text.to_owned(),
        }))
    }

    fn queued(
        kind: QueuedKind,
        session: &SessionId,
        seq: u64,
        text: &str,
    ) -> Result<Self, SessionError> {
        if text.trim().is_empty() {
            return Err(SessionError::EmptyText);
        }
        let correlation = CorrelationId::for_queued(kind, session, seq)?;
        Ok(match kind {
            QueuedKind::Mail => Self::Mail {
                correlation,
                text: text.to_owned(),
            },
            QueuedKind::Tasking => Self::Tasking {
                correlation,
                text: text.to_owned(),
            },
            QueuedKind::Notice => Self::Notice {
                correlation,
                text: text.to_owned(),
            },
        })
    }

    /// The envelope's correlation id (both channels carry one).
    #[must_use]
    pub fn correlation(&self) -> &CorrelationId {
        match self {
            Self::Mail { correlation, .. }
            | Self::Tasking { correlation, .. }
            | Self::Notice { correlation, .. } => correlation,
            Self::InterruptAndSteer(interrupt) => &interrupt.correlation,
        }
    }

    /// Is this the out-of-band channel?
    #[must_use]
    pub fn is_interrupt(&self) -> bool {
        matches!(self, Self::InterruptAndSteer(_))
    }

    /// The queued-delivery class, or `None` for the out-of-band interrupt
    /// channel. Used by the delivery queue for bounce attribution and
    /// trace rendering (Sprint 03 step 03).
    #[must_use]
    pub fn queued_kind(&self) -> Option<QueuedKind> {
        match self {
            Self::Mail { .. } => Some(QueuedKind::Mail),
            Self::Tasking { .. } => Some(QueuedKind::Tasking),
            Self::Notice { .. } => Some(QueuedKind::Notice),
            Self::InterruptAndSteer(_) => None,
        }
    }

    /// Canonical JSONL record (compact, no trailing LF — the writer owns
    /// framing). This is also the queue-persistence encoding.
    ///
    /// # Panics
    ///
    /// Never in practice: envelope serialization is total over validated
    /// values (strings only).
    #[must_use]
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).expect("SessionEnvelope serialization is total")
    }

    /// Parse one envelope JSONL line (trailing LF/CR tolerated).
    ///
    /// # Errors
    ///
    /// [`SessionError::Protocol`] on malformed JSON, unknown `kind`,
    /// invalid correlation, or empty text.
    pub fn from_json_line(line: &str) -> Result<Self, SessionError> {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
        if trimmed.trim().is_empty() {
            return Err(SessionError::Protocol("empty envelope line".to_owned()));
        }
        let envelope: Self = serde_json::from_str(trimmed)
            .map_err(|e| SessionError::Protocol(format!("envelope parse: {e}")))?;
        envelope.validate()?;
        Ok(envelope)
    }

    /// Post-parse invariants: non-empty payloads.
    ///
    /// # Errors
    ///
    /// [`SessionError::EmptyText`] when a persisted envelope lost its body.
    pub fn validate(&self) -> Result<(), SessionError> {
        let empty = match self {
            Self::Mail { text, .. } | Self::Tasking { text, .. } | Self::Notice { text, .. } => {
                text.trim().is_empty()
            }
            Self::InterruptAndSteer(interrupt) => interrupt.steering_text.trim().is_empty(),
        };
        if empty {
            Err(SessionError::EmptyText)
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// pi wire dialect (stdin commands)
// ---------------------------------------------------------------------------

/// A pi RPC-mode stdin command (pi docs `rpc-commands.md`; pi 0.99.1).
/// Only the five commands hamlet ever sends are modeled; the enum is closed
/// on purpose — new commands require a protocol review, not a drive-by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PiCommand {
    /// Start a task run (or, mid-stream without `streamingBehavior`, pi
    /// rejects — hamlet never sends it mid-stream by construction).
    Prompt {
        /// Correlation `id` echoed by pi's response.
        id: String,
        /// User-visible message text.
        message: String,
    },
    /// Queue a steering message delivered at the next LLM-call boundary.
    Steer {
        /// Correlation `id`.
        id: String,
        /// Steering text.
        message: String,
    },
    /// Interrupt the current run; responds once the session is idle and
    /// *continues* messages still queued via `steer` (ADR-0015 cl. 3).
    Abort {
        /// Correlation `id`.
        id: String,
    },
    /// Drop pi's queued steering/follow-up messages.
    ClearQueue {
        /// Correlation `id`.
        id: String,
    },
    /// State probe — doubles as the readiness handshake (ADR-0015 cl. 3).
    GetState {
        /// Correlation `id` (compare against [`SessionId::probe_id`]).
        id: String,
    },
}

impl PiCommand {
    /// The record's `id` — the wire handle a `response` echoes back, and
    /// the join key between an envelope and its stream acknowledgment
    /// (`<correlation>:<suffix>` for hamlet-minted records).
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Prompt { id, .. }
            | Self::Steer { id, .. }
            | Self::Abort { id }
            | Self::ClearQueue { id }
            | Self::GetState { id } => id,
        }
    }

    /// Canonical JSONL record (compact, no trailing LF).
    ///
    /// # Panics
    ///
    /// Never in practice: serialization is total over string fields.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).expect("PiCommand serialization is total")
    }
}

/// Parse one pi-dialect stdin line (used by the FakeAgent and by tests that
/// assert on wire bytes). Trailing LF/CR tolerated.
///
/// # Errors
///
/// [`SessionError::Protocol`] on malformed JSON or a command outside the
/// closed [`PiCommand`] set (cue records like `fake_cue` are *not* pi
/// dialect and must be intercepted before this call).
pub fn parse_pi_command(line: &str) -> Result<PiCommand, SessionError> {
    let trimmed = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    serde_json::from_str(trimmed)
        .map_err(|e| SessionError::Protocol(format!("pi command parse: {e}")))
}

/// The readiness handshake command for a session life: `get_state` with id
/// [`SessionId::probe_id`]. The supervisor sends it right after spawn; the
/// matching successful [`AgentEvent::Response`] performs Starting → Running
/// (ADR-0015 cl. 3 — pi RPC mode emits no startup banner).
#[must_use]
pub fn readiness_probe(session: &SessionId) -> PiCommand {
    PiCommand::GetState {
        id: session.probe_id(),
    }
}

/// Project an envelope onto its pi-wire records for a session in `status`
/// (ADR-0015 cl. 3). The daemon's single stdin writer task owns ordering:
/// records of one envelope are written contiguously, in order.
///
/// # Errors
///
/// - [`SessionError::NotAtTaskBoundary`] — queued delivery while the
///   session is busy/starting, or interrupt while starting.
/// - [`SessionError::NotAlive`] — anything against a crashed, stopped, or
///   hibernating session.
pub fn pi_binding(
    envelope: &SessionEnvelope,
    status: SessionStatus,
) -> Result<Vec<PiCommand>, SessionError> {
    if matches!(
        status,
        SessionStatus::Crashed | SessionStatus::Stopped | SessionStatus::Hibernating
    ) {
        return Err(SessionError::NotAlive(status));
    }
    match envelope {
        SessionEnvelope::Mail { correlation, text }
        | SessionEnvelope::Tasking { correlation, text }
        | SessionEnvelope::Notice { correlation, text } => {
            if !status.accepts_delivery() {
                return Err(SessionError::NotAtTaskBoundary(status));
            }
            Ok(vec![PiCommand::Prompt {
                id: correlation.wire_id("p"),
                message: text.clone(),
            }])
        }
        SessionEnvelope::InterruptAndSteer(interrupt) => {
            let correlation = &interrupt.correlation;
            if status == SessionStatus::Starting {
                return Err(SessionError::NotAtTaskBoundary(status));
            }
            if status == SessionStatus::Busy {
                let steer = PiCommand::Steer {
                    id: correlation.wire_id("s"),
                    message: interrupt.render_text(),
                };
                let abort = PiCommand::Abort {
                    id: correlation.wire_id("a"),
                };
                let mut records = Vec::with_capacity(3);
                if interrupt.priority == InterruptPriority::Critical {
                    records.push(PiCommand::ClearQueue {
                        id: correlation.wire_id("q"),
                    });
                }
                records.push(steer);
                records.push(abort);
                Ok(records)
            } else {
                // Running/Idle: nothing in flight to interrupt — deliver as
                // a fresh task, still header-marked as an interrupt.
                Ok(vec![PiCommand::Prompt {
                    id: correlation.wire_id("p"),
                    message: interrupt.render_text(),
                }])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// pi wire dialect (stdout events)
// ---------------------------------------------------------------------------

/// What pi reported about a submitted command (`response.data.disposition`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// pi accepted the prompt and started a run.
    Started,
    /// pi queued the message (steer/follow-up, or prompt during a run).
    Queued,
    /// An extension/input handler consumed it; no run follows.
    Handled,
    /// Forward-compat: pi added a value hamlet doesn't model yet.
    Other(String),
}

impl Disposition {
    fn parse(raw: &str) -> Self {
        match raw {
            "started" => Self::Started,
            "queued" => Self::Queued,
            "handled" => Self::Handled,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// Token/cost accounting attached to `message_update` events (pi docs
/// `json.md`). Sprint 03 step 04 aggregates these into sled; the schema is
/// fixed here so the aggregation has a typed target.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Usage {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-write tokens.
    pub cache_write: u64,
    /// Total tokens.
    pub total_tokens: u64,
    /// Cost breakdown (USD).
    pub cost: Cost,
}

/// Cost component of [`Usage`] (USD floats, as pi reports them).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cost {
    /// Input cost.
    pub input: f64,
    /// Output cost.
    pub output: f64,
    /// Cache-read cost.
    pub cache_read: f64,
    /// Cache-write cost.
    pub cache_write: f64,
    /// Total cost.
    pub total: f64,
}

/// One stdout JSONL record from a session, typed for the fields hamlet
/// consumes (supervision + telemetry). Unknown record types are preserved
/// as [`AgentEvent::Other`] — the parser is forward-compatible and never
/// fatal on new pi events; malformed lines become [`AgentEvent::Malformed`]
/// (traced + skipped, never a supervisor crash — Sprint 03 step 04).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum AgentEvent {
    /// Command acknowledgment (`response`). `id` is `None` for pi's
    /// record-level `parse` failures.
    Response {
        /// Echoed command `id`, when the command carried one.
        id: Option<String>,
        /// Command name (`"prompt"`, `"steer"`, `"abort"`, `"parse"`, …).
        command: String,
        /// Command-handling success.
        success: bool,
        /// Error text when `success` is false.
        error: Option<String>,
        /// `data.disposition` for prompt/steer/follow-up responses.
        disposition: Option<Disposition>,
    },
    /// A low-level agent run started.
    AgentStart,
    /// A low-level run ended; retries/steering may still follow — this is
    /// *not* a task boundary.
    AgentEnd {
        /// pi will retry automatically.
        will_retry: bool,
    },
    /// **The task boundary**: pi will not continue automatically (no
    /// retries, compaction, or queued messages pending).
    AgentSettled,
    /// One assistant turn started.
    TurnStart,
    /// One assistant turn finished.
    TurnEnd,
    /// Streaming content update carrying cumulative [`Usage`].
    MessageUpdate {
        /// Usage snapshot, when present and well-formed.
        usage: Option<Usage>,
    },
    /// A record type hamlet doesn't model (yet) — preserved verbatim.
    Other {
        /// The record's `type` value.
        record_type: String,
        /// Full raw record.
        raw: Value,
    },
    /// A line that is not a valid JSONL record. Never fatal; `line` keeps
    /// the raw bytes for tracing (truncate before logging at the call site).
    Malformed {
        /// The offending line (without framing newline).
        line: String,
        /// Why parsing failed.
        reason: String,
    },
}

impl AgentEvent {
    /// Is this the task-boundary event (queued delivery may resume)?
    #[must_use]
    pub fn is_task_boundary(&self) -> bool {
        matches!(self, Self::AgentSettled)
    }
}

/// Parse one stdout JSONL line into an [`AgentEvent`]. Infallible in the
/// typed sense: anything unrecognized degrades to `Other`/`Malformed`
/// instead of failing (the stream is adversarial input — CONVENTIONS §9).
#[must_use]
pub fn parse_agent_event(line: &str) -> AgentEvent {
    let trimmed = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    if trimmed.trim().is_empty() {
        return AgentEvent::Malformed {
            line: trimmed.to_owned(),
            reason: "empty line".to_owned(),
        };
    }
    let raw: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(e) => {
            return AgentEvent::Malformed {
                line: trimmed.to_owned(),
                reason: format!("json: {e}"),
            }
        }
    };
    let record_type = match raw.get("type").and_then(Value::as_str) {
        Some(ty) => ty.to_owned(),
        None => {
            return AgentEvent::Malformed {
                line: trimmed.to_owned(),
                reason: "record has no string \"type\"".to_owned(),
            }
        }
    };
    match record_type.as_str() {
        "response" => {
            let command = match raw.get("command").and_then(Value::as_str) {
                Some(command) => command.to_owned(),
                None => return AgentEvent::Other { record_type, raw },
            };
            let success = raw.get("success").and_then(Value::as_bool).unwrap_or(false);
            AgentEvent::Response {
                id: raw.get("id").and_then(Value::as_str).map(str::to_owned),
                command,
                success,
                error: raw.get("error").and_then(Value::as_str).map(str::to_owned),
                disposition: raw
                    .get("data")
                    .and_then(|data| data.get("disposition"))
                    .and_then(Value::as_str)
                    .map(Disposition::parse),
            }
        }
        "agent_start" => AgentEvent::AgentStart,
        "agent_settled" => AgentEvent::AgentSettled,
        "turn_start" => AgentEvent::TurnStart,
        "turn_end" => AgentEvent::TurnEnd,
        "agent_end" => AgentEvent::AgentEnd {
            will_retry: raw
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "message_update" => match raw
            .get("usage")
            .map(|usage| serde_json::from_value::<Usage>(usage.clone()))
        {
            None => AgentEvent::MessageUpdate { usage: None },
            Some(Ok(usage)) => AgentEvent::MessageUpdate { usage: Some(usage) },
            Some(Err(_)) => AgentEvent::Other { record_type, raw },
        },
        _ => AgentEvent::Other { record_type, raw },
    }
}

// ---------------------------------------------------------------------------
// Restart policy math (pure; config keys in `crate::config::SessionsConfig`)
// ---------------------------------------------------------------------------

/// Deterministic restart backoff: the delay before auto-restart number
/// `restart_count + 1`, or [`None`] when the policy is exhausted (the
/// session stays [`SessionStatus::Crashed`] permanently and the supervisor
/// stamps TITLE.md lineage — Sprint 03 step 02).
///
/// `delay(n) = min(backoff_initial_ms · 2ⁿ, backoff_max_ms)` with
/// `n = restart_count` (0-based). No wall-clock input: the schedule is a
/// pure function of the persisted counter (CONVENTIONS §3 r4).
#[must_use]
pub fn restart_backoff_ms(
    backoff_initial_ms: u64,
    backoff_max_ms: u64,
    max_restarts: u32,
    restart_count: u32,
) -> Option<u64> {
    if restart_count >= max_restarts {
        return None;
    }
    let delay = backoff_initial_ms
        .saturating_mul(2u64.saturating_pow(restart_count))
        .min(backoff_max_ms);
    Some(delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(uid: u32, incarnation: u32) -> SessionId {
        SessionId::new(uid, incarnation).unwrap()
    }

    // --- SessionId ---------------------------------------------------------

    #[test]
    fn session_id_uid_matrix() {
        for uid in [MAYOR_UID, FIRST_AGENT_UID, 1500, LAST_AGENT_UID] {
            assert!(SessionId::new(uid, 0).is_ok(), "uid {uid} must be valid");
        }
        for uid in [0, 999, 2001, u32::MAX] {
            assert_eq!(SessionId::new(uid, 7), Err(SessionError::InvalidUid(uid)));
        }
    }

    /// Golden: token + sled-key encoding (ADR-0005 `session u64`).
    #[test]
    fn session_id_token_and_u64_golden() {
        let s = sid(1001, 3);
        assert_eq!(s.token(), "1001.3");
        assert_eq!(s.to_string(), "1001.3");
        assert_eq!(s.as_u64(), (3u64 << 32) | 1001);
        assert_eq!(sid(1000, 0).as_u64(), 1000);
        // Monotonic per slot: later incarnations sort above earlier ones.
        assert!(sid(1001, 4).as_u64() > sid(1001, 3).as_u64());
        assert_eq!(s.probe_id(), "rdy-1001.3");
    }

    #[test]
    fn incarnation_bumps_and_never_wraps() {
        assert_eq!(sid(1002, 0).next_incarnation().unwrap(), sid(1002, 1));
        assert_eq!(
            sid(1002, u32::MAX).next_incarnation(),
            Err(SessionError::IncarnationOverflow(sid(1002, u32::MAX)))
        );
    }

    // --- SessionStatus -----------------------------------------------------

    /// Type-level test: the full 7×7 transition matrix against the ADR-0015
    /// cl. 6 table (self-loops allowed everywhere).
    #[test]
    fn transition_matrix_is_closed_and_exact() {
        let allowed: [(&str, &[&str]); 7] = [
            ("starting", &["starting", "running", "crashed", "stopped"]),
            (
                "running",
                &[
                    "running",
                    "busy",
                    "idle",
                    "crashed",
                    "stopped",
                    "hibernating",
                ],
            ),
            (
                "busy",
                &["busy", "idle", "crashed", "stopped", "hibernating"],
            ),
            (
                "idle",
                &["idle", "busy", "crashed", "stopped", "hibernating"],
            ),
            ("crashed", &["crashed", "starting"]),
            ("stopped", &["stopped", "starting"]),
            ("hibernating", &["hibernating", "stopped", "crashed"]),
        ];
        let by_name = |name: &str| -> SessionStatus {
            SessionStatus::ALL
                .into_iter()
                .find(|s| s.to_string() == name)
                .unwrap()
        };
        // Every variant appears exactly once as a source (exhaustiveness).
        assert_eq!(SessionStatus::ALL.len(), allowed.len());
        for (from_name, targets) in allowed {
            let from = by_name(from_name);
            for to in SessionStatus::ALL {
                let expected = targets.iter().any(|t| by_name(t) == to);
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "{from} → {to} mismatch"
                );
                match from.transition(to) {
                    Ok(next) => assert!(expected && next == to),
                    Err(SessionError::IllegalTransition { from: f, to: t }) => {
                        assert!(!expected && f == from && t == to)
                    }
                    Err(other) => panic!("unexpected error {other}"),
                }
            }
        }
    }

    #[test]
    fn status_predicates() {
        for status in SessionStatus::ALL {
            let alive = status.is_alive();
            assert_eq!(
                alive,
                !matches!(status, SessionStatus::Crashed | SessionStatus::Stopped),
                "{status} aliveness"
            );
            let accepts = status.accepts_delivery();
            assert_eq!(
                accepts,
                matches!(status, SessionStatus::Running | SessionStatus::Idle),
                "{status} delivery"
            );
            // Delivery acceptance implies aliveness (type-level invariant).
            assert!(!accepts || alive);
        }
    }

    /// Golden serde: statuses are snake_case in charter JSON.
    #[test]
    fn status_serde_golden() {
        let json = serde_json::to_string(&SessionStatus::Hibernating).unwrap();
        assert_eq!(json, "\"hibernating\"");
        for status in SessionStatus::ALL {
            let text = serde_json::to_string(&status).unwrap();
            assert_eq!(
                serde_json::from_str::<SessionStatus>(&text).unwrap(),
                status
            );
        }
    }

    #[test]
    fn on_agent_event_reducer() {
        assert_eq!(
            SessionStatus::Idle.on_agent_event(&AgentEvent::AgentStart),
            SessionStatus::Busy
        );
        assert_eq!(
            SessionStatus::Running.on_agent_event(&AgentEvent::AgentStart),
            SessionStatus::Busy
        );
        // Duplicate agent_start while busy is idempotent, not illegal.
        assert_eq!(
            SessionStatus::Busy.on_agent_event(&AgentEvent::AgentStart),
            SessionStatus::Busy
        );
        // agent_end is NOT the boundary; only agent_settled is.
        let end = AgentEvent::AgentEnd { will_retry: true };
        assert_eq!(
            SessionStatus::Busy.on_agent_event(&end),
            SessionStatus::Busy
        );
        assert_eq!(
            SessionStatus::Busy.on_agent_event(&AgentEvent::AgentSettled),
            SessionStatus::Idle
        );
        // Non-alive states are untouched by stream events (supervisor owns
        // those transitions).
        assert_eq!(
            SessionStatus::Crashed.on_agent_event(&AgentEvent::AgentStart),
            SessionStatus::Crashed
        );
        assert_eq!(
            SessionStatus::Starting.on_agent_event(&AgentEvent::AgentSettled),
            SessionStatus::Starting
        );
    }

    // --- CorrelationId -----------------------------------------------------

    #[test]
    fn correlation_id_golden_forms() {
        let s = sid(1001, 2);
        assert_eq!(
            CorrelationId::for_queued(QueuedKind::Mail, &s, 7)
                .unwrap()
                .as_str(),
            "m-7-1001.2"
        );
        assert_eq!(
            CorrelationId::for_queued(QueuedKind::Tasking, &s, 8)
                .unwrap()
                .as_str(),
            "t-8-1001.2"
        );
        assert_eq!(
            CorrelationId::for_queued(QueuedKind::Notice, &s, 9)
                .unwrap()
                .as_str(),
            "n-9-1001.2"
        );
        assert_eq!(
            CorrelationId::for_interrupt(&s, 3).unwrap().as_str(),
            "i-3-1001.2"
        );
        assert_eq!(
            CorrelationId::for_interrupt(&s, 3).unwrap().wire_id("s"),
            "i-3-1001.2:s"
        );
    }

    #[test]
    fn correlation_id_charset_rejections() {
        for bad in [
            "",
            "a b",
            "colon:id",
            "slash/id",
            "sem;i",
            "ünïcode",
            &"x".repeat(65),
        ] {
            assert_eq!(
                CorrelationId::parse(bad),
                Err(SessionError::InvalidCorrelation(bad.to_owned())),
                "{bad:?} must be rejected"
            );
        }
        assert!(CorrelationId::parse("m-7-1001.2").is_ok());
        assert!(CorrelationId::parse(&"x".repeat(64)).is_ok());
    }

    // --- Envelope goldens --------------------------------------------------

    /// Committed golden vectors: the exact canonical JSONL for every
    /// envelope variant (queue-persistence + FakeAgent contract bytes).
    #[test]
    fn envelope_json_golden_vectors() {
        let s = sid(1001, 2);
        let cases = [
            (
                SessionEnvelope::mail(&s, 7, "From: grace\nSubject: report\nBody: done").unwrap(),
                r#"{"kind":"mail","correlation":"m-7-1001.2","text":"From: grace\nSubject: report\nBody: done"}"#,
            ),
            (
                SessionEnvelope::tasking(&s, 8, "Build the west wall per INDENTURE §3").unwrap(),
                r#"{"kind":"tasking","correlation":"t-8-1001.2","text":"Build the west wall per INDENTURE §3"}"#,
            ),
            (
                SessionEnvelope::notice(&s, 9, "Parcel quota at 80%").unwrap(),
                r#"{"kind":"notice","correlation":"n-9-1001.2","text":"Parcel quota at 80%"}"#,
            ),
            (
                SessionEnvelope::interrupt(
                    &s,
                    3,
                    InterruptSource::Human,
                    InterruptPriority::Normal,
                    "Stop digging; redirect to the gatehouse",
                )
                .unwrap(),
                r#"{"kind":"interrupt_and_steer","correlation":"i-3-1001.2","source":"human","priority":"normal","steering_text":"Stop digging; redirect to the gatehouse"}"#,
            ),
            (
                SessionEnvelope::interrupt(
                    &s,
                    4,
                    InterruptSource::Mayor,
                    InterruptPriority::Critical,
                    "Abandon current task; take commission 2",
                )
                .unwrap(),
                r#"{"kind":"interrupt_and_steer","correlation":"i-4-1001.2","source":"mayor","priority":"critical","steering_text":"Abandon current task; take commission 2"}"#,
            ),
        ];
        for (envelope, golden) in cases {
            let line = envelope.to_json_line();
            assert_eq!(line, golden, "envelope golden drifted");
            assert!(!line.contains('\n'), "JSONL record must be one line");
            assert_eq!(
                SessionEnvelope::from_json_line(golden).unwrap(),
                envelope,
                "golden must round-trip"
            );
            // Tolerant of writer framing:
            assert_eq!(
                SessionEnvelope::from_json_line(&format!("{golden}\n")).unwrap(),
                envelope
            );
        }
    }

    #[test]
    fn envelope_channel_distinction_is_structural() {
        let s = sid(1001, 0);
        let queued = [
            SessionEnvelope::mail(&s, 1, "m").unwrap(),
            SessionEnvelope::tasking(&s, 2, "t").unwrap(),
            SessionEnvelope::notice(&s, 3, "n").unwrap(),
        ];
        let interrupt = SessionEnvelope::interrupt(
            &s,
            4,
            InterruptSource::System,
            InterruptPriority::Normal,
            "halt",
        )
        .unwrap();
        for envelope in &queued {
            assert!(!envelope.is_interrupt());
            assert_eq!(
                envelope.correlation().as_str().chars().next(),
                Some(match envelope {
                    SessionEnvelope::Mail { .. } => 'm',
                    SessionEnvelope::Tasking { .. } => 't',
                    _ => 'n',
                })
            );
        }
        assert!(interrupt.is_interrupt());
        assert!(interrupt.correlation().as_str().starts_with("i-"));
    }

    #[test]
    fn envelope_rejects_empty_and_malformed() {
        let s = sid(1001, 0);
        assert_eq!(
            SessionEnvelope::mail(&s, 1, ""),
            Err(SessionError::EmptyText)
        );
        assert_eq!(
            SessionEnvelope::mail(&s, 1, "  \n\t "),
            Err(SessionError::EmptyText)
        );
        assert_eq!(
            SessionEnvelope::interrupt(
                &s,
                1,
                InterruptSource::Human,
                InterruptPriority::Normal,
                " "
            ),
            Err(SessionError::EmptyText)
        );
        for bad in [
            "",
            "\n",
            "not json",
            r#"{"kind":"carrier_pigeon","correlation":"m-1-1001.0","text":"x"}"#,
            r#"{"kind":"mail","correlation":"m:1","text":"x"}"#,
            r#"{"kind":"mail","correlation":"m-1-1001.0"}"#,
            r#"{"kind":"mail","correlation":"m-1-1001.0","text":"  "}"#,
            r#"{"kind":"interrupt_and_steer","correlation":"i-1-1001.0","source":"alien","priority":"normal","steering_text":"x"}"#,
        ] {
            assert!(
                matches!(
                    SessionEnvelope::from_json_line(bad),
                    Err(SessionError::Protocol(_) | SessionError::EmptyText)
                ),
                "{bad:?} must be rejected"
            );
        }
    }

    // --- pi wire binding ---------------------------------------------------

    /// Golden: queued delivery binds to a single `prompt` record.
    #[test]
    fn binding_queued_mail_golden() {
        let s = sid(1001, 2);
        let envelope = SessionEnvelope::mail(&s, 7, "hello parcel").unwrap();
        for status in [SessionStatus::Running, SessionStatus::Idle] {
            let records = pi_binding(&envelope, status).unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(
                records[0].to_json_line(),
                r#"{"type":"prompt","id":"m-7-1001.2:p","message":"hello parcel"}"#
            );
            // FakeAgent must be able to read its own dialect back.
            assert_eq!(
                parse_pi_command(&records[0].to_json_line()).unwrap(),
                records[0]
            );
        }
        // Mid-task: refused (queued mail waits for the boundary).
        assert_eq!(
            pi_binding(&envelope, SessionStatus::Busy),
            Err(SessionError::NotAtTaskBoundary(SessionStatus::Busy))
        );
        assert_eq!(
            pi_binding(&envelope, SessionStatus::Starting),
            Err(SessionError::NotAtTaskBoundary(SessionStatus::Starting))
        );
    }

    /// Golden: the interrupt_and_steer wire schema (ADR-0015 cl. 3).
    #[test]
    fn binding_interrupt_golden() {
        let s = sid(1001, 2);
        let normal = SessionEnvelope::interrupt(
            &s,
            3,
            InterruptSource::Human,
            InterruptPriority::Normal,
            "stop and rebuild the gate",
        )
        .unwrap();
        let records = pi_binding(&normal, SessionStatus::Busy).unwrap();
        let lines: Vec<String> = records.iter().map(PiCommand::to_json_line).collect();
        assert_eq!(
            lines,
            vec![
                r#"{"type":"steer","id":"i-3-1001.2:s","message":"[hamlet-interrupt source=human priority=normal corr=i-3-1001.2]\nstop and rebuild the gate"}"#.to_owned(),
                r#"{"type":"abort","id":"i-3-1001.2:a"}"#.to_owned(),
            ]
        );

        let critical = SessionEnvelope::interrupt(
            &s,
            4,
            InterruptSource::System,
            InterruptPriority::Critical,
            "drop everything",
        )
        .unwrap();
        let records = pi_binding(&critical, SessionStatus::Busy).unwrap();
        let lines: Vec<String> = records.iter().map(PiCommand::to_json_line).collect();
        assert_eq!(
            lines,
            vec![
                r#"{"type":"clear_queue","id":"i-4-1001.2:q"}"#.to_owned(),
                r#"{"type":"steer","id":"i-4-1001.2:s","message":"[hamlet-interrupt source=system priority=critical corr=i-4-1001.2]\ndrop everything"}"#.to_owned(),
                r#"{"type":"abort","id":"i-4-1001.2:a"}"#.to_owned(),
            ]
        );

        // Idle/Running: nothing in flight — single header-marked prompt.
        let records = pi_binding(&normal, SessionStatus::Idle).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].to_json_line(),
            r#"{"type":"prompt","id":"i-3-1001.2:p","message":"[hamlet-interrupt source=human priority=normal corr=i-3-1001.2]\nstop and rebuild the gate"}"#
        );

        // Not-yet-ready and dead sessions refuse interrupts.
        assert_eq!(
            pi_binding(&normal, SessionStatus::Starting),
            Err(SessionError::NotAtTaskBoundary(SessionStatus::Starting))
        );
        for dead in [
            SessionStatus::Crashed,
            SessionStatus::Stopped,
            SessionStatus::Hibernating,
        ] {
            assert_eq!(pi_binding(&normal, dead), Err(SessionError::NotAlive(dead)));
            let queued = SessionEnvelope::mail(&s, 1, "x").unwrap();
            assert_eq!(pi_binding(&queued, dead), Err(SessionError::NotAlive(dead)));
        }
    }

    #[test]
    fn readiness_probe_golden() {
        let s = sid(1001, 0);
        assert_eq!(
            readiness_probe(&s).to_json_line(),
            r#"{"type":"get_state","id":"rdy-1001.0"}"#
        );
    }

    // --- Event parsing -----------------------------------------------------

    #[test]
    fn event_parse_goldens() {
        // Prompt acceptance with disposition.
        assert_eq!(
            parse_agent_event(
                r#"{"id":"m-7-1001.2:p","type":"response","command":"prompt","success":true,"data":{"disposition":"started"}}"#
            ),
            AgentEvent::Response {
                id: Some("m-7-1001.2:p".to_owned()),
                command: "prompt".to_owned(),
                success: true,
                error: None,
                disposition: Some(Disposition::Started),
            }
        );
        // Failed response, no id (pi's parse failure shape).
        assert_eq!(
            parse_agent_event(
                r#"{"type":"response","command":"parse","success":false,"error":"Failed to parse command: Unexpected token..."}"#
            ),
            AgentEvent::Response {
                id: None,
                command: "parse".to_owned(),
                success: false,
                error: Some("Failed to parse command: Unexpected token...".to_owned()),
                disposition: None,
            }
        );
        // Lifecycle events.
        assert_eq!(
            parse_agent_event(r#"{"type":"agent_start"}"#),
            AgentEvent::AgentStart
        );
        assert_eq!(
            parse_agent_event(r#"{"type":"agent_settled"}"#),
            AgentEvent::AgentSettled
        );
        assert_eq!(
            parse_agent_event(r#"{"type":"turn_start"}"#),
            AgentEvent::TurnStart
        );
        assert_eq!(
            parse_agent_event(r#"{"type":"turn_end","message":{}}"#),
            AgentEvent::TurnEnd
        );
        assert_eq!(
            parse_agent_event(r#"{"type":"agent_end","messages":[],"willRetry":true}"#),
            AgentEvent::AgentEnd { will_retry: true }
        );
        assert_eq!(
            parse_agent_event(r#"{"type":"agent_end"}"#),
            AgentEvent::AgentEnd { will_retry: false }
        );
        // Usage-carrying update (values verbatim from pi docs json.md).
        assert_eq!(
            parse_agent_event(
                r#"{"type":"message_update","usage":{"input":100,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":101,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello "}}"#
            ),
            AgentEvent::MessageUpdate {
                usage: Some(Usage {
                    input: 100,
                    output: 1,
                    cache_read: 0,
                    cache_write: 0,
                    total_tokens: 101,
                    cost: Cost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                        total: 0.0
                    },
                }),
            }
        );
        assert_eq!(
            parse_agent_event(r#"{"type":"message_update"}"#),
            AgentEvent::MessageUpdate { usage: None }
        );
    }

    #[test]
    fn event_parse_is_never_fatal() {
        // Framing tolerance: trailing LF and CRLF.
        assert_eq!(
            parse_agent_event("{\"type\":\"agent_start\"}\n"),
            AgentEvent::AgentStart
        );
        assert_eq!(
            parse_agent_event("{\"type\":\"agent_start\"}\r\n"),
            AgentEvent::AgentStart
        );
        // Garbage → Malformed, unknown → Other (forward-compat with new pi
        // event types), missing type → Malformed.
        match parse_agent_event("{not json at all") {
            AgentEvent::Malformed { line, reason } => {
                assert_eq!(line, "{not json at all");
                assert!(reason.starts_with("json: "), "{reason}");
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        match parse_agent_event("") {
            AgentEvent::Malformed { reason, .. } => assert_eq!(reason, "empty line"),
            other => panic!("expected Malformed, got {other:?}"),
        }
        match parse_agent_event(r#"{"noType":true}"#) {
            AgentEvent::Malformed { reason, .. } => {
                assert_eq!(reason, "record has no string \"type\"")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        match parse_agent_event(
            r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"bash","isError":false}"#,
        ) {
            AgentEvent::Other { record_type, raw } => {
                assert_eq!(record_type, "tool_execution_end");
                assert_eq!(raw.get("toolName").and_then(Value::as_str), Some("bash"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
        // A usage block of the wrong shape degrades to Other (kept raw for
        // tracing), never a panic.
        match parse_agent_event(r#"{"type":"message_update","usage":"lots"}"#) {
            AgentEvent::Other { record_type, .. } => assert_eq!(record_type, "message_update"),
            other => panic!("expected Other, got {other:?}"),
        }
        // A response without a command name is Other, not a crash.
        assert!(matches!(
            parse_agent_event(r#"{"type":"response","success":true}"#),
            AgentEvent::Other { .. }
        ));
    }

    #[test]
    fn disposition_forward_compat() {
        assert_eq!(
            parse_agent_event(
                r#"{"type":"response","command":"prompt","success":true,"data":{"disposition":"teleported"}}"#
            ),
            AgentEvent::Response {
                id: None,
                command: "prompt".to_owned(),
                success: true,
                error: None,
                disposition: Some(Disposition::Other("teleported".to_owned())),
            }
        );
    }

    // --- Restart backoff ---------------------------------------------------

    /// Golden: exponential doubling schedule, capped, exhaustion = None
    /// (ADR-0015 cl. 4).
    #[test]
    fn restart_backoff_schedule_golden() {
        // initial=1000ms, cap=60_000ms, max_restarts=5.
        let schedule: Vec<Option<u64>> = (0..=6)
            .map(|n| restart_backoff_ms(1_000, 60_000, 5, n))
            .collect();
        assert_eq!(
            schedule,
            vec![
                Some(1_000),
                Some(2_000),
                Some(4_000),
                Some(8_000),
                Some(16_000),
                None, // max_restarts=5 exhausted (counts 0..4 were restarts 1..5)
                None,
            ]
        );
        // Cap clamps: initial=1000, cap=3000 → 1000, 2000, 3000, 3000, …
        assert_eq!(restart_backoff_ms(1_000, 3_000, 100, 2), Some(3_000));
        assert_eq!(restart_backoff_ms(1_000, 3_000, 100, 3), Some(3_000));
        // max_restarts=0 disables auto-restart entirely.
        assert_eq!(restart_backoff_ms(1_000, 60_000, 0, 0), None);
        // Overflow saturates instead of wrapping.
        assert_eq!(
            restart_backoff_ms(u64::MAX, u64::MAX, 10, 9),
            Some(u64::MAX)
        );
    }

    // --- SessionRecord (charter v2 entry) ----------------------------------

    #[test]
    fn session_record_serde_golden() {
        let record = SessionRecord {
            id: sid(1001, 2),
            status: SessionStatus::Idle,
            restart_count: 1,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(
            json,
            r#"{"id":{"uid":1001,"incarnation":2},"status":"idle","restart_count":1}"#
        );
        assert_eq!(
            serde_json::from_str::<SessionRecord>(&json).unwrap(),
            record
        );
        // deny_unknown_fields: corruption, not tolerance.
        assert!(serde_json::from_str::<SessionRecord>(
            r#"{"id":{"uid":1001,"incarnation":2},"status":"idle","restart_count":1,"extra":0}"#
        )
        .is_err());
    }
}
