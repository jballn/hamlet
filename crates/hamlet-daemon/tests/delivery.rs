//! Sequential delivery queue + Divine Interruption integration tests
//! (Sprint 03 step 03; SPEC §6, ADR-0015 — CONVENTIONS §9 doctrine).
//!
//! Real supervision + real [`QueueStore`] persistence against the
//! **FakeAgent** binary over [`HostProcessSpawner`] — no podman. Coverage:
//! FIFO ordering under load, task-boundary semantics (never a mid-task
//! write), interrupt mid-task with correlation (normal + critical),
//! interrupt refusals (not-live, restart backoff, stopped), enqueue-poke
//! delivery to an idle session, overflow → typed bounce error, and
//! restart-recovers-queue (daemon death → fresh Supervisor → undelivered
//! envelopes delivered from the tree, CONVENTIONS §3 r6).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::charter::Charter;
use hamlet_core::commission::CommissionRecord;
use hamlet_core::config::{Config, Secrets};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{
    parse_pi_command, pi_binding, InterruptPriority, InterruptSource, QueuedKind, SessionEnvelope,
    SessionError, SessionId, SessionStatus,
};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::title_stub;
use hamlet_daemon::delivery::QueueError;
use hamlet_daemon::supervise::{
    HostProcessSpawner, ParcelRef, SessionSpawner, SuperviseError, Supervisor,
};
use hamlet_daemon::telemetry::{AgentKey, Telemetry, TraceRecord};
use serde_json::{json, Value};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const COMMISSION: u64 = 7;

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

fn agent_key(uid: u32) -> AgentKey {
    AgentKey {
        commission: COMMISSION,
        uid,
    }
}

struct Fixture {
    tmp: tempfile::TempDir,
    tree: HamletTree,
    config: Config,
    telemetry: Arc<Telemetry>,
    supervisor: Supervisor,
    uid: u32,
}

fn fixture(tweak: impl FnOnce(&mut Config)) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionRecord::draft(name("commission")));
    let uid = charter
        .allocate_slot(name("scribe"), Some(COMMISSION))
        .unwrap();
    assert_eq!(uid, 1001);
    charter.save_atomic(&tree.charter()).unwrap();
    fs::create_dir_all(tree.parcel(uid)).unwrap();
    fs::write(
        tree.parcel_title(uid),
        title_stub(&name("scribe"), uid, COMMISSION, 1_700_000_000),
    )
    .unwrap();

    let mut config = Config::default();
    tweak(&mut config);
    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Supervisor::new(
        tree.clone(),
        config.clone(),
        spawner,
        Arc::clone(&telemetry),
    );
    Fixture {
        tmp,
        tree,
        config,
        telemetry,
        supervisor,
        uid,
    }
}

impl Fixture {
    fn write_script(&self, id: &str, script: Value) -> PathBuf {
        let path = self.tmp.path().join(format!("script-{id}.json"));
        fs::write(&path, script.to_string()).unwrap();
        path
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.tmp.path().join(format!("log-{id}.txt"))
    }

    fn scribe_parcel(&self, script: Option<&Path>) -> ParcelRef {
        let mut env = BTreeMap::new();
        if let Some(script) = script {
            env.insert(
                "FAKEAGENT_SCRIPT".to_owned(),
                script.to_string_lossy().into_owned(),
            );
        }
        ParcelRef::new(self.uid, name("scribe"), COMMISSION).with_env(env)
    }

    fn start_parcel(&self, script: &Path) -> ParcelRef {
        self.scribe_parcel(Some(script))
    }

    async fn start(&self, script: &Path, brief: &str) -> SessionId {
        self.supervisor
            .start_session(&self.start_parcel(script), brief, &Secrets::default())
            .await
            .unwrap()
    }

    fn queue_len(&self) -> usize {
        self.supervisor.queues().len(self.uid).unwrap_or(usize::MAX)
    }
}

fn status(f: &Fixture) -> SessionStatus {
    f.supervisor.session_record(f.uid).unwrap().unwrap().status
}

fn traces(telemetry: &Telemetry, uid: u32) -> Vec<TraceRecord> {
    telemetry.recent_traces(agent_key(uid), usize::MAX).unwrap()
}

fn events(telemetry: &Telemetry, uid: u32) -> Vec<String> {
    traces(telemetry, uid)
        .into_iter()
        .map(|t| t.event)
        .collect()
}

async fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timeout waiting for {what}");
}

/// The pi-dialect commands the agent received, in wire order (skips the
/// fakeagent's own `#`/env-check log lines). Every remaining line MUST be
/// a valid pi command — this is the framing-corruption check: an interrupt
/// racing a queue-drain write would surface here as a parse failure.
fn commands(log: &Path) -> Vec<hamlet_core::session::PiCommand> {
    let logged = fs::read_to_string(log).unwrap();
    logged
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| {
            parse_pi_command(line).unwrap_or_else(|e| {
                panic!("framing corrupted — unparsable stdin line: {e}: {line}")
            })
        })
        .collect()
}

/// The `id` of every received command, in order.
fn command_ids(log: &Path) -> Vec<String> {
    commands(log)
        .iter()
        .map(|command| match command {
            hamlet_core::session::PiCommand::Prompt { id, .. }
            | hamlet_core::session::PiCommand::Steer { id, .. }
            | hamlet_core::session::PiCommand::Abort { id }
            | hamlet_core::session::PiCommand::ClearQueue { id }
            | hamlet_core::session::PiCommand::GetState { id } => id.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Ordering under load
// ---------------------------------------------------------------------------

/// FIFO under load: five mails enqueued offline (before any session ran)
/// are delivered strictly in enqueue order — one prompt per task boundary
/// — with the readiness probe first and the queue file emptied at the end.
#[tokio::test(flavor = "multi_thread")]
async fn queued_mails_deliver_in_fifo_order() {
    let f = fixture(|_| {});
    let log = f.log_path("fifo");
    let script = f.write_script("fifo", json!({ "log": log }));

    let mut correlations = Vec::new();
    for i in 1..=5 {
        correlations.push(
            f.supervisor
                .enqueue(f.uid, QueuedKind::Mail, &format!("message {i}"))
                .unwrap(),
        );
    }
    // Offline enqueue: correlation carries the slot's first incarnation.
    assert_eq!(correlations[0].as_str(), "m-1-1001.0");
    assert_eq!(
        f.queue_len(),
        5,
        "queue is file-as-truth before any session"
    );
    assert!(f.tree.session_queue(f.uid).exists());

    f.start(&script, "").await;
    wait_for("queue drained", || {
        f.queue_len() == 0 && status(&f) == SessionStatus::Idle
    })
    .await;

    let ids = command_ids(&log);
    assert_eq!(ids[0], "rdy-1001.0", "probe first");
    let expected: Vec<String> = std::iter::once("rdy-1001.0".to_owned())
        .chain(correlations.iter().map(|c| c.wire_id("p")))
        .collect();
    assert_eq!(ids, expected, "strict FIFO, one prompt per boundary");

    // The queue file is removed when empty (no tree litter).
    wait_for("queue file removed", || {
        !f.tree.session_queue(f.uid).exists()
    })
    .await;
    let delivered = events(&f.telemetry, f.uid)
        .iter()
        .filter(|e| *e == "session-delivered")
        .count();
    assert_eq!(delivered, 5, "{delivered} delivery traces");

    f.supervisor.stop_session(f.uid).await.unwrap();
}

// ---------------------------------------------------------------------------
// Boundary semantics
// ---------------------------------------------------------------------------

/// Never interleave mid-task: while the agent is busy (held run), enqueued
/// mails stay in the persistent queue; each `agent_settled` boundary
/// releases exactly ONE prompt.
#[tokio::test(flavor = "multi_thread")]
async fn boundary_semantics_no_mid_task_writes() {
    let f = fixture(|_| {});
    let log = f.log_path("boundary");
    // Each run holds ~350ms before settling — deterministic windows.
    let script = f.write_script("boundary", json!({ "log": log, "settle_after_ms": 350 }));

    f.start(&script, "do the work").await;
    wait_for("busy with the brief run", || {
        status(&f) == SessionStatus::Busy
    })
    .await;

    for i in 1..=3 {
        f.supervisor
            .enqueue(f.uid, QueuedKind::Mail, &format!("mail {i}"))
            .unwrap();
    }
    // Mid-task: nothing may reach stdin beyond the in-flight brief prompt.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(status(&f), SessionStatus::Busy, "still mid-task");
    let ids = command_ids(&log);
    assert_eq!(
        ids,
        ["rdy-1001.0", "t-1-1001.0:p"],
        "no mid-task writes: {ids:?}"
    );
    assert_eq!(f.queue_len(), 3, "undelivered items persist mid-task");

    // First boundary releases exactly one envelope (seq 1 went to the
    // brief, so the mails are m-2..m-4).
    wait_for("first mail delivered", || {
        command_ids(&log).contains(&"m-2-1001.0:p".to_owned())
    })
    .await;
    assert_eq!(f.queue_len(), 2, "exactly one envelope per boundary");
    assert!(
        !command_ids(&log).contains(&"m-3-1001.0:p".to_owned()),
        "m-3 leaked mid-task"
    );

    wait_for("queue fully drained", || {
        f.queue_len() == 0 && status(&f) == SessionStatus::Idle
    })
    .await;
    let ids = command_ids(&log);
    let prompts: Vec<&String> = ids.iter().filter(|id| id.ends_with(":p")).collect();
    assert_eq!(
        prompts,
        [
            "t-1-1001.0:p",
            "m-2-1001.0:p",
            "m-3-1001.0:p",
            "m-4-1001.0:p"
        ],
        "one prompt per boundary, in order"
    );

    f.supervisor.stop_session(f.uid).await.unwrap();
}

/// An enqueue against an idle (ready, nothing in flight) session is
/// delivered immediately via the control-channel poke — no boundary event
/// needed.
#[tokio::test(flavor = "multi_thread")]
async fn enqueue_poke_delivers_to_idle_session() {
    let f = fixture(|_| {});
    let log = f.log_path("poke");
    let script = f.write_script("poke", json!({ "log": log }));
    f.start(&script, "").await;
    wait_for("running", || status(&f) == SessionStatus::Running).await;

    let correlation = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Notice, "system notice")
        .unwrap();
    wait_for("notice delivered", || f.queue_len() == 0).await;
    assert!(command_ids(&log).contains(&correlation.wire_id("p")));
    assert_eq!(correlation.as_str(), "n-1-1001.0");

    f.supervisor.stop_session(f.uid).await.unwrap();
}

// ---------------------------------------------------------------------------
// Divine Interruption
// ---------------------------------------------------------------------------

/// Interrupt mid-task: `steer` bypasses the queue and injects
/// `steer(:s) + abort(:a)` down the live pipe WHILE the agent is busy; the
/// correlation id ties the records to the interrupt; the queued mails are
/// untouched (interrupts never consume the queue) and drain afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_mid_task_with_correlation() {
    let f = fixture(|_| {});
    let log = f.log_path("interrupt");
    let script = f.write_script("interrupt", json!({ "log": log, "slow_task": true }));

    f.start(&script, "long task").await;
    wait_for("busy", || status(&f) == SessionStatus::Busy).await;
    let mail = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "queued mail")
        .unwrap();
    assert_eq!(mail.as_str(), "m-2-1001.0", "seq 1 went to the brief");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.queue_len(), 1, "mail waits for the boundary");

    let correlation = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::Human,
            InterruptPriority::Normal,
            "stop and summarize",
        )
        .await
        .unwrap();
    assert_eq!(correlation.as_str(), "i-3-1001.0");

    // Exact wire bytes of the injection (golden via the shared binding).
    let envelope = SessionEnvelope::interrupt(
        &SessionId::new(1001, 0).unwrap(),
        3,
        InterruptSource::Human,
        InterruptPriority::Normal,
        "stop and summarize",
    )
    .unwrap();
    let want: Vec<String> = pi_binding(&envelope, SessionStatus::Busy)
        .unwrap()
        .iter()
        .map(|c| c.to_json_line())
        .collect();
    // steer() returns once the writer task flushed the records into the
    // pipe — the agent's log receipt follows asynchronously.
    for record in &want {
        let record = record.clone();
        wait_for(&format!("wire record {record}"), || {
            fs::read_to_string(&log)
                .map(|logged| logged.contains(record.as_str()))
                .unwrap_or(false)
        })
        .await;
    }
    assert!(want[0].contains(r#""type":"steer""#) && want[0].contains("i-3-1001.0:s"));
    assert!(want[1].contains(r#""type":"abort""#) && want[1].contains("i-3-1001.0:a"));
    // The header makes the interrupt distinguishable inside the message
    // content too (SPEC §6 wire-schema requirement).
    assert!(want[0].contains("[hamlet-interrupt source=human priority=normal corr=i-3-1001.0]"));

    // The abort settles the run → boundary → the queued mail drains
    // (its own run is then held again by slow_task, so key on the wire id,
    // not on Idle).
    wait_for("mail delivered after the interrupt", || {
        f.queue_len() == 0 && command_ids(&log).contains(&"m-2-1001.0:p".to_owned())
    })
    .await;
    let ids = command_ids(&log);
    let steer_pos = ids.iter().position(|id| id == "i-3-1001.0:s").unwrap();
    let mail_pos = ids.iter().position(|id| id == "m-2-1001.0:p").unwrap();
    assert!(steer_pos < mail_pos, "interrupt precedes the queued mail");

    let injected = traces(&f.telemetry, f.uid)
        .into_iter()
        .find(|t| t.event == "session-interrupt-injected")
        .expect("interrupt trace");
    assert!(
        injected.detail.contains("i-3-1001.0"),
        "{}",
        injected.detail
    );
    assert!(
        injected.detail.contains("source=human"),
        "{}",
        injected.detail
    );

    f.supervisor.stop_session(f.uid).await.unwrap();
}

/// Critical interrupt mid-task: `clear_queue(:q)` precedes the steer/abort
/// pair (drops pi-side steering), while the hamlet-side queue is bypassed
/// and preserved. Framing stays intact (every logged line parses).
#[tokio::test(flavor = "multi_thread")]
async fn critical_interrupt_clears_pi_queue_only() {
    let f = fixture(|_| {});
    let log = f.log_path("critical");
    let script = f.write_script("critical", json!({ "log": log, "slow_task": true }));

    f.start(&script, "long task").await;
    wait_for("busy", || status(&f) == SessionStatus::Busy).await;
    f.supervisor
        .enqueue(f.uid, QueuedKind::Mail, "survivor")
        .unwrap();

    let correlation = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::System,
            InterruptPriority::Critical,
            "halt now",
        )
        .await
        .unwrap();
    assert_eq!(correlation.as_str(), "i-3-1001.0");

    wait_for("mail delivered after the critical interrupt", || {
        f.queue_len() == 0 && command_ids(&log).contains(&"m-2-1001.0:p".to_owned())
    })
    .await;
    let ids = command_ids(&log);
    let q = ids
        .iter()
        .position(|id| id == "i-3-1001.0:q")
        .expect("clear_queue");
    let s = ids.iter().position(|id| id == "i-3-1001.0:s").unwrap();
    let a = ids.iter().position(|id| id == "i-3-1001.0:a").unwrap();
    let m = ids.iter().position(|id| id == "m-2-1001.0:p").unwrap();
    assert!(
        q < s && s < a && a < m,
        "critical order q<s<a<mail: {ids:?}"
    );
    // Framing check: every `{`-line parsed as a pi command (commands()).
    assert!(ids.len() >= 5, "{ids:?}");

    f.supervisor.stop_session(f.uid).await.unwrap();
}

/// Interrupt against a running/idle session (nothing in flight) binds as
/// ONE header-marked prompt — and gates the queue drain until the run it
/// starts reaches its boundary.
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_at_idle_is_header_marked_prompt() {
    let f = fixture(|_| {});
    let log = f.log_path("idle-interrupt");
    let script = f.write_script(
        "idle-interrupt",
        json!({ "log": log, "settle_after_ms": 300 }),
    );

    f.start(&script, "").await;
    wait_for("running", || status(&f) == SessionStatus::Running).await;
    let correlation = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::Mayor,
            InterruptPriority::Normal,
            "prioritize the audit",
        )
        .await
        .unwrap();
    assert_eq!(correlation.as_str(), "i-1-1001.0");

    let expected = r#"{"type":"prompt","id":"i-1-1001.0:p","message":"[hamlet-interrupt source=mayor priority=normal corr=i-1-1001.0]\nprioritize the audit"}"#.to_string();
    wait_for("header-marked prompt on the wire", || {
        fs::read_to_string(&log)
            .map(|logged| logged.contains(&expected))
            .unwrap_or(false)
    })
    .await;
    wait_for("settled", || status(&f) == SessionStatus::Idle).await;

    f.supervisor.stop_session(f.uid).await.unwrap();
}

/// Typed refusals: not-live, restart backoff (not alive), and after stop.
/// Each refusal is traced; nothing reaches the pipe.
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_refusals_are_typed() {
    let f = fixture(|cfg| {
        cfg.sessions.restart.backoff_initial_ms = 60_000;
        cfg.sessions.restart.backoff_max_ms = 60_000;
    });
    // Not live at all.
    assert!(matches!(
        f.supervisor
            .steer(
                f.uid,
                InterruptSource::Human,
                InterruptPriority::Normal,
                "x"
            )
            .await,
        Err(SuperviseError::NotLive(1001))
    ));

    let log = f.log_path("refusals");
    let script = f.write_script("refusals", json!({ "log": log, "crash_after_commands": 1 }));
    f.start(&script, "").await;
    wait_for("crashed and in backoff", || {
        status(&f) == SessionStatus::Crashed && f.supervisor.is_live(f.uid)
    })
    .await;
    let err = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::Mayor,
            InterruptPriority::Critical,
            "wake",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            SuperviseError::Session(SessionError::NotAlive(SessionStatus::Crashed))
        ),
        "{err}"
    );
    assert!(events(&f.telemetry, f.uid).contains(&"session-interrupt-refused".to_owned()));

    // Empty steering text is refused at construction.
    assert!(matches!(
        f.supervisor
            .steer(
                f.uid,
                InterruptSource::Human,
                InterruptPriority::Normal,
                "  "
            )
            .await,
        Err(SuperviseError::Session(SessionError::EmptyText))
    ));

    f.supervisor.stop_session(f.uid).await.unwrap();
    assert!(matches!(
        f.supervisor
            .steer(
                f.uid,
                InterruptSource::Human,
                InterruptPriority::Normal,
                "x"
            )
            .await,
        Err(SuperviseError::NotLive(1001))
    ));
}

