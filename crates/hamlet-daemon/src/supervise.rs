//! Agent session supervision (SPEC §6; protocol binding ADR-0015 — cited
//! as "ADR-0008" by the Sprint 03 prompts).
//!
//! Owns the pi session process lifecycle over `podman exec` stdio: spawn,
//! health/state tracking, restart policy, graceful stop, hard kill. The
//! daemon — never the agent — is the source of session truth
//! (SPEC §6 telemetry rule): statuses derive from the parsed JSON-RPC
//! stream and process exits, are validated through the closed transition
//! table ([`SessionStatus::transition`]) and persisted to the charter with
//! atomic saves (CONVENTIONS §3 r2). Crash telemetry goes to sled
//! ([`Telemetry::trace_event`]); permanent-crash lineage goes to TITLE.md.
//!
//! # Architecture
//!
//! One tokio task per session life *chain* (all incarnations of one UID
//! slot). Inside a life, the pipes are split three ways so reads, writes,
//! kill, and wait never contend on one borrow:
//!
//! - **stdout** → line reader → [`parse_agent_event`] (never fatal:
//!   `Other`/`Malformed` degrade, ADR-0015) → status reducer → charter;
//! - **stdin** → owned by the life task alone (single-writer invariant,
//!   SPEC §6): queued deliveries AND priority interrupts funnel through
//!   the same select loop, so the records of one envelope are always
//!   written contiguously and an interrupt landing mid-drain can never
//!   corrupt framing;
//! - **the child** → a reaper task that resolves `wait` or performs a
//!   requested kill.
//!
//! # Delivery (SPEC §6; Sprint 03 step 03)
//!
//! Per-session FIFO queues live in [`crate::delivery::QueueStore`]
//! (file-as-truth: `courier/queues/<uid>.jsonl`, survives daemon restart).
//! [`Supervisor::enqueue`] accepts mail/tasking/notice envelopes (bounded
//! by `sessions.queue_depth`; overflow answers the typed
//! [`QueueError::Overflow`] — Sprint 04's bounce hook) and pokes the life
//! task; the task pops **one** envelope per task boundary (status
//! `running`/`idle`, no prompt in flight) and writes its `pi_binding`
//! records. [`Supervisor::steer`] bypasses the queue entirely: the
//! `interrupt_and_steer` envelope travels over the same control channel
//! and is injected down the live pipe immediately — concurrent with an
//! in-flight task — with the correlation id tying it to the resulting
//! stream events (wire ids `<corr>:{q,s,a,p}`).
//!
//! Restart policy (ADR-0015 cl. 4): unexpected exit or readiness timeout →
//! charter `crashed` + sled trace; budget check via the pure schedule
//! [`RestartConfig::backoff_ms`] over the persisted `restart_count` (no
//! wall-clock state, CONVENTIONS §3 r4); a scheduled restart bumps the
//! incarnation ([`SessionId::next_incarnation`]) and — with
//! `continue_on_restart` — adds pi `--continue`. `restart_count` resets to
//! 0 on the first task boundary (`agent_settled`) of a life. Budget
//! exhaustion → permanently `crashed`, TITLE.md lineage stamp, sled trace.
//!
//! # Secrets (SPEC §11.4, CONVENTIONS §6)
//!
//! [`session_env`] maps `.env` [`Secrets`] into the exec's `-e` env —
//! out-of-band, never files in the parcel home. Every log/error path masks
//! values ([`crate::podman::redacted_argv`]).
//!
//! # Test seam (CONVENTIONS §9)
//!
//! [`SessionSpawner`] is the seam: [`PodmanSpawner`] delegates to the
//! [`Runtime`] (real podman or `FakeRuntime`); [`HostProcessSpawner`] runs
//! the FakeAgent binary (`src/bin/fakeagent.rs`, ADR-0015 cl. 5) as a host
//! process so every supervision behavior is testable without podman.
//!
//! # Telemetry (SPEC §6, §11.2; Sprint 03 step 04)
//!
//! Every stdout frame is folded by [`crate::metrics`] *before* any
//! supervision branch runs, so token tallies, API cost, latency spikes and
//! stream traces are daemon-parsed evidence — never agent self-report. The
//! fold is pure and inline (it cannot block a drain or an interrupt);
//! every sled write belongs to the per-session sink task, whose channel is
//! fed non-blockingly. [`Supervisor::metrics_for_session`],
//! [`Supervisor::metrics_for_uid`] and
//! [`Supervisor::metrics_for_commission`] are the live query surface the
//! CLI (Sprint 08) and WS streams (Sprint 09) read.
//!
//! # Trace vocabulary (sled `traces.v1`)
//!
//! Lifecycle (this module): `session-started`, `session-spawned`,
//! `session-ready`, `session-enqueued`, `session-queue-overflow`,
//! `session-delivered`, `session-delivery-failed`,
//! `session-delivery-rejected`, `session-interrupt-injected`,
//! `session-interrupt-refused`, `session-crashed`,
//! `session-readiness-timeout`, `session-readiness-refused`,
//! `session-spawn-failed`, `session-restart-scheduled`,
//! `session-permanently-crashed`, `session-stop-requested`,
//! `session-grace-expired`, `session-stopped`, `session-killed`,
//! `session-stop-during-backoff`, `session-orphaned-recovered`,
//! `session-title-stamp-failed`, `session-stream-tail-truncated`,
//! `session-telemetry-failed`.
//!
//! Stream-derived ([`crate::metrics`], ADR-0017 cl. 4 — one source per event,
//! so no duplicates): `session-delivery-acked`, `session-interrupt-acked`,
//! `session-tool-call`, `session-agent-retry`, `session-stream-event`,
//! `session-stream-malformed`, `session-stream-malformed-suppressed`,
//! `session-stream-unknown-record`, `session-stream-summary`.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{SystemTime, UNIX_EPOCH};

use hamlet_core::charter::{Charter, CharterError};
use hamlet_core::config::{Config, Secrets, SessionsConfig};
use hamlet_core::sanitize::SafeName;
use hamlet_core::session::{
    parse_agent_event, pi_binding, readiness_probe, AgentEvent, CorrelationId, InterruptPriority,
    InterruptSource, QueuedKind, SessionEnvelope, SessionError, SessionId, SessionRecord,
    SessionStatus,
};
use hamlet_core::tree::HamletTree;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    Lines,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Duration};

use crate::delivery::{QueueError, QueueStore};
use crate::metrics;
use crate::podman::{
    spawn_child_exec, BoxFuture, ExecHandle, PodmanError, Runtime, SessionExecSpec,
    SIGNAL_EXIT_CODE,
};
use crate::telemetry::{
    AgentKey, MetricsScope, MetricsSnapshot, SessionKey, Telemetry, TelemetryError, TraceRecord,
};

/// Upper bound of the stderr tail kept for crash diagnostics.
const STDERR_TAIL_CAP: usize = 2048;
/// Upper bound of a crash detail rendered into traces / TITLE.md.
const DETAIL_CAP: usize = 160;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed supervision failures.
#[derive(Debug)]
pub enum SuperviseError {
    /// Session-protocol failure (identity, transition table, binding).
    Session(SessionError),
    /// Charter load/save/validation failure.
    Charter(CharterError),
    /// Podman/spawner failure.
    Podman(PodmanError),
    /// Telemetry store failure on a *required* write (correlation seqs).
    Telemetry(TelemetryError),
    /// Delivery-queue failure: overflow (the bounce hook), persistence,
    /// or corruption ([`crate::delivery`]).
    Queue(QueueError),
    /// Filesystem failure with context.
    Io {
        /// What was attempted.
        context: String,
        /// Underlying error.
        source: io::Error,
    },
    /// The UID slot does not exist in the charter.
    NoSlot(u32),
    /// A supervision task is already running for this UID.
    AlreadyActive(u32),
    /// No live supervision task for this UID.
    NotLive(u32),
    /// The session is draining toward stop (stdin closing/closed) — it
    /// accepts neither deliveries nor interrupts.
    Draining(u32),
    /// The charter session record vanished mid-supervision.
    NoRecord(u32),
    /// A supervision task panicked or a session-chain invariant broke.
    TaskFailed(String),
}

impl fmt::Display for SuperviseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(e) => write!(f, "session: {e}"),
            Self::Charter(e) => write!(f, "charter: {e}"),
            Self::Podman(e) => write!(f, "podman: {e}"),
            Self::Telemetry(e) => write!(f, "telemetry: {e}"),
            Self::Queue(e) => write!(f, "delivery queue: {e}"),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::NoSlot(uid) => write!(f, "no charter slot for uid {uid}"),
            Self::AlreadyActive(uid) => write!(f, "session for uid {uid} is already supervised"),
            Self::NotLive(uid) => write!(f, "no live session for uid {uid}"),
            Self::Draining(uid) => write!(f, "session for uid {uid} is draining toward stop"),
            Self::NoRecord(uid) => write!(f, "no charter session record for uid {uid}"),
            Self::TaskFailed(msg) => write!(f, "supervision task failed: {msg}"),
        }
    }
}

impl std::error::Error for SuperviseError {}

impl From<SessionError> for SuperviseError {
    fn from(e: SessionError) -> Self {
        Self::Session(e)
    }
}

impl From<CharterError> for SuperviseError {
    fn from(e: CharterError) -> Self {
        Self::Charter(e)
    }
}

impl From<PodmanError> for SuperviseError {
    fn from(e: PodmanError) -> Self {
        Self::Podman(e)
    }
}

impl From<TelemetryError> for SuperviseError {
    fn from(e: TelemetryError) -> Self {
        Self::Telemetry(e)
    }
}

impl From<QueueError> for SuperviseError {
    fn from(e: QueueError) -> Self {
        Self::Queue(e)
    }
}

// ---------------------------------------------------------------------------
// Crash hook (SPEC §6, §7; ADR-0029 cl. 2)
// ---------------------------------------------------------------------------

/// What a supervision task reports when a session life dies: the dead
/// [`SessionId`], how much of the restart budget it had already spent, and
/// whether the daemon is giving up on it.
///
/// This is **telemetry-shaped notification data**, never state (CONVENTIONS
/// §2): the charter already holds the terminal record, and the hook only
/// decides whether the Mayor should be woken (`mayor.crash_wake`,
/// ADR-0029 cl. 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashNotice {
    /// Charter slot whose session died.
    pub uid: u32,
    /// The dead life.
    pub session: SessionId,
    /// Consecutive auto-restarts already spent before this crash.
    pub restarts: u32,
    /// True when the restart budget is exhausted — the daemon will not
    /// revive the agent and the parcel carries its permanent `TITLE.md`
    /// stamp.
    pub permanent: bool,
    /// One-line, scrubbed reason (never stderr verbatim, never secrets —
    /// `scrub_detail`).
    pub detail: String,
}

/// The crash hook seam (ADR-0029 cl. 2): the supervisor notifies, the
/// Mayor plane decides. Implementations must not block the supervision
/// task — the production bridge hands the notice to its own task.
pub trait CrashWake: Send + Sync {
    /// One life died. Called on the supervision task, so keep it cheap.
    fn crashed(&self, notice: &CrashNotice);
}

/// Recording hook for tests: the notices a supervision chain emitted.
#[derive(Default)]
pub struct RecordingCrashWake {
    notices: std::sync::Mutex<Vec<CrashNotice>>,
}

impl RecordingCrashWake {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every notice, in order.
    pub fn notices(&self) -> Vec<CrashNotice> {
        self.notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl CrashWake for RecordingCrashWake {
    fn crashed(&self, notice: &CrashNotice) {
        self.notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(notice.clone());
    }
}

// ---------------------------------------------------------------------------
// Spawner seam
// ---------------------------------------------------------------------------

/// The session-spawn seam (CONVENTIONS §9): production spawns
/// `podman exec -i …` through the [`Runtime`]; tests spawn the FakeAgent
/// host binary — same [`SessionExecSpec`], same supervision code.
pub trait SessionSpawner: Send + Sync {
    /// Spawn one session process with piped stdio.
    fn spawn_session<'a>(
        &'a self,
        spec: &'a SessionExecSpec,
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>>;
}

/// Production spawner: delegates to the podman [`Runtime`] seam
/// ([`Runtime::spawn_session`], argv per ADR-0015 cl. 1).
pub struct PodmanSpawner {
    runtime: Arc<dyn Runtime>,
}

impl PodmanSpawner {
    /// Spawner over a runtime (real podman or `FakeRuntime`).
    #[must_use]
    pub fn new(runtime: Arc<dyn Runtime>) -> Self {
        Self { runtime }
    }
}

impl SessionSpawner for PodmanSpawner {
    fn spawn_session<'a>(
        &'a self,
        spec: &'a SessionExecSpec,
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        self.runtime.spawn_session(spec)
    }
}

/// Test-only spawner: runs a host binary (the FakeAgent,
/// `src/bin/fakeagent.rs`) in place of the container exec, so supervision
/// logic is testable without podman. `spec.cmd[0]` (the `pi` token)
/// identifies the in-container program and is replaced by `program`; the
/// remaining tokens pass through verbatim (the fake ignores them). The spec
/// env becomes the child's added environment — which is how a test hands
/// the fake its `FAKEAGENT_SCRIPT`. Container/uid/workdir have no host
/// meaning and are ignored (argv validation still runs in the podman path's
/// golden tests).
pub struct HostProcessSpawner {
    program: PathBuf,
}

impl HostProcessSpawner {
    /// Spawner running `program` (e.g. `env!("CARGO_BIN_EXE_fakeagent")`).
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl SessionSpawner for HostProcessSpawner {
    fn spawn_session<'a>(
        &'a self,
        spec: &'a SessionExecSpec,
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        Box::pin(async move {
            let args: Vec<String> = spec.cmd.iter().skip(1).cloned().collect();
            let mut label = vec![self.program.to_string_lossy().into_owned()];
            label.extend(args.iter().cloned());
            spawn_child_exec(&self.program, &args, &spec.env, label)
        })
    }
}

// ---------------------------------------------------------------------------
// Pure composition helpers (golden-tested)
// ---------------------------------------------------------------------------

/// Compose the session exec env (ADR-0015 cl. 1): the structural parcel
/// env (bootstrap-derived: `HOME`, `PATH`, tool vars) + the `.env` secret
/// mapping + pi hygiene vars. This is the *only* channel secrets take into
/// a parcel — the exec's `-e` env, never files in the home (SPEC §11.4).
///
/// Mapping (HANDOFF step-01 decision, resolved here):
/// - `HAMLET_API_KEY_<PROVIDER>` → `<PROVIDER>_API_KEY` (uppercased suffix —
///   the env names pi's built-in providers read); non-identifier suffixes
///   are skipped (defensive; `.env` parsing already charset-limits keys);
/// - `HAMLET_MODEL_ENDPOINT` / `HAMLET_MODEL_API_KEY` pass through under
///   their hamlet names: pi consumes custom endpoints via a `models.json`
///   in `PI_CODING_AGENT_DIR` whose `apiKey` may *interpolate*
///   `$HAMLET_MODEL_API_KEY` (pi docs `models.md`) — so the secret stays in
///   the exec env and only the (non-secret) endpoint config would ever be
///   rendered to a file. Rendering `models.json` itself needs an api-type +
///   model catalog the `.env` surface does not carry; deferred until a
///   provisioning step owns it (recorded in HANDOFF).
/// - fixed: `PI_TELEMETRY=0`, `PI_SKIP_VERSION_CHECK=1`.
#[must_use]
pub fn session_env(
    structural: &BTreeMap<String, String>,
    secrets: &Secrets,
) -> BTreeMap<String, String> {
    let mut env = structural.clone();
    for (provider, secret) in &secrets.provider_keys {
        let name = format!("{}_API_KEY", provider.to_ascii_uppercase());
        if is_env_identifier(&name) {
            env.insert(name, secret.expose().to_owned());
        }
    }
    if let Some(endpoint) = &secrets.model_endpoint {
        env.insert(
            "HAMLET_MODEL_ENDPOINT".to_owned(),
            endpoint.expose().to_owned(),
        );
    }
    if let Some(key) = &secrets.model_api_key {
        env.insert("HAMLET_MODEL_API_KEY".to_owned(), key.expose().to_owned());
    }
    env.insert("PI_TELEMETRY".to_owned(), "0".to_owned());
    env.insert("PI_SKIP_VERSION_CHECK".to_owned(), "1".to_owned());
    env
}

fn is_env_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The pi command tokens (ADR-0015 cl. 1): `pi --mode rpc [--provider P]
/// [--model M]`. Provider/model are charset-validated by
/// [`SessionsConfig::validate`] (single argv tokens, no leading `-`).
#[must_use]
pub fn pi_session_cmd(sessions: &SessionsConfig) -> Vec<String> {
    let mut cmd = vec!["pi".to_owned(), "--mode".to_owned(), "rpc".to_owned()];
    if let Some(provider) = &sessions.provider {
        cmd.push("--provider".to_owned());
        cmd.push(provider.clone());
    }
    if let Some(model) = &sessions.model {
        cmd.push("--model".to_owned());
        cmd.push(model.clone());
    }
    cmd
}

/// The command for one incarnation: restarted lives (`incarnation > 0`)
/// append `--continue` when `continue_on_restart` is set, so pi resumes its
/// own session history from the parcel home (ADR-0015 cl. 4).
#[must_use]
pub fn cmd_for_incarnation(
    base: &[String],
    sessions: &SessionsConfig,
    incarnation: u32,
) -> Vec<String> {
    let mut cmd = base.to_vec();
    if incarnation > 0 && sessions.continue_on_restart {
        cmd.push("--continue".to_owned());
    }
    cmd
}

/// TITLE.md lineage line for a permanently crashed session (SPEC §6:
/// "crash lineage goes to TITLE.md"). The timestamp is lineage data only,
/// never a state input (CONVENTIONS §3 r4). `detail` is scrubbed to
/// printable ASCII and truncated.
#[must_use]
pub fn title_crash_line(session: &SessionId, restarts: u32, unix_ts: u64, detail: &str) -> String {
    format!(
        "- unix {unix_ts} · session {} permanently crashed after {restarts} auto-restart(s) · {}\n",
        session.token(),
        scrub_detail(detail)
    )
}

/// Scrub untrusted text (stderr tails, error messages) for lineage/trace
/// rendering: printable ASCII only, whitespace collapsed, ≤ [`DETAIL_CAP`]
/// chars. Values are already masked upstream ([`crate::podman::redacted_argv`]);
/// this is the second line of defense (CONVENTIONS §6).
#[must_use]
pub fn scrub_detail(raw: &str) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for c in raw.chars().take(DETAIL_CAP * 4) {
        if count >= DETAIL_CAP {
            out.push('…');
            break;
        }
        if c.is_ascii_graphic() {
            out.push(c);
            count += 1;
        } else if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
                count += 1;
            }
        } else {
            out.push('.');
            count += 1;
        }
    }
    out.trim().to_owned()
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().try_into().unwrap_or(u64::MAX))
}

// ---------------------------------------------------------------------------
// Parcel reference
// ---------------------------------------------------------------------------

/// Everything the supervisor needs to address one parcel's session target.
/// Derived from the charter via [`Supervisor::parcel_ref`] (production) or
/// assembled by tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParcelRef {
    /// Charter UID slot (the exec user, ADR-0011).
    pub uid: u32,
    /// Container name (sanitized; provisioning uses the slot identity).
    pub container: SafeName,
    /// Commission for telemetry scoping ([`AgentKey`]); 0 when the slot
    /// carries none.
    pub commission: u64,
    /// Structural (non-secret) parcel env from bootstrap — merged under the
    /// secret mapping by [`session_env`]. Tests put `FAKEAGENT_SCRIPT` here.
    pub env: BTreeMap<String, String>,
}

impl ParcelRef {
    /// A reference with empty structural env.
    #[must_use]
    pub fn new(uid: u32, container: SafeName, commission: u64) -> Self {
        Self {
            uid,
            container,
            commission,
            env: BTreeMap::new(),
        }
    }

    /// Attach the structural parcel env.
    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Stop/kill/delivery/interrupt requests from the API into a session's
/// supervision task. This channel is the single-writer funnel: every byte
/// that reaches the session's stdin (after the readiness probe) is written
/// by the life task in response to a `Control` message or a stream event,
/// so queued drains and priority interrupts can never interleave records
/// (SPEC §6 race safety).
#[derive(Debug)]
enum Control {
    GracefulStop,
    HardKill,
    /// Wake-up: the persistent queue may hold undelivered envelopes —
    /// attempt a boundary drain (the queue itself lives in the
    /// file-as-truth [`QueueStore`], not in this message).
    Deliver,
    /// Divine Interruption: inject immediately down the live pipe,
    /// bypassing the queue; the reply carries the correlation id (or the
    /// typed refusal).
    Interrupt {
        envelope: SessionEnvelope,
        reply: oneshot::Sender<Result<CorrelationId, SuperviseError>>,
    },
}

struct LiveEntry {
    generation: u64,
    control: mpsc::UnboundedSender<Control>,
    done: Option<JoinHandle<()>>,
}

struct Inner {
    tree: HamletTree,
    config: Config,
    spawner: Arc<dyn SessionSpawner>,
    telemetry: Arc<Telemetry>,
    /// Persistent per-session delivery queues (file-as-truth under
    /// `courier/queues/`; crate::delivery).
    queues: QueueStore,
    /// Serializes charter load→mutate→save across sessions (the daemon is
    /// the singular governor; atomic rename protects against crashes, this
    /// against concurrent in-process writers).
    charter_lock: Mutex<()>,
    /// Per-life kill senders (`SessionId::as_u64` → sender) so
    /// [`Supervisor::abandon`] can SIGKILL children without charter writes.
    killers: Mutex<BTreeMap<u64, mpsc::UnboundedSender<()>>>,
    /// Optional crash hook (ADR-0029 cl. 2). Installed after construction so
    /// the supervision plane and the Mayor plane can be wired in either
    /// order; absence is not an error (a village without a Mayor still
    /// supervises).
    crash_wake: Mutex<Option<Arc<dyn CrashWake>>>,
    generation: AtomicU64,
}

impl Inner {
    fn update_charter<T>(
        &self,
        f: impl FnOnce(&mut Charter) -> Result<T, SuperviseError>,
    ) -> Result<T, SuperviseError> {
        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.tree.charter();
        let mut charter = Charter::load(&path)?;
        let out = f(&mut charter)?;
        charter.validate()?;
        charter.save_atomic(&path)?;
        Ok(out)
    }