// ---------------------------------------------------------------------------
// Backpressure / bounce hook
// ---------------------------------------------------------------------------

/// Overflow at `sessions.queue_depth` answers the typed
/// [`QueueError::Overflow`] — the hook Sprint 04 routes back to the sender
/// as a bounce. The refused envelope never touches the queue file.
#[tokio::test(flavor = "multi_thread")]
async fn overflow_answers_typed_bounce_error() {
    let f = fixture(|cfg| cfg.sessions.queue_depth = 2);
    f.supervisor
        .enqueue(f.uid, QueuedKind::Mail, "one")
        .unwrap();
    f.supervisor
        .enqueue(f.uid, QueuedKind::Mail, "two")
        .unwrap();
    let err = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "three")
        .unwrap_err();
    match err {
        SuperviseError::Queue(QueueError::Overflow { uid, depth }) => {
            assert_eq!((uid, depth), (1001, 2));
        }
        other => panic!("expected QueueOverflow, got {other}"),
    }
    assert_eq!(f.queue_len(), 2);
    let raw = fs::read_to_string(f.tree.session_queue(f.uid)).unwrap();
    assert!(!raw.contains("three"), "refused mail must not persist");
    assert!(events(&f.telemetry, f.uid).contains(&"session-queue-overflow".to_owned()));

    // Enqueue against a missing slot is refused too.
    assert!(matches!(
        f.supervisor.enqueue(1500, QueuedKind::Mail, "x"),
        Err(SuperviseError::NoSlot(1500))
    ));
}