    /// Validate + persist one session record. The closed transition table
    /// and incarnation monotonicity are enforced here — a supervision bug
    /// becomes a typed error, never a corrupt charter.
    fn set_session(&self, record: SessionRecord) -> Result<(), SuperviseError> {
        let uid = record.id.uid;
        self.update_charter(|charter| {
            if let Some(prev) = charter.sessions.get(&uid) {
                prev.status.transition(record.status)?;
                if record.id.incarnation < prev.id.incarnation {
                    return Err(SuperviseError::TaskFailed(format!(
                        "incarnation regression for uid {uid}: {} < {}",
                        record.id.incarnation, prev.id.incarnation
                    )));
                }
            }
            charter.sessions.insert(uid, record);
            Ok(())
        })
    }

    fn session_record(&self, uid: u32) -> Result<Option<SessionRecord>, SuperviseError> {
        let _guard = self.charter_lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(Charter::load(&self.tree.charter())?
            .sessions
            .get(&uid)
            .copied())
    }

    /// Best-effort sled trace (ADR-0005 cl. 6: telemetry loss degrades,
    /// never corrupts state — supervision must not die on a sled failure).
    fn trace(&self, agent: AgentKey, event: &str, detail: impl Into<String>) {
        let record = TraceRecord {
            at_unix_ms: unix_now_ms(),
            event: event.to_owned(),
            detail: detail.into(),
        };
        if let Err(e) = self.telemetry.trace_event(agent, record) {
            eprintln!("hamlet-daemon: session trace {event} failed: {e}");
        }
    }

    fn register_killer(&self, session: SessionId, sender: mpsc::UnboundedSender<()>) {
        self.killers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.as_u64(), sender);
    }

    fn unregister_killer(&self, session: SessionId) {
        self.killers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session.as_u64());
    }

    /// Hand one crash notice to the installed hook (ADR-0029 cl. 2). The
    /// charter record is already written by the caller — this is a wake
    /// signal, never a state transition, so a missing or panicking-free
    /// hook changes nothing about what the village believes.
    fn notify_crash(&self, notice: CrashNotice) {
        let hook = {
            let guard = self.crash_wake.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        if let Some(hook) = hook {
            hook.crashed(&notice);
        }
    }
}

/// The session supervisor (SPEC §6): one instance per village tree, held
/// by the daemon (the singular governor — CONVENTIONS §2); all APIs take
/// `&self` and assume one daemon caller per tree.
pub struct Supervisor {
    inner: Arc<Inner>,
    live: Arc<Mutex<BTreeMap<u32, LiveEntry>>>,
}

impl Supervisor {
    /// Supervisor over a village tree, engine config (restart policy,
    /// timings, pi model selection), the spawn seam and the sled sink.
    #[must_use]
    pub fn new(
        tree: HamletTree,
        config: Config,
        spawner: Arc<dyn SessionSpawner>,
        telemetry: Arc<Telemetry>,
    ) -> Self {
        let queues = QueueStore::new(tree.clone());
        Self {
            inner: Arc::new(Inner {
                tree,
                config,
                spawner,
                telemetry,
                queues,
                charter_lock: Mutex::new(()),
                killers: Mutex::new(BTreeMap::new()),
                crash_wake: Mutex::new(None),
                generation: AtomicU64::new(0),
            }),
            live: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The village tree governed.
    #[must_use]
    pub fn tree(&self) -> &HamletTree {
        &self.inner.tree
    }

    /// The engine config in force.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Derive a [`ParcelRef`] from the charter (slot must exist; container
    /// name = slot identity, as provisioned; commission from the slot, 0
    /// when unbound). Structural env stays empty — production callers merge
    /// the bootstrap-derived parcel env via [`ParcelRef::with_env`].
    ///
    /// # Errors
    ///
    /// [`SuperviseError::NoSlot`] / charter failures.
    pub fn parcel_ref(&self, uid: u32) -> Result<ParcelRef, SuperviseError> {
        let _guard = self
            .inner
            .charter_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let charter = Charter::load(&self.inner.tree.charter())?;
        let slot = charter.slots.get(&uid).ok_or(SuperviseError::NoSlot(uid))?;
        Ok(ParcelRef::new(
            uid,
            slot.identity.clone(),
            slot.commission.unwrap_or(0),
        ))
    }

    /// The charter's current record for a UID slot (file-as-truth read).
    ///
    /// # Errors
    ///
    /// Charter load failures.
    pub fn session_record(&self, uid: u32) -> Result<Option<SessionRecord>, SuperviseError> {
        self.inner.session_record(uid)
    }

    /// The persistent delivery-queue store (file-as-truth under
    /// `courier/queues/`) — introspection for tests, the CLI, and
    /// Sprint 04's mail router / bounce rendering.
    #[must_use]
    pub fn queues(&self) -> &QueueStore {
        &self.inner.queues
    }

    /// Install the crash hook (ADR-0029 cl. 2). Wiring order is free: the
    /// Mayor plane may attach before or after sessions start. Replacing an
    /// installed hook is allowed (one writer at boot); a supervisor with no
    /// hook supervises exactly as before.
    pub fn set_crash_wake(&self, hook: Arc<dyn CrashWake>) {
        *self
            .inner
            .crash_wake
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    // --- live metrics query surface (SPEC §11.2; Sprint 03 step 04) --------

    /// Live telemetry snapshot for one session life (`window` bounds the
    /// recent spike/trace lists). Read-only over sled: never orchestration
    /// state, never a charter read (CONVENTIONS §2).
    ///
    /// # Errors
    ///
    /// Telemetry storage/corruption failures.
    pub fn metrics_for_session(
        &self,
        session: SessionId,
        commission: u64,
        window: usize,
    ) -> Result<MetricsSnapshot, SuperviseError> {
        let key = SessionKey::new(commission, session.uid, session.as_u64());
        Ok(self
            .inner
            .telemetry
            .metrics_for(MetricsScope::Session(key), window)?)
    }

    /// Live telemetry snapshot for one UID slot — every session of the
    /// agent's current commission (resolved from the charter slot, the only
    /// source of the uid → commission binding).
    ///
    /// # Errors
    ///
    /// [`SuperviseError::NoSlot`], charter or telemetry failures.
    pub fn metrics_for_uid(
        &self,
        uid: u32,
        window: usize,
    ) -> Result<MetricsSnapshot, SuperviseError> {
        let (_, agent) = self.session_identity(uid)?;
        Ok(self
            .inner
            .telemetry
            .metrics_for(MetricsScope::Agent(agent), window)?)
    }

    /// Live telemetry snapshot for a whole commission (every agent, every
    /// session) — the rollup Sprint 06 exports to `keep/archives/` and
    /// Sprint 09 streams to the canvas.
    ///
    /// # Errors
    ///
    /// Telemetry storage/corruption failures.
    pub fn metrics_for_commission(
        &self,
        commission: u64,
        window: usize,
    ) -> Result<MetricsSnapshot, SuperviseError> {
        Ok(self
            .inner
            .telemetry
            .metrics_for(MetricsScope::Commission(commission), window)?)
    }

    /// The telemetry sink this supervisor writes through (Sprint 01 API).
    #[must_use]
    pub fn telemetry(&self) -> &Arc<Telemetry> {
        &self.inner.telemetry
    }

    /// The identity a new envelope for `uid` must carry: the current
    /// charter session id (or the slot's first incarnation when the
    /// session never ran), plus the telemetry key for correlation seqs.
    fn session_identity(&self, uid: u32) -> Result<(SessionId, AgentKey), SuperviseError> {
        let _guard = self
            .inner
            .charter_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let charter = Charter::load(&self.inner.tree.charter())?;
        let slot = charter.slots.get(&uid).ok_or(SuperviseError::NoSlot(uid))?;
        let session = match charter.sessions.get(&uid) {
            Some(record) => record.id,
            None => SessionId::first(uid)?,
        };
        Ok((
            session,
            AgentKey {
                commission: slot.commission.unwrap_or(0),
                uid,
            },
        ))
    }

    fn queue_bound(&self) -> usize {
        usize::try_from(self.inner.config.sessions.queue_depth).unwrap_or(usize::MAX)
    }

    /// Enqueue one message for sequential stdin delivery (SPEC §6):
    /// mail items, Mayor tasking, system notices — one bounded FIFO per
    /// session, persisted to `courier/queues/<uid>.jsonl` before this
    /// call returns (undelivered items survive daemon restart). Works
    /// while the session is live (delivered at the next task boundary)
    /// or offline (delivered after the next `start_session`). Returns the
    /// allocated correlation id.
    ///
    /// Backpressure: at `sessions.queue_depth` undelivered envelopes the
    /// call fails with [`QueueError::Overflow`] — the typed bounce hook
    /// Sprint 04's mail plumbing routes back to the sender.
    ///
    /// # Errors
    ///
    /// [`SuperviseError::NoSlot`], empty text, telemetry (correlation
    /// seq), or queue failures.
    pub fn enqueue(
        &self,
        uid: u32,
        kind: QueuedKind,
        text: &str,
    ) -> Result<CorrelationId, SuperviseError> {
        let (session, key) = self.session_identity(uid)?;
        let seq = self.inner.telemetry.next_correlation_seq(key)?;
        let envelope = match kind {
            QueuedKind::Mail => SessionEnvelope::mail(&session, seq, text)?,
            QueuedKind::Tasking => SessionEnvelope::tasking(&session, seq, text)?,
            QueuedKind::Notice => SessionEnvelope::notice(&session, seq, text)?,
        };
        let correlation = envelope.correlation().clone();
        let depth = match self.inner.queues.enqueue(uid, envelope, self.queue_bound()) {
            Ok(depth) => depth,
            Err(err @ QueueError::Overflow { depth, .. }) => {
                self.inner.trace(
                    key,
                    "session-queue-overflow",
                    format!("{kind} {correlation} refused at depth {depth} — bounce to sender"),
                );
                return Err(err.into());
            }
            Err(err) => return Err(err.into()),
        };
        self.inner.trace(
            key,
            "session-enqueued",
            format!("{kind} {correlation} depth={depth}"),
        );
        // Wake the life task when one is live (best effort — between lives
        // the poke is lost harmlessly: every readiness/boundary re-drains).
        if let Some(entry) = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&uid)
        {
            let _ = entry.control.send(Control::Deliver);
        }
        Ok(correlation)
    }

    /// **Divine Interruption** (SPEC §6): bypass the delivery queue and
    /// inject an `interrupt_and_steer` envelope immediately down the live
    /// pipe — concurrent with an in-flight task. The binding depends on
    /// the parsed stream status (ADR-0015 cl. 3): busy →
    /// `[clear_queue?] + steer + abort`; running/idle → one header-marked
    /// prompt; starting/draining/dead → typed refusal. The returned
    /// correlation id ties the interrupt to the resulting stream events
    /// (wire ids `<corr>:{q,s,a,p}`).
    ///
    /// Sources: `system` (daemon events), `human` (operator — CLI
    /// `hamlet agent steer` / HTTP surface are Sprint 08 polish over this
    /// daemon API), `mayor` (Sprint 05 decrees).
    ///
    /// # Errors
    ///
    /// [`SuperviseError::NotLive`] (no supervision task),
    /// [`SuperviseError::Draining`] (stop in progress),
    /// [`SuperviseError::Session`] (not ready / not alive, empty text),
    /// telemetry or write failures.
    pub async fn steer(
        &self,
        uid: u32,
        source: InterruptSource,
        priority: InterruptPriority,
        steering_text: &str,
    ) -> Result<CorrelationId, SuperviseError> {
        let control = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&uid)
            .map(|entry| entry.control.clone())
            .ok_or(SuperviseError::NotLive(uid))?;
        let (session, key) = self.session_identity(uid)?;
        let seq = self.inner.telemetry.next_correlation_seq(key)?;
        let envelope = SessionEnvelope::interrupt(&session, seq, source, priority, steering_text)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        control
            .send(Control::Interrupt {
                envelope,
                reply: reply_tx,
            })
            .map_err(|_| SuperviseError::NotLive(uid))?;
        reply_rx.await.map_err(|_| SuperviseError::NotLive(uid))?
    }

    /// Is a supervision task running for this UID?
    #[must_use]
    pub fn is_live(&self, uid: u32) -> bool {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.get(&uid).is_some_and(|entry| {
            entry
                .done
                .as_ref()
                .is_none_or(|handle| !handle.is_finished())
        })
    }

    /// Start (or operator-restart) the pi session for a parcel
    /// (ADR-0015 cl. 1 argv; secrets via exec `-e` only). Returns the new
    /// life's [`SessionId`]; the charter shows `starting` immediately, and
    /// readiness (`running`) follows asynchronously once the
    /// `get_state` probe is answered.
    ///
    /// `brief` — the agent's register brief — is enqueued as the first
    /// tasking envelope on the persistent delivery queue and written at
    /// the first task boundary (readiness). If the session crashes before
    /// the write, the brief stays queued and is delivered after the
    /// restart (file-as-truth); once written it is never replayed
    /// (at-most-once, `crate::delivery`). Empty/whitespace briefs are
    /// skipped.
    ///
    /// A stale *alive* charter record without a live task (daemon restart)
    /// is healed to `crashed` first (CONVENTIONS §3 r6); an operator start
    /// after crashed/stopped bumps the incarnation and **resets the restart
    /// budget** (fresh operator intent).
    ///
    /// # Errors
    ///
    /// [`SuperviseError::AlreadyActive`], [`SuperviseError::NoSlot`],
    /// spawn/charter/session failures.
    pub async fn start_session(
        &self,
        parcel: &ParcelRef,
        brief: &str,
        secrets: &Secrets,
    ) -> Result<SessionId, SuperviseError> {
        let cmd = pi_session_cmd(&self.inner.config.sessions);
        self.start_session_with_cmd(parcel, brief, secrets, cmd)
            .await
    }

    /// [`Supervisor::start_session`] with an explicit in-container command
    /// (the guarded live smoke test uses a non-pi echo session when the
    /// parcel image has no pi; production always takes the
    /// [`pi_session_cmd`] path).
    ///
    /// # Errors
    ///
    /// As [`Supervisor::start_session`].
    pub async fn start_session_with_cmd(
        &self,
        parcel: &ParcelRef,
        brief: &str,
        secrets: &Secrets,
        cmd: Vec<String>,
    ) -> Result<SessionId, SuperviseError> {
        let uid = parcel.uid;
        // 1. Reap finished entries; refuse when a task is live.
        {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = live.get(&uid) {
                let finished = entry
                    .done
                    .as_ref()
                    .is_none_or(|handle| handle.is_finished());
                if !finished {
                    return Err(SuperviseError::AlreadyActive(uid));
                }
            }
            live.remove(&uid);
        }

        // 2. Charter: slot must exist; compute the new life's identity;
        //    heal stale alive records; persist `starting`.
        let mut healed = false;
        let session = self.inner.update_charter(|charter| {
            if !charter.slots.contains_key(&uid) {
                return Err(SuperviseError::NoSlot(uid));
            }
            let next = match charter.sessions.get(&uid) {
                None => SessionId::first(uid)?,
                Some(record) => {
                    if record.status.is_alive() {
                        healed = true;
                        let status = record.status.transition(SessionStatus::Crashed)?;
                        charter
                            .sessions
                            .insert(uid, SessionRecord { status, ..*record });
                    }
                    let record = charter
                        .sessions
                        .get(&uid)
                        .copied()
                        .ok_or(SuperviseError::NoRecord(uid))?;
                    record.id.next_incarnation()?
                }
            };
            if let Some(prev) = charter.sessions.get(&uid) {
                prev.status.transition(SessionStatus::Starting)?;
            }
            charter.sessions.insert(
                uid,
                SessionRecord {
                    id: next,
                    status: SessionStatus::Starting,
                    // Operator start = fresh restart budget (recorded decision).
                    restart_count: 0,
                },
            );
            Ok(next)
        })?;
        if healed {
            self.inner.trace(
                self.agent_key(parcel),
                "session-orphaned-recovered",
                format!("uid {uid} healed to crashed before operator start"),
            );
        }

        // 2b. The brief becomes the first queued tasking envelope — the
        //     persistent delivery queue supersedes the direct brief path
        //     (Sprint 03 step 03). Enqueued before spawn so the first
        //     readiness drain sees it.
        if !brief.trim().is_empty() {
            self.enqueue(uid, QueuedKind::Tasking, brief)?;
        }

        // 3. Compose the exec (secrets into `-e` env only) and spawn the
        //    first life synchronously so typed spawn errors reach the caller.
        let env = session_env(&parcel.env, secrets);
        let spec = SessionExecSpec {
            container: parcel.container.to_string(),
            uid,
            env,
            cmd,
        };
        let handle = self.inner.spawner.spawn_session(&spec).await?;

        // 4. Hand the life to its supervision task.
        let ctx = Arc::new(TaskCtx {
            inner: Arc::clone(&self.inner),
            uid,
            container: spec.container.clone(),
            commission: parcel.commission,
            base_env: spec.env,
            base_cmd: spec.cmd,
        });
        let generation = self.inner.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(supervise_lives(
            Arc::clone(&ctx),
            Arc::clone(&self.live),
            generation,
            session,
            handle,
            control_rx,
        ));
        self.live.lock().unwrap_or_else(|e| e.into_inner()).insert(
            uid,
            LiveEntry {
                generation,
                control: control_tx,
                done: Some(task),
            },
        );
        self.inner.trace(
            self.agent_key(parcel),
            "session-started",
            format!(
                "container={} session={} (spawned over the exec seam)",
                spec.container, session
            ),
        );
        Ok(session)
    }

    fn agent_key(&self, parcel: &ParcelRef) -> AgentKey {
        AgentKey {
            commission: parcel.commission,
            uid: parcel.uid,
        }
    }

    /// Graceful stop (consumed by Sprint 07 hibernation): stop accepting
    /// work, close the session's stdin (pi's orderly-shutdown signal), wait
    /// up to `sessions.stop_grace_ms` for exit, hard-kill on expiry.
    /// Undelivered queue entries are NOT drained on stop — they stay
    /// persisted (`courier/queues/`, file-as-truth) for the next session.
    /// Returns the final charter status — normally `stopped`, or
    /// `crashed` when the request landed inside a restart backoff window
    /// (crashed→stopped is not a legal transition; the session stays
    /// crashed, restart cancelled).
    ///
    /// # Errors
    ///
    /// [`SuperviseError::NotLive`], charter failures, task panic.
    pub async fn stop_session(&self, uid: u32) -> Result<SessionStatus, SuperviseError> {
        self.request_stop(uid, Control::GracefulStop).await
    }

    /// Hard kill (consumed by Sprint 07 hibernation): SIGKILL semantics
    /// immediately, no drain, no grace. Charter → `stopped` (a
    /// daemon-intended end, not a crash) + `session-killed` trace.
    ///
    /// # Errors
    ///
    /// As [`Supervisor::stop_session`].
    pub async fn kill_session(&self, uid: u32) -> Result<SessionStatus, SuperviseError> {
        self.request_stop(uid, Control::HardKill).await
    }

    async fn request_stop(
        &self,
        uid: u32,
        control: Control,
    ) -> Result<SessionStatus, SuperviseError> {
        let (sender, done) = {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            match live.get_mut(&uid) {
                Some(entry) => (entry.control.clone(), entry.done.take()),
                None => return Err(SuperviseError::NotLive(uid)),
            }
        };
        // A finished task's receiver is gone — sending fails harmlessly.
        let _ = sender.send(control);
        let joined = match done {
            Some(handle) => handle
                .await
                .map_err(|e| SuperviseError::TaskFailed(format!("uid {uid}: {e}"))),
            None => Ok(()),
        };
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uid);
        joined?;
        let status = self
            .session_record(uid)?
            .ok_or(SuperviseError::NoRecord(uid))?
            .status;
        Ok(status)
    }

    /// Gracefully stop every live session (sorted by uid — daemon shutdown).
    pub async fn shutdown(&self) -> BTreeMap<u32, Result<SessionStatus, SuperviseError>> {
        let uids: Vec<u32> = {
            let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            live.keys()
                .copied()
                .filter(|uid| {
                    live.get(uid).is_some_and(|entry| {
                        entry
                            .done
                            .as_ref()
                            .is_none_or(|handle| !handle.is_finished())
                    })
                })
                .collect()
        };
        let mut out = BTreeMap::new();
        for uid in uids {
            out.insert(uid, self.stop_session(uid).await);
        }
        out
    }