// ---------------------------------------------------------------------------
// Restart recovers the queue (CONVENTIONS §3 r6)
// ---------------------------------------------------------------------------

/// Daemon death with an undelivered queue: the envelopes live in the tree
/// (`courier/queues/<uid>.jsonl`), a fresh Supervisor recovers them, and
/// the restarted session delivers them — same correlations, same order.
#[tokio::test(flavor = "multi_thread")]
async fn restart_recovers_undelivered_queue() {
    let f = fixture(|_| {});
    let log_a = f.log_path("recover-a");
    let script_a = f.write_script("recover-a", json!({ "log": log_a, "slow_task": true }));

    f.start(&script_a, "held task").await;
    wait_for("busy", || status(&f) == SessionStatus::Busy).await;
    let c1 = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "survives restart")
        .unwrap();
    let c2 = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Notice, "also survives")
        .unwrap();
    assert_eq!(f.queue_len(), 2);

    // File-as-truth: the undelivered queue is on disk, canonical JSONL.
    let raw = fs::read_to_string(f.tree.session_queue(f.uid)).unwrap();
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), 2);
    for (line, correlation) in lines.iter().zip([&c1, &c2]) {
        let envelope = SessionEnvelope::from_json_line(line).unwrap();
        assert_eq!(envelope.correlation(), correlation);
    }

    // kill -9 the daemon (charter left stale-alive).
    f.supervisor.abandon();

    // Fresh daemon process against the same tree: recover() heals the
    // orphan; the queue is untouched by recovery.
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor2 = Supervisor::new(
        f.tree.clone(),
        f.config.clone(),
        spawner,
        Arc::clone(&f.telemetry),
    );
    let healed = supervisor2.recover().unwrap();
    assert_eq!(healed, vec![f.uid]);
    assert_eq!(supervisor2.queues().len(f.uid).unwrap(), 2);

    // Operator restart → new incarnation drains the recovered queue at
    // readiness (correlations are stable across the restart).
    let log_b = f.log_path("recover-b");
    let script_b = f.write_script("recover-b", json!({ "log": log_b }));
    let parcel = {
        let mut env = BTreeMap::new();
        env.insert(
            "FAKEAGENT_SCRIPT".to_owned(),
            script_b.to_string_lossy().into_owned(),
        );
        ParcelRef::new(f.uid, name("scribe"), COMMISSION).with_env(env)
    };
    let session = supervisor2
        .start_session(&parcel, "", &Secrets::default())
        .await
        .unwrap();
    assert_eq!(session.incarnation, 1);

    wait_for("recovered queue drained", || {
        supervisor2.queues().len(f.uid).unwrap_or(usize::MAX) == 0
    })
    .await;
    let ids = command_ids(&log_b);
    assert_eq!(ids[0], "rdy-1001.1", "new life probes with its own token");
    assert_eq!(
        ids[1..],
        [c1.wire_id("p"), c2.wire_id("p")],
        "recovered envelopes delivered in order: {ids:?}"
    );

    supervisor2.stop_session(f.uid).await.unwrap();
}