    /// Restart-resumability (CONVENTIONS §3 r6): after a daemon restart
    /// against the same tree, every session recorded *alive* in the charter
    /// is an orphan (its exec pipes died with the old daemon; re-attach is
    /// impossible over `podman exec` stdio) — mark it `crashed` and trace.
    /// Returns the healed uids. Call before any `start_session`.
    ///
    /// # Errors
    ///
    /// Charter failures, or when live tasks exist in this process.
    pub fn recover(&self) -> Result<Vec<u32>, SuperviseError> {
        {
            let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            if live.values().any(|entry| {
                entry
                    .done
                    .as_ref()
                    .is_none_or(|handle| !handle.is_finished())
            }) {
                return Err(SuperviseError::TaskFailed(
                    "recover() refuses to run while sessions are live".to_owned(),
                ));
            }
        }
        let mut healed = Vec::new();
        let commissions: BTreeMap<u32, u64> = {
            let _guard = self
                .inner
                .charter_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let charter = Charter::load(&self.inner.tree.charter())?;
            charter
                .slots
                .iter()
                .map(|(uid, slot)| (*uid, slot.commission.unwrap_or(0)))
                .collect()
        };
        self.inner.update_charter(|charter| {
            let targets: Vec<SessionRecord> = charter
                .sessions
                .values()
                .filter(|record| record.status.is_alive())
                .copied()
                .collect();
            for record in targets {
                let status = record.status.transition(SessionStatus::Crashed)?;
                charter
                    .sessions
                    .insert(record.id.uid, SessionRecord { status, ..record });
                healed.push(record.id.uid);
            }
            Ok(())
        })?;
        for uid in &healed {
            self.inner.trace(
                AgentKey {
                    commission: commissions.get(uid).copied().unwrap_or(0),
                    uid: *uid,
                },
                "session-orphaned-recovered",
                format!("session for uid {uid} marked crashed after daemon restart"),
            );
        }
        Ok(healed)
    }

    /// Simulate abrupt daemon death (kill/restart tests, CONVENTIONS §3
    /// r6): SIGKILL every session child and abort every supervision task
    /// **without touching the charter** — the tree keeps the stale alive
    /// statuses a `kill -9`ed daemon would leave for [`Supervisor::recover`].
    pub fn abandon(&self) {
        let killers: Vec<mpsc::UnboundedSender<()>> = self
            .inner
            .killers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for killer in killers {
            let _ = killer.send(());
        }
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        for entry in live.values() {
            if let Some(handle) = &entry.done {
                handle.abort();
            }
        }
        live.clear();
    }
}

// ---------------------------------------------------------------------------
// Supervision task
// ---------------------------------------------------------------------------

struct TaskCtx {
    inner: Arc<Inner>,
    uid: u32,
    container: String,
    commission: u64,
    base_env: BTreeMap<String, String>,
    base_cmd: Vec<String>,
}

impl TaskCtx {
    fn agent_key(&self) -> AgentKey {
        AgentKey {
            commission: self.commission,
            uid: self.uid,
        }
    }

    fn ready_timeout(&self) -> Duration {
        Duration::from_millis(self.inner.config.sessions.ready_timeout_ms)
    }

    fn stop_grace(&self) -> Duration {
        Duration::from_millis(self.inner.config.sessions.stop_grace_ms)
    }

    fn spec_for(&self, session: SessionId) -> SessionExecSpec {
        SessionExecSpec {
            container: self.container.clone(),
            uid: self.uid,
            env: self.base_env.clone(),
            cmd: cmd_for_incarnation(
                &self.base_cmd,
                &self.inner.config.sessions,
                session.incarnation,
            ),
        }
    }

    fn trace(&self, event: &str, detail: impl Into<String>) {
        self.inner.trace(self.agent_key(), event, detail);
    }

    fn set_session(
        &self,
        session: SessionId,
        status: SessionStatus,
        restart_count: u32,
    ) -> Result<(), SuperviseError> {
        self.inner.set_session(SessionRecord {
            id: session,
            status,
            restart_count,
        })
    }
}

/// How a restart-backoff wait ended.
enum BackoffWait {
    /// The delay elapsed; the restart proceeds.
    Elapsed,
    /// The control channel delivered a stop/kill (`Some`) or closed (`None`).
    Control(Option<Control>),
}

/// How one life ended (drives the restart policy).
enum LifeEnd {
    /// Daemon-intended end: graceful drain (`killed: false`) or hard kill.
    Stopped {
        killed: bool,
        code: i32,
        status: SessionStatus,
        restarts: u32,
    },
    /// Unexpected end → restart policy.
    Crashed {
        kind: CrashKind,
        code: Option<i32>,
        status: SessionStatus,
        restarts: u32,
        detail: String,
    },
}

#[derive(Clone, Copy)]
enum CrashKind {
    Exit,
    ReadinessTimeout,
    ReadinessRefused,
    SpawnFailed,
    Io,
}

impl CrashKind {
    fn trace_event(self) -> &'static str {
        match self {
            Self::Exit | Self::Io => "session-crashed",
            Self::ReadinessTimeout => "session-readiness-timeout",
            Self::ReadinessRefused => "session-readiness-refused",
            Self::SpawnFailed => "session-spawn-failed",
        }
    }
}

/// The per-UID supervision task: runs the life chain (restarts with
/// backoff) until a stop/kill, budget exhaustion, or incarnation overflow.
async fn supervise_lives(
    ctx: Arc<TaskCtx>,
    live: Arc<Mutex<BTreeMap<u32, LiveEntry>>>,
    generation: u64,
    first_session: SessionId,
    first_handle: Box<dyn ExecHandle>,
    mut control: mpsc::UnboundedReceiver<Control>,
) {
    let uid = ctx.uid;
    let mut session = first_session;
    let mut restarts = 0u32;
    let mut handle = Some(first_handle);

    'chain: loop {
        // --- obtain this life's process (pre-spawned first life, or spawn
        //     a restarted incarnation: charter `starting` first, then exec)
        let spawned = match handle.take() {
            Some(pre) => Ok(pre),
            None => {
                if let Err(e) = ctx.set_session(session, SessionStatus::Starting, restarts) {
                    ctx.trace(
                        "session-spawn-failed",
                        format!("charter starting-write failed: {e}"),
                    );
                    break;
                }
                ctx.trace(
                    "session-spawned",
                    format!(
                        "container={} session={} restarts={restarts}",
                        ctx.container, session
                    ),
                );
                ctx.inner
                    .spawner
                    .spawn_session(&ctx.spec_for(session))
                    .await
            }
        };
        let end = match spawned {
            Ok(live_handle) => run_life(&ctx, session, live_handle, &mut control, restarts).await,
            Err(e) => {
                let status = ctx
                    .inner
                    .session_record(uid)
                    .ok()
                    .flatten()
                    .map_or(SessionStatus::Starting, |r| r.status);
                LifeEnd::Crashed {
                    kind: CrashKind::SpawnFailed,
                    code: None,
                    status,
                    restarts,
                    detail: scrub_detail(&e.to_string()),
                }
            }
        };

        // --- classify + persist
        match end {
            LifeEnd::Stopped {
                killed,
                code,
                status,
                restarts: final_restarts,
            } => {
                if let Err(e) = ctx.set_session(session, SessionStatus::Stopped, final_restarts) {
                    ctx.trace("session-telemetry-failed", format!("charter: {e}"));
                }
                let (event, detail) = if killed {
                    ("session-killed", format!("hard kill, exit={code}"))
                } else {
                    (
                        "session-stopped",
                        format!("graceful drain, exit={code}, from {status}"),
                    )
                };
                ctx.trace(event, detail);
                break 'chain;
            }
            LifeEnd::Crashed {
                kind,
                code,
                status,
                restarts: live_restarts,
                detail,
            } => {
                restarts = live_restarts;
                if let Err(e) = ctx.set_session(session, SessionStatus::Crashed, restarts) {
                    ctx.trace("session-telemetry-failed", format!("charter: {e}"));
                    // Charter is truth: without it the restart policy has no
                    // persisted counter — end the chain rather than guess.
                    break 'chain;
                }
                ctx.trace(
                    kind.trace_event(),
                    format!(
                        "session={} from={status} exit={} · {detail}",
                        session,
                        code.map_or("none".to_owned(), |c| c.to_string())
                    ),
                );
                match ctx.inner.config.sessions.restart.backoff_ms(restarts) {
                    None => {
                        permanent_crash(&ctx, session, restarts, &detail);
                        ctx.inner.notify_crash(CrashNotice {
                            uid,
                            session,
                            restarts,
                            permanent: true,
                            detail: scrub_detail(&detail),
                        });
                        break 'chain;
                    }
                    Some(delay_ms) => {
                        ctx.trace(
                            "session-restart-scheduled",
                            format!("session={session} restarts={restarts} backoff_ms={delay_ms}"),
                        );
                        // Loop-select: only stop/kill cancels the restart;
                        // delivery pokes stay queued (file-as-truth — the
                        // next life drains) and interrupts are refused
                        // typed. The pinned timer fires once → break, so
                        // no arm can spin without yielding (step-02
                        // gotcha 2).
                        let mut backoff = Box::pin(sleep(Duration::from_millis(delay_ms)));
                        let waited = loop {
                            tokio::select! {
                                () = &mut backoff => break BackoffWait::Elapsed,
                                msg = control.recv() => match msg {
                                    Some(stop @ (Control::GracefulStop | Control::HardKill)) => {
                                        break BackoffWait::Control(Some(stop))
                                    }
                                    Some(Control::Deliver) => continue,
                                    Some(Control::Interrupt { envelope, reply }) => {
                                        let correlation = envelope.correlation().clone();
                                        let refusal = Err(SuperviseError::Session(
                                            SessionError::NotAlive(SessionStatus::Crashed),
                                        ));
                                        ctx.trace(
                                            "session-interrupt-refused",
                                            format!(
                                                "{correlation} session={session} not alive (restart backoff)"
                                            ),
                                        );
                                        let _ = reply.send(refusal);
                                        continue;
                                    }
                                    None => break BackoffWait::Control(None),
                                },
                            }
                        };
                        match waited {
                            BackoffWait::Elapsed => {} // backoff elapsed → restart
                            BackoffWait::Control(Some(_control)) => {
                                ctx.trace(
                                    "session-stop-during-backoff",
                                    format!("session={session} restart cancelled; stays crashed"),
                                );
                                break 'chain;
                            }
                            // All senders dropped (daemon teardown): stay crashed.
                            BackoffWait::Control(None) => break 'chain,
                        }
                        restarts += 1;
                        // The daemon will revive this life: a restartable
                        // crash is *observable*, but it is not a Mayor wake
                        // under the default policy (ADR-0029 cl. 2).
                        ctx.inner.notify_crash(CrashNotice {
                            uid,
                            session,
                            restarts,
                            permanent: false,
                            detail: scrub_detail(&detail),
                        });
                        match session.next_incarnation() {
                            Ok(next) => session = next,
                            Err(e) => {
                                ctx.trace(
                                    "session-permanently-crashed",
                                    format!("incarnation overflow: {e}"),
                                );
                                break 'chain;
                            }
                        }
                    }
                }
            }
        }
    }

    // Remove our live entry (generation-matched: never a newer session's).
    // (Per-life killer registrations are released by `run_life` itself.)
    let mut live_map = live.lock().unwrap_or_else(|e| e.into_inner());
    let matches = live_map
        .get(&uid)
        .is_some_and(|entry| entry.generation == generation);
    if matches {
        live_map.remove(&uid);
    }
}

/// Permanent failure: TITLE.md lineage stamp + sled trace (SPEC §6).
fn permanent_crash(ctx: &TaskCtx, session: SessionId, restarts: u32, detail: &str) {
    let line = title_crash_line(&session, restarts, unix_now_ms() / 1000, detail);
    if let Err(e) = stamp_title(&ctx.inner.tree, ctx.uid, &line) {
        ctx.trace(
            "session-title-stamp-failed",
            format!("session={session}: {e}"),
        );
    }
    ctx.trace(
        "session-permanently-crashed",
        format!("session={session} restarts={restarts} budget exhausted · {detail}"),
    );
}

/// Append one lineage line to `parcel<uid>/TITLE.md` with ADR-0007
/// discipline: existing regular file only — never create (lineage must
/// descend from provisioning), never follow a planted symlink.
fn stamp_title(tree: &HamletTree, uid: u32, line: &str) -> Result<(), SuperviseError> {
    let path = tree.parcel_title(uid);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => {
            return Err(SuperviseError::TaskFailed(format!(
                "TITLE.md {} is not a regular file",
                path.display()
            )))
        }
        Err(e) => {
            return Err(SuperviseError::Io {
                context: format!("stat {}", path.display()),
                source: e,
            })
        }
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .map_err(|e| SuperviseError::Io {
            context: format!("append {}", path.display()),
            source: e,
        })?;
    use std::io::Write;
    file.write_all(line.as_bytes())
        .map_err(|e| SuperviseError::Io {
            context: format!("append {}", path.display()),
            source: e,
        })
}

/// One session life: spawn-handed process → readiness → event loop →
/// classified end. Owns stdin (single writer) and the stdout reader; a
/// reaper task owns wait/kill.
///
/// This wrapper owns the life's **telemetry sink** ([`crate::metrics`]): it
/// spawns the per-session sink task, hands the fold to [`run_life_inner`],
/// and guarantees [`SessionMetrics::finish`] runs on *every* exit path —
/// including crash and kill — so open latency spans are closed and the
/// per-life `session-stream-summary` rollup is always written.
async fn run_life(
    ctx: &TaskCtx,
    session: SessionId,
    handle: Box<dyn ExecHandle>,
    control: &mut mpsc::UnboundedReceiver<Control>,
    restarts: u32,
) -> LifeEnd {
    let mut metrics = metrics::spawn(
        Arc::clone(&ctx.inner.telemetry),
        ctx.agent_key(),
        SessionKey::new(ctx.commission, ctx.uid, session.as_u64()),
        metrics::ExtractorConfig::new(ctx.inner.config.sessions.latency_spike_ms),
    );
    let end = run_life_inner(ctx, session, handle, control, restarts, &mut metrics).await;
    let report = metrics.finish(unix_now_ms()).await;
    if report.failures > 0 {
        // Telemetry loss degrades, never corrupts state (ADR-0005 cl. 6):
        // traced, and supervision carries on with its classification.
        ctx.trace(
            "session-telemetry-failed",
            format!(
                "session={session} {} sled write(s) failed in the metrics sink",
                report.failures
            ),
        );
    }
    end
}

async fn run_life_inner(
    ctx: &TaskCtx,
    session: SessionId,
    mut handle: Box<dyn ExecHandle>,
    control: &mut mpsc::UnboundedReceiver<Control>,
    mut restarts: u32,
    metrics: &mut metrics::SessionMetrics,
) -> LifeEnd {
    let mut status = SessionStatus::Starting;
    let Some(pipes) = handle.take_pipes() else {
        return LifeEnd::Crashed {
            kind: CrashKind::Io,
            code: None,
            status,
            restarts,
            detail: "exec handle had no pipes".to_owned(),
        };
    };

    // stderr: diagnostics only, never protocol (ADR-0015 cl. 1) — drained
    // into a bounded tail for crash traces.
    let stderr_tail = Arc::new(Mutex::new(VecDeque::<u8>::new()));
    tokio::spawn(drain_stderr(pipes.stderr, Arc::clone(&stderr_tail)));

    // Reaper: owns the child; resolves exit or performs a requested kill.
    let (exit_tx, mut exit_rx) = oneshot::channel::<Result<i32, PodmanError>>();
    let (kill_tx, kill_rx) = mpsc::unbounded_channel::<()>();
    ctx.inner.register_killer(session, kill_tx.clone());
    tokio::spawn(reaper(handle, kill_rx, exit_tx));

    let mut reader = BufReader::new(pipes.stdout).lines();
    let mut ready = false;
    let mut draining = false;
    let mut killed = false;
    // A queued prompt (or a prompt-shaped interrupt) was written but the
    // stream has not yet confirmed the run (agent_start) or released the
    // boundary (agent_settled) — until then no second envelope may go out
    // (never interleave mid-task, SPEC §6).
    let mut prompt_in_flight = false;
    let probe_id = session.probe_id();

    // Readiness handshake: pi RPC emits no banner (ADR-0015 cl. 3). The
    // probe is written while stdin is still owned outright — valid by
    // construction, no Option dance before the loop.
    let mut stdin = pipes.stdin;
    let probe_result = write_record(&mut stdin, &readiness_probe(&session).to_json_line()).await;
    // From here stdin is optional: a stop request CLOSES it (drop = EOF).
    let mut stdin: Option<Box<dyn AsyncWrite + Unpin + Send>> = Some(stdin);
    if let Err(e) = probe_result {
        ctx.inner.unregister_killer(session);
        return LifeEnd::Crashed {
            kind: CrashKind::Io,
            code: None,
            status,
            restarts,
            detail: scrub_detail(&format!("probe write: {e}")),
        };
    }

    let mut ready_timer = Box::pin(sleep(ctx.ready_timeout()));
    let mut grace_timer = Box::pin(sleep(ctx.stop_grace()));
    // Flush-retry tick: armed only while the metrics handle holds coalesced
    // state the channel refused, so an idle session pays nothing and a
    // stream that goes quiet at a task boundary still publishes its totals
    // to `metrics_for` within one interval (metrics::FLUSH_RETRY_INTERVAL).
    let mut flush_timer = Box::pin(sleep(metrics::FLUSH_RETRY_INTERVAL));

    loop {
        enum Step {
            Line(String),
            Eof,
            Exit(Result<i32, PodmanError>),
            Control(Option<Control>),
            ReadyTimeout,
            GraceTimeout,
            /// Retry a metrics flush the observation channel refused.
            FlushTick,
        }
        let step = tokio::select! {
            line = reader.next_line() => match line {
                Ok(Some(line)) => Step::Line(line),
                Ok(None) => Step::Eof,
                Err(e) => Step::Exit(Err(PodmanError::spawn(
                    format!("stdout read: {e}"), Vec::new()))),
            },
            exit = &mut exit_rx => Step::Exit(exit.unwrap_or_else(|_| Err(
                PodmanError::spawn("reaper task vanished", Vec::new())))),
            msg = control.recv() => Step::Control(msg),
            () = &mut ready_timer, if !ready => Step::ReadyTimeout,
            () = &mut grace_timer, if draining => Step::GraceTimeout,
            () = &mut flush_timer, if metrics.has_pending() => Step::FlushTick,
        };
        match step {
            Step::Line(line) => {
                let event = parse_agent_event(&line);
                // Daemon-parsed telemetry (SPEC §6): the fold is pure and
                // runs before any supervision branch, so pre-readiness and
                // malformed frames are measured too and nothing an agent
                // self-reports is ever believed.
                metrics.observe(unix_now_ms(), &event);
                match event {
                    AgentEvent::Response { id, success, .. }
                        if !ready && id.as_deref() == Some(probe_id.as_str()) =>
                    {
                        if !success {
                            let _ = kill_tx.send(());
                            ctx.inner.unregister_killer(session);
                            return LifeEnd::Crashed {
                                kind: CrashKind::ReadinessRefused,
                                code: None,
                                status,
                                restarts,
                                detail: "readiness probe answered with success=false".to_owned(),
                            };
                        }
                        ready = true;
                        status = SessionStatus::Running;
                        if let Err(e) = ctx.set_session(session, status, restarts) {
                            ctx.trace(
                                "session-telemetry-failed",
                                format!("charter running-write: {e}"),
                            );
                        }
                        ctx.trace("session-ready", format!("session={session}"));
                        // First task boundary of this life: drain the
                        // persistent queue (the register brief is its
                        // first tasking envelope).
                        try_drain(
                            ctx,
                            DrainGate {
                                session,
                                status,
                                ready,
                                draining,
                            },
                            &mut prompt_in_flight,
                            &mut stdin,
                            metrics,
                        )
                        .await;
                    }
                    event => {
                        if !ready {
                            continue; // pre-readiness events: Starting reducer is inert
                        }
                        match event {
                            AgentEvent::AgentSettled if status == SessionStatus::Busy => {
                                status = SessionStatus::Idle;
                                // Task boundary reached: the restart budget
                                // resets and the next queued envelope may go
                                // out (sequential delivery, SPEC §6).
                                restarts = 0;
                                save_status(ctx, session, status, restarts);
                                prompt_in_flight = false;
                                try_drain(
                                    ctx,
                                    DrainGate {
                                        session,
                                        status,
                                        ready,
                                        draining,
                                    },
                                    &mut prompt_in_flight,
                                    &mut stdin,
                                    metrics,
                                )
                                .await;
                            }
                            AgentEvent::AgentStart
                                if matches!(
                                    status,
                                    SessionStatus::Running | SessionStatus::Idle
                                ) =>
                            {
                                status = SessionStatus::Busy;
                                // The stream confirms the run our prompt
                                // started — the drain gate releases only
                                // here, on settle, or on a rejected prompt.
                                prompt_in_flight = false;
                                save_status(ctx, session, status, restarts);
                            }
                            AgentEvent::Response {
                                success: false,
                                id,
                                command,
                                error,
                                ..
                            } => {
                                if id.as_deref().is_some_and(|id| id.ends_with(":p")) {
                                    // Our prompt (queued delivery or
                                    // prompt-shaped interrupt) was refused:
                                    // release the drain gate; the envelope
                                    // is lost (at-most-once) — Sprint 04
                                    // consumes the trace for bounces.
                                    prompt_in_flight = false;
                                    ctx.trace(
                                        "session-delivery-rejected",
                                        format!(
                                            "command {command} rejected (id={id:?}): {}",
                                            error.unwrap_or_default()
                                        ),
                                    );
                                } else {
                                    ctx.trace(
                                        "session-telemetry-failed",
                                        format!(
                                            "command {command} rejected (id={id:?}): {}",
                                            error.unwrap_or_default()
                                        ),
                                    );
                                }
                            }
                            // Turn/usage/other/malformed: telemetry consumers
                            // (Sprint 03 step 04) — never fatal here.
                            _ => {}
                        }
                    }
                }
            }
            Step::Eof => {
                // stdout closed: the process is exiting — collect its code
                // within the grace window, then classify.
                let code = match timeout(ctx.stop_grace(), &mut exit_rx).await {
                    Ok(Ok(Ok(code))) => code,
                    Ok(_) => SIGNAL_EXIT_CODE, // reaper vanished or wait failed
                    Err(_) => {
                        let _ = kill_tx.send(());
                        killed = true;
                        SIGNAL_EXIT_CODE
                    }
                };
                ctx.inner.unregister_killer(session);
                return classify(code, draining, killed, status, restarts, &stderr_tail);
            }
            Step::Exit(result) => {
                let code = result.unwrap_or(SIGNAL_EXIT_CODE);
                // QA05-02: the process is gone, but stdout may still hold
                // frames it emitted before dying — the reaper's exit and the
                // reader's buffered lines are ready simultaneously and this
                // arm can win the race. Drain before classifying, or the
                // telemetry tail (and, on a crash, the frames describing the
                // death itself) is silently lost.
                drain_tail(
                    ctx,
                    session,
                    &mut reader,
                    metrics,
                    &mut status,
                    &mut restarts,
                )
                .await;
                ctx.inner.unregister_killer(session);
                return classify(code, draining, killed, status, restarts, &stderr_tail);
            }
            Step::Control(Some(Control::GracefulStop)) if !draining => {
                draining = true;
                grace_timer = Box::pin(sleep(ctx.stop_grace()));
                // Close stdin: EOF is pi's orderly-shutdown signal
                // (ADR-0015 cl. 4 stop_grace semantics). Undelivered queue
                // entries are NOT drained on stop — they stay persisted
                // (file-as-truth) for the next session. NOTE: tokio's
                // `ChildStdin::poll_shutdown` is a no-op — the write end
                // closes on DROP, so drop it.
                drop(stdin.take());
                let pending = ctx.inner.queues.len(ctx.uid).unwrap_or(0);
                ctx.trace(
                    "session-stop-requested",
                    format!(
                        "session={session} graceful drain, from {status}, {pending} undelivered stay queued"
                    ),
                );
            }
            Step::Control(Some(Control::HardKill)) if !killed => {
                draining = true;
                killed = true;
                let _ = kill_tx.send(());
                ctx.trace(
                    "session-killed",
                    format!("session={session} hard kill requested, from {status}"),
                );
            }
            Step::Control(Some(Control::Deliver)) => {
                // Enqueue poke: deliver when at a task boundary; otherwise
                // the envelope waits for the next agent_settled.
                try_drain(
                    ctx,
                    DrainGate {
                        session,
                        status,
                        ready,
                        draining,
                    },
                    &mut prompt_in_flight,
                    &mut stdin,
                    metrics,
                )
                .await;
            }
            Step::Control(Some(Control::Interrupt { envelope, reply })) => {
                // Divine Interruption: injected through the same writer as
                // the queue drain (single-writer funnel — framing can never
                // interleave), but immediately, regardless of boundaries.
                let outcome = inject_interrupt(
                    ctx,
                    DrainGate {
                        session,
                        status,
                        ready,
                        draining,
                    },
                    &mut stdin,
                    &envelope,
                    metrics,
                )
                .await;
                if outcome.is_ok() && status.accepts_delivery() {
                    // Prompt-shaped injection (running/idle): gate the
                    // queue until the stream shows the run.
                    prompt_in_flight = true;
                }
                let _ = reply.send(outcome);
            }
            Step::Control(Some(_)) => {} // duplicate stop/kill: already draining
            Step::Control(None) => {
                // Supervisor dropped without stop (daemon teardown): drain
                // gracefully rather than orphan the child.
                draining = true;
                grace_timer = Box::pin(sleep(ctx.stop_grace()));
                drop(stdin.take());
            }
            Step::FlushTick => {
                metrics.retry_flush();
                // Re-pin: a fired `sleep` stays ready, so without this the
                // arm would spin without yielding (step-02 gotcha 2). When
                // nothing is pending the guard disables the arm entirely,
                // so the elapsed timer costs nothing.
                flush_timer = Box::pin(sleep(metrics::FLUSH_RETRY_INTERVAL));
            }
            Step::ReadyTimeout => {
                let _ = kill_tx.send(());
                ctx.inner.unregister_killer(session);
                return LifeEnd::Crashed {
                    kind: CrashKind::ReadinessTimeout,
                    code: None,
                    status,
                    restarts,
                    detail: format!(
                        "no readiness response within {}ms · stderr: {}",
                        ctx.inner.config.sessions.ready_timeout_ms,
                        tail_string(&stderr_tail)
                    ),
                };
            }
            Step::GraceTimeout => {
                let _ = kill_tx.send(());
                ctx.trace(
                    "session-grace-expired",
                    format!("session={session} hard kill after grace window"),
                );
                // Do NOT return to the select loop here: the elapsed timer
                // arm would stay ready and spin without yielding (starving
                // the reaper on a current-thread runtime). Await the exit
                // with a bounded margin, then classify.
                let code = match timeout(ctx.stop_grace(), &mut exit_rx).await {
                    Ok(Ok(Ok(code))) => code,
                    _ => SIGNAL_EXIT_CODE,
                };
                ctx.inner.unregister_killer(session);
                return classify(code, true, true, status, restarts, &stderr_tail);
            }
        }
    }
}