// ---------------------------------------------------------------------------
// Race safety
// ---------------------------------------------------------------------------

/// Interrupt racing a queue-drain write: with runs cycling fast, a burst
/// of enqueues plus repeated interrupts must never corrupt framing — every
/// stdin line the agent logged parses as one pi command, and all queued
/// mails arrive exactly once, in order.
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_during_drain_keeps_framing() {
    let f = fixture(|_| {});
    let log = f.log_path("race");
    let script = f.write_script("race", json!({ "log": log, "settle_after_ms": 60 }));

    f.start(&script, "cycle").await;
    wait_for("busy", || status(&f) == SessionStatus::Busy).await;

    let mut mails = Vec::new();
    for i in 1..=6 {
        mails.push(
            f.supervisor
                .enqueue(f.uid, QueuedKind::Mail, &format!("burst {i}"))
                .unwrap(),
        );
    }
    // Fire interrupts while boundaries/drains are cycling.
    let mut interrupts = Vec::new();
    for i in 1..=3 {
        match f
            .supervisor
            .steer(
                f.uid,
                InterruptSource::System,
                if i % 2 == 0 {
                    InterruptPriority::Critical
                } else {
                    InterruptPriority::Normal
                },
                &format!("adjust course {i}"),
            )
            .await
        {
            Ok(correlation) => interrupts.push(correlation),
            Err(SuperviseError::Draining(_)) => {} // stop raced us: fine
            Err(other) => panic!("unexpected steer failure: {other}"),
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    wait_for("burst drained", || {
        f.queue_len() == 0 && status(&f) == SessionStatus::Idle
    })
    .await;

    // Framing: commands() panics on any unparsable line.
    let ids = command_ids(&log);
    for correlation in &mails {
        let id = correlation.wire_id("p");
        assert_eq!(
            ids.iter().filter(|got| **got == id).count(),
            1,
            "mail {id} delivered exactly once: {ids:?}"
        );
    }
    let mail_positions: Vec<usize> = mails
        .iter()
        .map(|c| ids.iter().position(|id| *id == c.wire_id("p")).unwrap())
        .collect();
    let mut sorted = mail_positions.clone();
    sorted.sort_unstable();
    assert_eq!(
        mail_positions, sorted,
        "FIFO order held: {mail_positions:?}"
    );
    for correlation in &interrupts {
        assert!(
            ids.iter()
                .any(|id| id.starts_with(&format!("{}:", correlation.as_str()))),
            "interrupt {correlation} reached the wire: {ids:?}"
        );
    }

    f.supervisor.stop_session(f.uid).await.unwrap();
}