fn save_status(ctx: &TaskCtx, session: SessionId, status: SessionStatus, restarts: u32) {
    if let Err(e) = ctx.set_session(session, status, restarts) {
        ctx.trace(
            "session-telemetry-failed",
            format!("charter {status}-write: {e}"),
        );
    }
}

/// Drain stdout to EOF after the process has exited (QA05-02).
///
/// The reaper's exit and the reader's remaining buffered frames become ready
/// at the same instant, and `tokio::select!` may take the exit arm first —
/// so without this drain the frames an agent emitted before dying are never
/// folded: the tail of a stopped run's accounting (final cumulative usage
/// snapshots, a trailing `agent_settled`) and, on the crash path, the frames
/// describing the death itself — the most forensic part of the stream
/// (SPEC §6; ADR-0017 cl. 2 lossless accounting).
///
/// Two deliberate limits:
///
/// - **Bounded by one stop-grace window.** Normally the write end died with
///   the process and every buffered line reads back instantly; the budget
///   only matters when something else (a `podman exec` intermediate, an
///   orphaned grandchild) still holds stdout open, which must not wedge the
///   life task. Exceeding it is traced, never silent.
/// - **No delivery.** stdin is gone, and popping an envelope only to fail the
///   write would lose it — `pop` persists the removal *before* the write
///   (at-most-once, [`crate::delivery`]). The tail is read-only: telemetry
///   plus the pure status reducer, so the classification and the restart
///   budget see the boundary the stream actually reached.
async fn drain_tail<R>(
    ctx: &TaskCtx,
    session: SessionId,
    reader: &mut Lines<R>,
    metrics: &mut metrics::SessionMetrics,
    status: &mut SessionStatus,
    restarts: &mut u32,
) where
    R: AsyncBufRead + Unpin,
{
    let mut folded = 0u64;
    let drain = async {
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    folded += 1;
                    let event = parse_agent_event(&line);
                    metrics.observe(unix_now_ms(), &event);
                    let next = status.on_agent_event(&event);
                    if next == SessionStatus::Idle && *status != SessionStatus::Idle {
                        // Task boundary reached in the tail: the restart
                        // budget resets exactly as it would have live.
                        *restarts = 0;
                    }
                    *status = next;
                }
                Ok(None) => break, // clean EOF — the whole tail is folded
                Err(_) => break,   // read error: nothing more is coming
            }
        }
    };
    if timeout(ctx.stop_grace(), drain).await.is_err() {
        ctx.trace(
            "session-stream-tail-truncated",
            format!(
                "session={session} stdout still open one stop-grace after exit; \
                 {folded} tail frame(s) folded, remainder dropped"
            ),
        );
    }
}

fn classify(
    code: i32,
    draining: bool,
    killed: bool,
    status: SessionStatus,
    restarts: u32,
    stderr_tail: &Mutex<VecDeque<u8>>,
) -> LifeEnd {
    if draining {
        return LifeEnd::Stopped {
            killed,
            code,
            status,
            restarts,
        };
    }
    LifeEnd::Crashed {
        kind: CrashKind::Exit,
        code: Some(code),
        status,
        restarts,
        detail: scrub_detail(&format!(
            "exit code {code} · stderr: {}",
            tail_string(stderr_tail)
        )),
    }
}

/// The inputs to every write-side decision in a life: which session, what
/// the parsed stream says about it, and whether the task may still write at
/// all. One value object so the gate cannot be evaluated inconsistently
/// between a queue drain and an interrupt injection (SPEC §6).
#[derive(Debug, Clone, Copy)]
struct DrainGate {
    session: SessionId,
    status: SessionStatus,
    ready: bool,
    draining: bool,
}

impl DrainGate {
    /// May a queued envelope go out right now? Readiness reached, not
    /// draining toward stop, and the parsed status is at a task boundary
    /// (`running`/`idle`).
    fn allows_delivery(self) -> bool {
        self.ready && !self.draining && self.status.accepts_delivery()
    }
}

/// Attempt one queued delivery at a task boundary (SPEC §6: sequential,
/// never interleaved mid-task). Gates: [`DrainGate::allows_delivery`] plus
/// no prompt awaiting stream confirmation. Pops at most ONE envelope — the
/// next drain trigger is the next boundary event or an enqueue poke.
async fn try_drain(
    ctx: &TaskCtx,
    gate: DrainGate,
    prompt_in_flight: &mut bool,
    stdin: &mut Option<Box<dyn AsyncWrite + Unpin + Send>>,
    metrics: &mut metrics::SessionMetrics,
) {
    if !gate.allows_delivery() || *prompt_in_flight {
        return;
    }
    let Some(stdin) = stdin.as_mut() else {
        return;
    };
    *prompt_in_flight = drain_one(ctx, gate, stdin, metrics).await;
}

/// Pop the front envelope and write its pi-binding records contiguously.
/// Returns whether a prompt is now in flight (the caller gates the next
/// drain on `agent_start`/`agent_settled`/rejection). Pop persists the
/// removal BEFORE the write (at-most-once, `crate::delivery`); write or
/// binding failures are traced, never fatal.
async fn drain_one(
    ctx: &TaskCtx,
    gate: DrainGate,
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    metrics: &mut metrics::SessionMetrics,
) -> bool {
    let session = gate.session;
    let envelope = match ctx.inner.queues.pop(ctx.uid) {
        Ok(Some(envelope)) => envelope,
        Ok(None) => return false,
        Err(e) => {
            ctx.trace(
                "session-delivery-failed",
                format!("session={session} queue pop: {e}"),
            );
            return false;
        }
    };
    let correlation = envelope.correlation();
    let kind = envelope
        .queued_kind()
        .map_or_else(|| "envelope".to_owned(), |kind| kind.to_string());
    match pi_binding(&envelope, gate.status) {
        Ok(commands) => {
            for command in &commands {
                if let Err(e) = write_record(stdin, &command.to_json_line()).await {
                    ctx.trace(
                        "session-delivery-failed",
                        format!("{kind} {correlation}: write: {e}"),
                    );
                    return false;
                }
                // Telemetry joins the write to the agent's `response`
                // (`delivery-ack` latency, ADR-0016 cl. 1).
                metrics.note_sent(command.id(), unix_now_ms());
            }
            ctx.trace(
                "session-delivered",
                format!("{kind} {correlation} session={session}"),
            );
            true
        }
        Err(e) => {
            ctx.trace(
                "session-delivery-failed",
                format!("{kind} {correlation}: binding at {}: {e}", gate.status),
            );
            false
        }
    }
}

/// Inject the `interrupt_and_steer` envelope down the live pipe NOW —
/// bypassing the queue, concurrent with any in-flight task (SPEC §6
/// Divine Interruption). Goes through the same single writer as the queue
/// drain (the life task's select loop), so an interrupt landing mid-drain
/// can never interleave records. Busy → `[clear_queue?] + steer + abort`;
/// running/idle → one header-marked prompt; starting/draining/dead →
/// typed refusal. Every record's wire id carries the correlation
/// (`<corr>:{q,s,a,p}`), tying the interrupt to the resulting stream
/// events.
async fn inject_interrupt(
    ctx: &TaskCtx,
    gate: DrainGate,
    stdin: &mut Option<Box<dyn AsyncWrite + Unpin + Send>>,
    envelope: &SessionEnvelope,
    metrics: &mut metrics::SessionMetrics,
) -> Result<CorrelationId, SuperviseError> {
    let session = gate.session;
    let correlation = envelope.correlation().clone();
    let (source, priority) = match envelope {
        SessionEnvelope::InterruptAndSteer(interrupt) => (interrupt.source, interrupt.priority),
        _ => {
            return Err(SuperviseError::TaskFailed(
                "interrupt control carried a queued envelope".to_owned(),
            ))
        }
    };
    let result: Result<(), SuperviseError> = async {
        if gate.draining {
            return Err(SuperviseError::Draining(ctx.uid));
        }
        let stdin = stdin.as_mut().ok_or(SuperviseError::Draining(ctx.uid))?;
        for command in pi_binding(envelope, gate.status)? {
            write_record(stdin, &command.to_json_line())
                .await
                .map_err(|e| SuperviseError::Io {
                    context: "interrupt write".to_owned(),
                    source: e,
                })?;
            // Each interrupt record is individually correlatable
            // (`<corr>:{q,s,a,p}`) so its ack latency is measured per
            // record (ADR-0016 cl. 1).
            metrics.note_sent(command.id(), unix_now_ms());
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            ctx.trace(
                "session-interrupt-injected",
                format!(
                    "{correlation} source={source} priority={priority} from {} session={session}",
                    gate.status
                ),
            );
            Ok(correlation)
        }
        Err(e) => {
            ctx.trace(
                "session-interrupt-refused",
                format!("{correlation} source={source} priority={priority}: {e}"),
            );
            Err(e)
        }
    }
}

async fn write_record(
    stdin: &mut Box<dyn AsyncWrite + Unpin + Send>,
    line: &str,
) -> io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

/// Bounded stderr tail (diagnostics only — never protocol, never secrets:
/// podman `-e` values are masked in every argv render).
async fn drain_stderr(
    mut stderr: Box<dyn AsyncRead + Unpin + Send>,
    tail: Arc<Mutex<VecDeque<u8>>>,
) {
    let mut chunk = [0u8; 1024];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                tail.extend(chunk.iter().take(n));
                while tail.len() > STDERR_TAIL_CAP {
                    tail.pop_front();
                }
            }
        }
    }
}

fn tail_string(tail: &Mutex<VecDeque<u8>>) -> String {
    let tail = tail.lock().unwrap_or_else(|e| e.into_inner());
    let bytes: Vec<u8> = tail.iter().copied().collect();
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// Owns the child handle: resolves the exit code, or kills on request and
/// reports [`SIGNAL_EXIT_CODE`]. The `poll_fn` shape is deliberate: it ends
/// the `wait` borrow before `kill` runs (a `select!` arm body cannot
/// reborrow the handle its sibling arm's future still holds).
async fn reaper(
    mut handle: Box<dyn ExecHandle>,
    mut kill_rx: mpsc::UnboundedReceiver<()>,
    exit_tx: oneshot::Sender<Result<i32, PodmanError>>,
) {
    enum Outcome {
        Waited(Result<i32, PodmanError>),
        KillRequested,
    }
    let mut wait_fut = handle.wait();
    let mut kill_fut = Box::pin(kill_rx.recv());
    let outcome = std::future::poll_fn(|cx| {
        if let Poll::Ready(result) =
            std::future::Future::poll(std::pin::Pin::new(&mut wait_fut), cx)
        {
            return Poll::Ready(Outcome::Waited(result));
        }
        match std::future::Future::poll(std::pin::Pin::new(&mut kill_fut), cx) {
            Poll::Ready(Some(())) => Poll::Ready(Outcome::KillRequested),
            // Senders dropped: no kill will arrive; keep waiting for exit.
            Poll::Ready(None) | Poll::Pending => Poll::Pending,
        }
    })
    .await;
    drop(wait_fut); // release the &mut borrow before killing
    drop(kill_fut);
    let result = match outcome {
        Outcome::Waited(result) => result,
        Outcome::KillRequested => {
            let _ = handle.kill();
            drop(handle); // kill_on_drop backstop: the process cannot survive
            Ok(SIGNAL_EXIT_CODE)
        }
    };
    let _ = exit_tx.send(result);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions() -> SessionsConfig {
        SessionsConfig::default()
    }

    /// Golden: the `.env` → exec-env mapping (ADR-0015 cl. 1; SPEC §11.4).
    #[test]
    fn session_env_golden() {
        let structural: BTreeMap<String, String> = [
            ("HOME".to_owned(), "/home/1001".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ]
        .into_iter()
        .collect();
        let secrets = Secrets::from_vars([
            ("HAMLET_API_KEY_ANTHROPIC", "test-key-do-not-use"),
            ("HAMLET_API_KEY_openai", "test-key-do-not-use-2"),
            ("HAMLET_MODEL_ENDPOINT", "https://models.example/v1"),
            ("HAMLET_MODEL_API_KEY", "test-key-do-not-use-3"),
            ("UNRELATED", "ignored"),
        ]);
        let env = session_env(&structural, &secrets);
        assert_eq!(env.get("HOME").map(String::as_str), Some("/home/1001"));
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("test-key-do-not-use")
        );
        assert_eq!(
            env.get("OPENAI_API_KEY").map(String::as_str),
            Some("test-key-do-not-use-2")
        );
        assert_eq!(
            env.get("HAMLET_MODEL_ENDPOINT").map(String::as_str),
            Some("https://models.example/v1")
        );
        assert_eq!(
            env.get("HAMLET_MODEL_API_KEY").map(String::as_str),
            Some("test-key-do-not-use-3")
        );
        assert_eq!(env.get("PI_TELEMETRY").map(String::as_str), Some("0"));
        assert_eq!(
            env.get("PI_SKIP_VERSION_CHECK").map(String::as_str),
            Some("1")
        );
        assert!(!env.contains_key("UNRELATED"));
        // Sorted rendering is the BTreeMap invariant (CONVENTIONS §3 r1).
        let keys: Vec<&String> = env.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn session_env_skips_non_identifier_providers() {
        let secrets = Secrets::from_vars([("HAMLET_API_KEY_BAD-PROV!DER", "x")]);
        let env = session_env(&BTreeMap::new(), &secrets);
        assert!(!env.keys().any(|k| k.contains("BAD")));
        assert_eq!(env.get("PI_TELEMETRY").map(String::as_str), Some("0"));
    }

    /// Golden: pi argv composition (ADR-0015 cl. 1).
    #[test]
    fn pi_session_cmd_golden() {
        assert_eq!(pi_session_cmd(&sessions()), ["pi", "--mode", "rpc"]);
        let mut cfg = sessions();
        cfg.provider = Some("anthropic".to_owned());
        cfg.model = Some("claude".to_owned());
        assert_eq!(
            pi_session_cmd(&cfg),
            [
                "pi",
                "--mode",
                "rpc",
                "--provider",
                "anthropic",
                "--model",
                "claude"
            ]
        );
        // --continue only for restarted incarnations with the flag on.
        let base = pi_session_cmd(&cfg);
        assert_eq!(cmd_for_incarnation(&base, &cfg, 0), base);
        let mut continued = base.clone();
        continued.push("--continue".to_owned());
        assert_eq!(cmd_for_incarnation(&base, &cfg, 1), continued);
        cfg.continue_on_restart = false;
        assert_eq!(cmd_for_incarnation(&base, &cfg, 3), base);
    }

    /// Golden: TITLE.md lineage line + detail scrubbing.
    #[test]
    fn title_crash_line_golden() {
        let session = SessionId::new(1001, 2).unwrap();
        let line = title_crash_line(&session, 5, 1_700_000_000, "exit code 137");
        assert_eq!(
            line,
            "- unix 1700000000 · session 1001.2 permanently crashed after 5 auto-restart(s) · exit code 137\n"
        );
    }

    #[test]
    fn scrub_detail_neutralizes_hostile_text() {
        assert_eq!(scrub_detail("a\u{0}b\u{1b}[31m"), "a.b.[31m");
        assert_eq!(scrub_detail("  many   spaces\n\t"), "many spaces");
        let long = "x".repeat(400);
        let scrubbed = scrub_detail(&long);
        assert!(scrubbed.chars().count() <= DETAIL_CAP + 1);
        assert!(scrubbed.ends_with('…'));
        assert_eq!(scrub_detail(""), "");
    }
}
