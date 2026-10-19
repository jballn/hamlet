//! QA step 05 — mock-agent adversarial suite (SPEC §6; ADR-0015/0016/0017;
//! CONVENTIONS §3 r6, §6, §9). FakeAgent over [`HostProcessSpawner`], no
//! podman required.
//!
//! Targets (task prompt 03/05, verbatim):
//!  1. Ordering — 100 enqueued messages, FIFO, one per task boundary, no
//!     interleaving under a chatty/slow agent.
//!  2. Interrupt races — steer during drain-write (storm), steer while
//!     idle, double steer, steer during the crash-restart window; framing
//!     never corrupts, correlation ids always resolvable.
//!  3. Crash paths — crash mid-task with queued messages (restart policy,
//!     queue preserved, transitions, TITLE.md, sled traces); restart-limit
//!     exhaustion → crashed terminal, no zombie respawn loop.
//!  4. Restart recovery — daemon death mid-delivery → recover → undelivered
//!     queue replayed, an ACKED message is never delivered twice.
//!  5. Malformed streams — garbage, truncated JSON, hostile huge frames,
//!     forged responses, charter-smuggling "metric" frames: skipped/traced,
//!     never applied.
//!  6. Secrets hygiene — `.env` material never appears in FakeAgent-visible
//!     files, recorded argv, charter, or sled.
//!  7. Backpressure — overflow → typed bounce; per-life evidence caps hold;
//!     additive accounting stays lossless under a flood.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::charter::Charter;
use hamlet_core::commission::CommissionRecord;
use hamlet_core::config::{Config, Secrets};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{
    parse_pi_command, InterruptPriority, InterruptSource, QueuedKind, SessionEnvelope,
    SessionError, SessionStatus,
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
    fn write_script(&self, id: &str, script: Value) -> std::path::PathBuf {
        let path = self.tmp.path().join(format!("script-{id}.json"));
        fs::write(&path, script.to_string()).unwrap();
        path
    }

    fn log_path(&self, id: &str) -> std::path::PathBuf {
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

    async fn start(&self, script: &Path, brief: &str) {
        self.supervisor
            .start_session(
                &self.scribe_parcel(Some(script)),
                brief,
                &Secrets::default(),
            )
            .await
            .unwrap();
    }

    fn queue_len(&self) -> usize {
        self.supervisor.queues().len(self.uid).unwrap_or(usize::MAX)
    }

    fn record(&self) -> hamlet_core::session::SessionRecord {
        self.supervisor
            .session_record(self.uid)
            .unwrap()
            .expect("session record")
    }

    fn status(&self) -> SessionStatus {
        self.record().status
    }
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

fn details(telemetry: &Telemetry, uid: u32, event: &str) -> Vec<String> {
    traces(telemetry, uid)
        .into_iter()
        .filter(|t| t.event == event)
        .map(|t| t.detail)
        .collect()
}

fn summary(telemetry: &Telemetry, uid: u32) -> String {
    details(telemetry, uid, "session-stream-summary")
        .pop()
        .expect("no session-stream-summary trace")
}

/// Read one `k=v` counter out of a summary detail.
fn field(detail: &str, key: &str) -> u64 {
    detail
        .split_whitespace()
        .find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| v.parse::<u64>().ok())?
        })
        .unwrap_or_else(|| panic!("summary has no {key}: {detail}"))
}

async fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timeout waiting for {what}");
}

/// Every `{`-line the agent logged MUST parse as exactly one pi command —
/// the framing-corruption tripwire for all race tests.
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

/// How many fakeagent lives appended to this log (one marker per spawn).
fn lives(log: &Path) -> usize {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == "# fakeagent start")
        .count()
}

// ---------------------------------------------------------------------------
// 1. Ordering: 100 messages through a chatty, slow agent
// ---------------------------------------------------------------------------

/// 100 offline-enqueued mails drain STRICTLY FIFO through a chatty
/// (flooded + garbage-interleaved) and slow (held boundaries) agent — one
/// prompt per task boundary, zero interleaving, zero reordering, zero
/// duplication (SPEC §6 "one task boundary at a time — never interleaved").
#[tokio::test(flavor = "multi_thread")]
async fn fifo_100_through_a_chatty_slow_agent() {
    let f = fixture(|cfg| cfg.sessions.queue_depth = 128);
    let log = f.log_path("fifo100");
    let script = f.write_script(
        "fifo100",
        json!({
            "log": log,
            "settle_after_ms": 3,      // slow: every boundary is held
            "flood_frames": 20,        // chatty: 20 usage frames per turn
            "malformed_every_n": 7,    // hostile: garbage interleaved
        }),
    );

    let mut correlations = Vec::new();
    for i in 1..=100 {
        correlations.push(
            f.supervisor
                .enqueue(f.uid, QueuedKind::Mail, &format!("message {i}"))
                .unwrap(),
        );
    }
    assert_eq!(f.queue_len(), 100, "all 100 persisted before any life");

    f.start(&script, "").await;
    // QA05-06: wait on AGENT-SIDE evidence for the wire-log assertions —
    // `queue_len() == 0` goes true when the last envelope is POPPED
    // (before the stdin write, at-most-once) and `Idle` is stale while
    // that prompt is in flight, so the old predicate could snapshot the
    // log before the final line landed. The fake logs a line (flushed)
    // strictly BEFORE it answers it, so the last prompt's presence in the
    // log proves every earlier line is there too.
    let last_id = correlations.last().unwrap().wire_id("p");
    wait_for("100-message queue drained", || {
        f.queue_len() == 0 && ids_iter_contains(&log, &last_id)
    })
    .await;

    let ids = command_ids(&log);
    let expected: Vec<String> = std::iter::once("rdy-1001.0".to_owned())
        .chain(correlations.iter().map(|c| c.wire_id("p")))
        .collect();
    assert_eq!(
        ids, expected,
        "strict FIFO, exactly one prompt per boundary"
    );

    assert_eq!(
        events(&f.telemetry, f.uid)
            .iter()
            .filter(|e| *e == "session-delivered")
            .count(),
        100
    );
    wait_for("queue file removed", || {
        !f.tree.session_queue(f.uid).exists()
    })
    .await;

    f.supervisor.stop_session(f.uid).await.unwrap();
    wait_for("life summary", || {
        !details(&f.telemetry, f.uid, "session-stream-summary").is_empty()
    })
    .await;
    // Chatty confirmed: the stream carried far more frames than prompts,
    // including garbage — and the ordering still held.
    let s = summary(&f.telemetry, f.uid);
    assert!(field(&s, "frames") > 2_000, "{s}");
    assert!(field(&s, "malformed") >= 100, "{s}");
}

// ---------------------------------------------------------------------------
// 2. Interrupt races
// ---------------------------------------------------------------------------

/// Steer storm during drain-writes: SIX concurrent steers (mixed sources
/// and priorities) while a mail burst cycles boundaries, then a second
/// wave. Framing never corrupts; every wire id belongs to a correlation
/// this daemon allocated; every accepted interrupt is RESOLVED (acked or
/// typed-rejected in the traces) — none vanish silently.
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_storm_keeps_framing_and_resolves_correlations() {
    let f = fixture(|_| {});
    let log = f.log_path("storm");
    let script = f.write_script("storm", json!({ "log": log, "settle_after_ms": 15 }));

    f.start(&script, "cycle").await;
    // The brief run makes Running transient (ready → prompt → Busy inside
    // one drain); anything past Starting means the probe was answered.
    wait_for("ready", || f.status() != SessionStatus::Starting).await;

    let mut mails = Vec::new();
    for i in 1..=8 {
        mails.push(
            f.supervisor
                .enqueue(f.uid, QueuedKind::Mail, &format!("burst {i}"))
                .unwrap(),
        );
    }

    // Wave 1: six steers racing each other AND the drain writer.
    let wave1 = tokio::join!(
        f.supervisor.steer(
            f.uid,
            InterruptSource::Human,
            InterruptPriority::Normal,
            "adjust 0"
        ),
        f.supervisor.steer(
            f.uid,
            InterruptSource::Mayor,
            InterruptPriority::Critical,
            "adjust 1"
        ),
        f.supervisor.steer(
            f.uid,
            InterruptSource::System,
            InterruptPriority::Normal,
            "adjust 2"
        ),
        f.supervisor.steer(
            f.uid,
            InterruptSource::Human,
            InterruptPriority::Critical,
            "adjust 3"
        ),
        f.supervisor.steer(
            f.uid,
            InterruptSource::Mayor,
            InterruptPriority::Normal,
            "adjust 4"
        ),
        f.supervisor.steer(
            f.uid,
            InterruptSource::System,
            InterruptPriority::Critical,
            "adjust 5"
        ),
    );
    let mut interrupts = Vec::new();
    for outcome in [wave1.0, wave1.1, wave1.2, wave1.3, wave1.4, wave1.5] {
        match outcome {
            Ok(correlation) => interrupts.push(correlation),
            Err(SuperviseError::Session(SessionError::NotAtTaskBoundary(_)))
            | Err(SuperviseError::Draining(_)) => {
                // Typed refusal is legal mid-race; it must be traced.
            }
            Err(other) => panic!("unexpected steer failure: {other:?}"),
        }
    }
    // Wave 2: three more while the first wave's runs are still cycling.
    tokio::time::sleep(Duration::from_millis(25)).await;
    let wave2 = tokio::join!(
        f.supervisor.steer(
            f.uid,
            InterruptSource::System,
            InterruptPriority::Normal,
            "late 0"
        ),
        f.supervisor.steer(
            f.uid,
            InterruptSource::Human,
            InterruptPriority::Critical,
            "late 1"
        ),
        f.supervisor.steer(
            f.uid,
            InterruptSource::Mayor,
            InterruptPriority::Normal,
            "late 2"
        ),
    );
    for correlation in [wave2.0, wave2.1, wave2.2].into_iter().flatten() {
        interrupts.push(correlation);
    }

    // QA05-05 + QA05-06: `QueueStore::pop` persists the removal BEFORE the
    // stdin write (at-most-once, `crate::delivery`), and the daemon's
    // `session-delivered` trace fires when the WRITE returns — the agent
    // may not have logged the line yet (kernel buffer → agent reader under
    // parallel load). Wait on AGENT-SIDE evidence: the last mail's wire id
    // in the log (the fake logs a line, flushed, strictly before answering
    // it), plus every interrupt resolved in the traces (an ack proves the
    // agent processed — and therefore logged — the record).
    let deliveries = mails.len() + 1;
    let last_mail_id = mails.last().unwrap().wire_id("p");
    wait_for("storm settled", || {
        f.queue_len() == 0
            && f.status() == SessionStatus::Idle
            && details(&f.telemetry, f.uid, "session-delivered").len() >= deliveries
            && ids_iter_contains(&log, &last_mail_id)
            && interrupts.iter().all(|c| {
                traces(&f.telemetry, f.uid).iter().any(|t| {
                    t.detail.contains(c.as_str())
                        && matches!(
                            t.event.as_str(),
                            "session-interrupt-acked"
                                | "session-delivery-rejected"
                                | "session-telemetry-failed"
                                | "session-interrupt-refused"
                        )
                })
            })
    })
    .await;

    // (a) Framing: commands() panics on any unparsable stdin line.
    let ids = command_ids(&log);
    // (b) Mails: exactly once, strict FIFO.
    for correlation in &mails {
        let id = correlation.wire_id("p");
        assert_eq!(
            ids.iter().filter(|got| **got == id).count(),
            1,
            "mail {id} delivered exactly once: {ids:?}"
        );
    }
    let positions: Vec<usize> = mails
        .iter()
        .map(|c| ids.iter().position(|id| *id == c.wire_id("p")).unwrap())
        .collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(
        positions, sorted,
        "FIFO held under the storm: {positions:?}"
    );
    // (c) Every wire id resolves to an allocated correlation (no daemon-
    //     invented or forged ids on our own pipe).
    for id in &ids {
        let known = id == "rdy-1001.0"
            || id == "t-1-1001.0:p" // the register brief (tasking seq 1)
            || mails.iter().any(|c| c.wire_id("p") == *id)
            || interrupts
                .iter()
                .any(|c| ["q", "s", "a", "p"].iter().any(|sfx| c.wire_id(sfx) == *id));
        assert!(known, "wire id {id} belongs to no allocated correlation");
    }
    // (d) Interrupt resolvability is part of the settle predicate above:
    //     every accepted interrupt has an ack/rejection trace by the time
    //     the log snapshot was taken (a timed-out wait fails the test).

    f.supervisor.stop_session(f.uid).await.unwrap();
}

/// Double steer: two steers back-to-back against a BUSY session inject in
/// submission order (single-writer funnel: s1,a1 BEFORE s2,a2 — no
/// interleaved half-records), both ack; then a double steer at IDLE binds
/// as header-marked prompts, and a mid-stream rejection (pi's own guard)
/// is typed and traced, never a framing corruption.
#[tokio::test(flavor = "multi_thread")]
async fn double_steer_busy_then_idle() {
    let f = fixture(|_| {});
    let log = f.log_path("double");
    let script = f.write_script("double", json!({ "log": log, "settle_after_ms": 500 }));

    f.start(&script, "long task").await;
    wait_for("busy", || f.status() == SessionStatus::Busy).await;

    let c1 = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::Human,
            InterruptPriority::Normal,
            "first",
        )
        .await
        .unwrap();
    let c2 = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::Mayor,
            InterruptPriority::Normal,
            "second",
        )
        .await
        .unwrap();

    // steer1's abort may settle the run before steer2 is injected — then
    // steer2 legitimately binds as a header-marked prompt (:p) instead of
    // steer+abort (:s,:a). Either binding is a valid double-steer outcome;
    // what must hold is: no interleaved half-records, and injection order.
    // QA05-05: `settle_after_ms` can end the run before EITHER injection is
    // processed, so c1's binding is status-dependent exactly like c2's —
    // waiting only on c2 let the assertions below look up c1's `:s`/`:a`
    // records when c1 had legitimately bound as a header-marked `:p`. Wait
    // for both, then branch on the binding each actually got. The invariants
    // are unchanged: each injection's own records stay ordered, and injection
    // 1 completes before injection 2 starts.
    //
    // And "on the wire" must mean the WHOLE binding: `inject_interrupt`
    // writes `:s` and `:a` as two flushed records, so the fake can log `:s`
    // a moment before `:a` arrives. Waiting on the first record only lets
    // the ordering assertions below look up an `:a` that has not landed yet.
    // (Not a framing defect — the single-writer funnel still guarantees no
    // other envelope's records interleave between them.)
    let binding_landed = |steer_id: &str, prompt_id: &str, abort_id: &str| {
        let ids = command_ids(&log);
        if ids.iter().any(|got| got == steer_id) {
            ids.iter().any(|got| got == abort_id)
        } else {
            ids.iter().any(|got| got == prompt_id)
        }
    };
    let (c1s, c1a, c1p) = (c1.wire_id("s"), c1.wire_id("a"), c1.wire_id("p"));
    let (c2s, c2a, c2p) = (c2.wire_id("s"), c2.wire_id("a"), c2.wire_id("p"));
    wait_for("both steers fully on the wire", || {
        binding_landed(&c1s, &c1p, &c1a) && binding_landed(&c2s, &c2p, &c2a)
    })
    .await;
    let ids = command_ids(&log);
    let pos = |id: &str| ids.iter().position(|got| got == id).unwrap();
    let c1_last = if ids.contains(&c1.wire_id("s")) {
        assert!(
            pos(&c1.wire_id("s")) < pos(&c1.wire_id("a")),
            "steer1 precedes its own abort: {ids:?}"
        );
        pos(&c1.wire_id("a"))
    } else {
        pos(&c1.wire_id("p"))
    };
    let c2_first = if ids.contains(&c2.wire_id("s")) {
        assert!(
            c1_last < pos(&c2.wire_id("s")),
            "injection 1 completes before injection 2 starts: {ids:?}"
        );
        assert!(pos(&c2.wire_id("s")) < pos(&c2.wire_id("a")), "{ids:?}");
        pos(&c2.wire_id("s"))
    } else {
        assert!(
            c1_last < pos(&c2.wire_id("p")),
            "injection 1 completes before injection 2 starts: {ids:?}"
        );
        pos(&c2.wire_id("p"))
    };
    let _ = c2_first;

    wait_for("session settles", || f.status() == SessionStatus::Idle).await;
    wait_for("both interrupts acked", || {
        let acked = details(&f.telemetry, f.uid, "session-interrupt-acked");
        acked.iter().any(|d| d.contains(c1.as_str()))
            && acked.iter().any(|d| d.contains(c2.as_str()))
    })
    .await;

    // Double steer at idle: the first binds as a header-marked prompt and
    // starts a run; the second races the agent_start parse — it is either
    // another prompt (pi may reject it mid-stream: typed + traced) or a
    // steer+abort pair. All outcomes keep framing intact.
    // Double steer "at idle": between the steer() call and the life task
    // processing it, the previous run's agent_start may flip the parsed
    // status — so c3 may bind as a header-marked prompt (:p, idle) OR as
    // steer+abort (:s/:a, busy). Both are legal; the invariant is that the
    // binding is decided once, by the single writer, and framing survives.
    let c3 = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::System,
            InterruptPriority::Normal,
            "idle one",
        )
        .await
        .unwrap();
    let c4 = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::System,
            InterruptPriority::Normal,
            "idle two",
        )
        .await
        .unwrap();
    wait_for("c3 on the wire (either binding)", || {
        ids_iter_contains(&log, &c3.wire_id("p")) || ids_iter_contains(&log, &c3.wire_id("s"))
    })
    .await;
    wait_for("idle-two resolved", || {
        let ids = command_ids(&log);
        ids.contains(&c4.wire_id("p")) || ids.contains(&c4.wire_id("s"))
    })
    .await;
    wait_for("settles again", || f.status() == SessionStatus::Idle).await;
    // Whatever the binding, every stdin line still parses (commands()).
    let _ = command_ids(&log);

    f.supervisor.stop_session(f.uid).await.unwrap();
}

fn ids_iter_contains(log: &Path, id: &str) -> bool {
    command_ids(log).iter().any(|got| got == id)
}

/// Steer during the crash-restart window: the session never reaches
/// readiness (every life dies on the probe), so every steer must answer a
/// TYPED refusal — `NotAtTaskBoundary(Starting)` inside a life,
/// `NotAlive(Crashed)` inside a backoff window — and the refusal storm
/// must NOT cancel the restart chain (only stop/kill may): the budget
/// still exhausts to the permanent-crashed terminal state.
#[tokio::test(flavor = "multi_thread")]
async fn steer_during_crash_restart_window_is_typed_and_never_cancels() {
    let f = fixture(|cfg| {
        cfg.sessions.restart.max_restarts = 2;
        cfg.sessions.restart.backoff_initial_ms = 200;
        cfg.sessions.restart.backoff_max_ms = 400;
    });
    let log = f.log_path("window");
    let script = f.write_script("window", json!({ "log": log, "crash_after_commands": 1 }));
    f.start(&script, "").await;

    let mut saw_crashed_refusal = false;
    let mut saw_boundary_refusal = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if !f.supervisor.is_live(f.uid)
            && events(&f.telemetry, f.uid).contains(&"session-permanently-crashed".to_owned())
        {
            break;
        }
        assert!(Instant::now() < deadline, "restart chain never terminated");
        match f
            .supervisor
            .steer(
                f.uid,
                InterruptSource::Human,
                InterruptPriority::Normal,
                "wake up",
            )
            .await
        {
            Ok(c) => panic!("steer succeeded against a never-ready crashing session: {c}"),
            Err(SuperviseError::NotLive(_)) => {}
            Err(SuperviseError::Session(SessionError::NotAlive(SessionStatus::Crashed))) => {
                saw_crashed_refusal = true;
            }
            Err(SuperviseError::Session(SessionError::NotAtTaskBoundary(_))) => {
                saw_boundary_refusal = true;
            }
            Err(other) => panic!("unexpected steer error in crash window: {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    assert!(
        saw_crashed_refusal,
        "never hit a backoff window (crashed refusal) — timing bug in the test or the chain"
    );
    let rec = f.record();
    assert_eq!(rec.status, SessionStatus::Crashed, "terminal state");
    assert_eq!(rec.restart_count, 2);
    assert_eq!(rec.id.incarnation, 2, "lives: inc 0, 1, 2");
    assert_eq!(lives(&log), 3, "refusals never cancelled a restart");
    assert!(events(&f.telemetry, f.uid).contains(&"session-interrupt-refused".to_owned()));
    let title = fs::read_to_string(f.tree.parcel_title(f.uid)).unwrap();
    assert!(
        title.contains("session 1001.2 permanently crashed after 2 auto-restart(s)"),
        "lineage stamp missing: {title}"
    );
    // Terminal: the dead session refuses steers with NotLive, forever.
    tokio::time::sleep(Duration::from_millis(300)).await;
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
    // `NotAtTaskBoundary(Starting)` refusals are timing-dependent (the
    // starting windows are milliseconds short); recorded, not required.
    let _ = saw_boundary_refusal;
}

// ---------------------------------------------------------------------------
// 3. Crash paths
// ---------------------------------------------------------------------------

/// Agent crash mid-task with queued messages: life 0 dies mid-run (frame
/// cap), the auto-restart (policy honored: backoff, new incarnation,
/// `--continue` per config default) survives and drains the preserved
/// queue in FIFO with STABLE correlations; the envelope that was popped
/// and written before the crash is at-most-once — delivered to life 0's
/// stdin exactly once and NEVER replayed to life 1. Charter transitions
/// and sled traces recorded throughout; TITLE.md carries no permanent stamp.
#[tokio::test(flavor = "multi_thread")]
async fn crash_mid_task_preserves_queue_and_auto_restart_recovers() {
    let f = fixture(|cfg| {
        cfg.sessions.restart.max_restarts = 3;
        cfg.sessions.restart.backoff_initial_ms = 50;
        cfg.sessions.restart.backoff_max_ms = 100;
    });
    let log = f.log_path("midtask");
    // Life 0 (no `--continue` in argv) dies 4 frames into the first run —
    // mid-task, with mail still queued. Life 1 gets `--continue`
    // (continue_on_restart default) → crash rule disarmed → survives.
    let script = f.write_script(
        "midtask",
        json!({
            "log": log,
            "crash_after_frames": 4,
            "crash_if_argv_lacks": "--continue",
        }),
    );

    let m1 = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "in flight when it dies")
        .unwrap();
    let m2 = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "survivor one")
        .unwrap();
    let m3 = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "survivor two")
        .unwrap();
    f.start(&script, "the brief").await;
    let t4 = hamlet_core::session::CorrelationId::parse("t-4-1001.0").unwrap();

    // QA05-05/06: same pop→write window as the storm test, and the
    // daemon's `session-delivered` trace still precedes the agent logging
    // the line — wait on AGENT-SIDE evidence: the last envelope's (t4)
    // wire id in the log, plus an empty queue.
    let t4_id = t4.wire_id("p");
    wait_for("restarted life drains the queue", || {
        f.queue_len() == 0 && ids_iter_contains(&log, &t4_id)
    })
    .await;

    let ids = command_ids(&log);
    assert_eq!(lives(&log), 2, "exactly one auto-restart");
    assert_eq!(ids[0], "rdy-1001.0");
    assert_eq!(ids[1], m1.wire_id("p"), "m1 went out on life 0's pipe");
    assert_eq!(
        ids.iter().filter(|id| **id == m1.wire_id("p")).count(),
        1,
        "at-most-once: the popped envelope is never replayed: {ids:?}"
    );
    assert_eq!(
        details(&f.telemetry, f.uid, "session-delivered").len(),
        4,
        "m1 on life 0, then m2/m3/t4 on life 1"
    );
    let tail_start = ids
        .iter()
        .position(|id| id == "rdy-1001.1")
        .expect("life 1 re-probes with its own token");
    assert_eq!(
        ids[tail_start..],
        [
            "rdy-1001.1".to_owned(),
            m2.wire_id("p"),
            m3.wire_id("p"),
            t4.wire_id("p")
        ],
        "preserved queue drains FIFO with stable correlations: {ids:?}"
    );

    // Charter state after the chain: healed, budget reset at the boundary.
    //
    // QA05-05: the *last* delivery (t4) starts a run of its own, so the
    // status read here is legitimately `idle` (that run settled) or `busy`
    // (still in flight) — asserting a bare `Idle` raced the final
    // agent_start. What the chain must actually guarantee is the healed
    // identity plus the budget reset, and `restart_count` is only ever
    // written back to 0 *at* a boundary, so waiting on it is the evidence
    // that a settle was reached.
    wait_for("healed record with the restart budget reset", || {
        let rec = f.record();
        rec.id.incarnation == 1 && rec.restart_count == 0
    })
    .await;
    let rec = f.record();
    assert_eq!(rec.id.incarnation, 1);
    assert!(
        matches!(rec.status, SessionStatus::Idle | SessionStatus::Busy),
        "settled or mid-final-run — never crashed/starting: {:?}",
        rec.status
    );
    assert_eq!(rec.restart_count, 0, "reset on first agent_settled");

    // Sled traces: crash classified, restart spawned, no permanent stamp.
    let ev = events(&f.telemetry, f.uid);
    assert_eq!(
        ev.iter().filter(|e| *e == "session-crashed").count(),
        1,
        "{ev:?}"
    );
    assert!(ev.contains(&"session-spawned".to_owned()), "{ev:?}");
    assert!(
        !ev.contains(&"session-permanently-crashed".to_owned()),
        "{ev:?}"
    );
    assert!(
        details(&f.telemetry, f.uid, "session-spawned")
            .iter()
            .any(|d| d.contains("restarts=1")),
        "restart lineage in traces"
    );
    let title = fs::read_to_string(f.tree.parcel_title(f.uid)).unwrap();
    assert!(
        !title.contains("permanently crashed"),
        "a RECOVERED crash must not stamp permanent lineage: {title}"
    );

    f.supervisor.stop_session(f.uid).await.unwrap();
}

/// Restart-limit exhaustion is TERMINAL: crashed state, budget spent,
/// lineage stamped — and then NOTHING respawns. A 700ms observation window
/// (≫ the backoff schedule) proves no zombie loop: spawn count, charter
/// record and liveness are all frozen. Offline enqueue still persists (the
/// queue outlives the session); steer stays a typed `NotLive`.
#[tokio::test(flavor = "multi_thread")]
async fn restart_exhaustion_is_terminal_with_no_zombie_respawn() {
    let f = fixture(|cfg| {
        cfg.sessions.restart.max_restarts = 2;
        cfg.sessions.restart.backoff_initial_ms = 20;
        cfg.sessions.restart.backoff_max_ms = 40;
    });
    let log = f.log_path("zombie");
    let script = f.write_script(
        "zombie",
        json!({ "log": log, "crash_after_commands": 1, "crash_exit_code": 70 }),
    );
    f.start(&script, "").await;

    wait_for("permanent crash", || {
        f.status() == SessionStatus::Crashed && !f.supervisor.is_live(f.uid)
    })
    .await;
    let rec = f.record();
    let spawn_count = lives(&log);
    assert_eq!(spawn_count, 3, "initial life + 2 restarts, no more");

    // Observation window: a zombie loop would respawn inside one backoff.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(lives(&log), spawn_count, "ZOMBIE RESPAWN LOOP");
    assert_eq!(
        f.record(),
        rec,
        "charter record mutated after terminal state"
    );
    assert!(!f.supervisor.is_live(f.uid), "supervision task resurrected");
    assert_eq!(f.status(), SessionStatus::Crashed);

    let title = fs::read_to_string(f.tree.parcel_title(f.uid)).unwrap();
    assert!(
        title.contains("session 1001.2 permanently crashed after 2 auto-restart(s)"),
        "{title}"
    );
    assert!(title.contains("exit code 70"), "{title}");

    // Post-mortem API surface: steer typed-refuses; mail still persists.
    assert!(matches!(
        f.supervisor
            .steer(
                f.uid,
                InterruptSource::Mayor,
                InterruptPriority::Critical,
                "wake"
            )
            .await,
        Err(SuperviseError::NotLive(1001))
    ));
    f.supervisor
        .enqueue(f.uid, QueuedKind::Mail, "for the next life")
        .unwrap();
    assert_eq!(f.queue_len(), 1, "queue outlives the crashed session");
}

// ---------------------------------------------------------------------------
// 4. Restart recovery: daemon death mid-delivery
// ---------------------------------------------------------------------------

/// kill -9 the daemon MID-DELIVERY (first mail already written AND acked
/// by the agent): after `recover()` + operator restart, the undelivered
/// tail is replayed in order with stable correlations — and the ACKED
/// message is never delivered a second time (at-most-once, SPEC §6 /
/// `crate::delivery` persistence discipline).
#[tokio::test(flavor = "multi_thread")]
async fn daemon_death_mid_delivery_never_replays_an_acked_message() {
    let f = fixture(|_| {});
    let log_a = f.log_path("mid-death-a");
    let script_a = f.write_script(
        "mid-death-a",
        json!({ "log": log_a, "settle_after_ms": 150 }),
    );

    f.start(&script_a, "").await;
    wait_for("running", || f.status() == SessionStatus::Running).await;
    let m1 = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "will be acked before the kill")
        .unwrap();
    let m2 = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "survives in the file")
        .unwrap();
    let m3 = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Notice, "also survives")
        .unwrap();

    // Wait until m1 is not merely written but ACKED (response parsed,
    // delivery-acked traced), then kill -9 the daemon.
    wait_for("m1 delivery acked", || {
        details(&f.telemetry, f.uid, "session-delivery-acked")
            .iter()
            .any(|d| d.contains(m1.as_str()))
    })
    .await;
    f.supervisor.abandon();

    // The undelivered tail is file-as-truth on disk, mid-crash.
    let raw = fs::read_to_string(f.tree.session_queue(f.uid)).unwrap();
    let persisted: Vec<_> = raw
        .lines()
        .map(|line| SessionEnvelope::from_json_line(line).unwrap())
        .collect();
    let persisted_corrs: Vec<_> = persisted.iter().map(|e| e.correlation().clone()).collect();
    assert!(
        !persisted_corrs.iter().any(|c| c == &m1),
        "the acked envelope must be GONE from the queue file: {raw}"
    );
    assert!(
        persisted_corrs.contains(&m2) || persisted_corrs.contains(&m3),
        "undelivered tail lost: {raw}"
    );

    // Fresh daemon: recover heals the orphan, restart drains the tail.
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor2 = Supervisor::new(
        f.tree.clone(),
        f.config.clone(),
        spawner,
        Arc::clone(&f.telemetry),
    );
    assert_eq!(supervisor2.recover().unwrap(), vec![f.uid]);
    assert_eq!(
        supervisor2.queues().len(f.uid).unwrap(),
        persisted_corrs.len()
    );

    let log_b = f.log_path("mid-death-b");
    let script_b = f.write_script("mid-death-b", json!({ "log": log_b }));
    let parcel = {
        let mut env = BTreeMap::new();
        env.insert(
            "FAKEAGENT_SCRIPT".to_owned(),
            script_b.to_string_lossy().into_owned(),
        );
        ParcelRef::new(f.uid, name("scribe"), COMMISSION).with_env(env)
    };
    supervisor2
        .start_session(&parcel, "", &Secrets::default())
        .await
        .unwrap();

    // QA05-06: agent-side evidence again — the queue empties at the POP,
    // before the last line reaches the agent's log.
    let last_replayed = persisted_corrs.last().map(|c| c.wire_id("p"));
    wait_for("tail drained", || {
        supervisor2.queues().len(f.uid).unwrap_or(usize::MAX) == 0
            && last_replayed
                .as_ref()
                .is_none_or(|id| ids_iter_contains(&log_b, id))
    })
    .await;
    let ids_b = command_ids(&log_b);
    let expected: Vec<String> = std::iter::once("rdy-1001.1".to_owned())
        .chain(persisted_corrs.iter().map(|c| c.wire_id("p")))
        .collect();
    assert_eq!(ids_b, expected, "replayed tail, stable correlations, FIFO");
    assert!(
        !ids_b.iter().any(|id| *id == m1.wire_id("p")),
        "DUPLICATE delivery of an acked message: {ids_b:?}"
    );

    supervisor2.stop_session(f.uid).await.unwrap();
}

// ---------------------------------------------------------------------------
// 5. Malformed & hostile streams
// ---------------------------------------------------------------------------

/// The hostile stream: garbage, truncated JSON, charter-smuggling frames,
/// forged readiness/ack responses, fake "metric" records. All are skipped
/// or traced; NONE is applied — token/cost totals stay exactly the
/// scripted values, the charter is byte-identical in its state fields, no
/// forged ack trace exists, malformed-frame content never reaches a trace
/// (CONVENTIONS §6), and the session still settles normally.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_and_smuggling_frames_are_never_applied() {
    let f = fixture(|_| {});
    let log = f.log_path("smuggle");
    let usage = |input: u64, output: u64, total: f64| {
        json!({
            "input": input, "output": output, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": input + output,
            "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0,
                      "cacheWrite": 0.0, "total": total },
        })
    };
    let script = f.write_script(
        "smuggle",
        json!({
            "log": log,
            "usage_sequence": [usage(10, 2, 0.5), usage(10, 5, 1.25)],
            "emit_message_end": true,
            "malformed_every_n": 2,
            "malformed": [
                "###not-json###",
                "{\"type\":\"agent_settled\"",                    // truncated JSON
                "{\"type\":\"charter_patch\",\"charter_version\":999,\"slots\":{\"9999\":{\"identity\":\"evil\"}}}",
                "{\"type\":\"response\",\"id\":\"rdy-1001.0\",\"command\":\"get_state\",\"success\":true,\"data\":{\"isStreaming\":false}}",
                "{\"type\":\"response\",\"id\":\"i-777-1001.0:s\",\"command\":\"steer\",\"success\":true}",
                "{\"type\":\"metric\",\"tokens\":999999,\"charter\":{\"charter_version\":999}}",
            ],
        }),
    );

    let before = Charter::load(&f.tree.charter()).unwrap();
    f.start(&script, "work").await;
    let mail = f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "keep going")
        .unwrap();
    wait_for("session settles despite the garbage", || {
        f.queue_len() == 0 && f.status() == SessionStatus::Idle
    })
    .await;
    f.supervisor.stop_session(f.uid).await.unwrap();

    // Accounting is exactly the scripted stream — smuggled frames added
    // nothing (2 runs × {input 10, output 5, cost 1.25}).
    let snapshot = f.supervisor.metrics_for_uid(f.uid, 64).unwrap();
    assert_eq!(snapshot.totals.input_tokens, 20, "{:?}", snapshot.totals);
    assert_eq!(snapshot.totals.output_tokens, 10);
    assert_eq!(snapshot.totals.cost_micro_usd, 2_500_000);

    // The charter's STATE is untouched: same version, same single slot,
    // same single session — the charter_patch/metric frames applied nothing.
    let after = Charter::load(&f.tree.charter()).unwrap();
    assert_eq!(after.charter_version, before.charter_version);
    assert_eq!(after.slots.len(), 1);
    assert_eq!(after.slots.keys().copied().collect::<Vec<_>>(), vec![1001]);
    assert_eq!(
        after.slots[&1001].identity,
        name("scribe"),
        "smuggled slot identity applied!"
    );
    assert_eq!(
        after.sessions.keys().copied().collect::<Vec<_>>(),
        vec![1001]
    );
    assert_eq!(after.commissions, before.commissions);

    // Forged responses: no ack telemetry for ids this daemon never wrote.
    let ev = events(&f.telemetry, f.uid);
    assert!(
        !ev.contains(&"session-interrupt-acked".to_owned()),
        "{ev:?}"
    );
    for detail in details(&f.telemetry, f.uid, "session-delivery-acked") {
        assert!(!detail.contains("i-777"), "{detail}");
        assert!(
            detail.contains(mail.as_str()) || detail.contains("t-1-1001.0"),
            "unexpected ack: {detail}"
        );
    }

    // Drift signals present: the unmodelled record types are traced BY NAME.
    let unknown = details(&f.telemetry, f.uid, "session-stream-unknown-record");
    assert!(
        unknown.iter().any(|d| d.contains("charter_patch")),
        "{unknown:?}"
    );
    assert!(unknown.iter().any(|d| d.contains("metric")), "{unknown:?}");
    assert!(
        ev.contains(&"session-stream-malformed".to_owned()),
        "{ev:?}"
    );

    // No frame CONTENT ever reaches any trace (CONVENTIONS §6).
    for trace in traces(&f.telemetry, f.uid) {
        for forbidden in [
            "evil",
            "999999",
            "not-json",
            "i-777",
            "charter_version\":999",
        ] {
            assert!(
                !trace.detail.contains(forbidden),
                "frame content leaked into {}: {}",
                trace.event,
                trace.detail
            );
        }
    }
}

/// Hostile huge frames (4 MiB of garbage, 2 MiB of truncated JSON) are
/// survived: skipped, counted, never applied — the session still settles,
/// the totals stay exact, and no crash/restart is triggered. NOTE: this
/// test documents survival, not boundedness — see HANDOFF finding QA05-01
/// (the reader allocates the whole frame before any size guard exists).
#[tokio::test(flavor = "multi_thread")]
async fn hostile_huge_frames_are_skipped_not_applied() {
    let f = fixture(|_| {});
    let log = f.log_path("huge");
    let huge_garbage = "A".repeat(4 * 1024 * 1024);
    let huge_truncated = format!(
        "{{\"type\":\"message_update\",\"usage\":{{\"input\":{}",
        "9".repeat(2 * 1024 * 1024)
    );
    let script = f.write_script(
        "huge",
        json!({
            "log": log,
            "flood_frames": 24,
            "malformed_every_n": 6,
            "malformed": [huge_garbage, huge_truncated],
        }),
    );

    f.start(&script, "survive this").await;
    wait_for("settles", || f.status() == SessionStatus::Idle).await;

    let ev = events(&f.telemetry, f.uid);
    assert!(!ev.contains(&"session-crashed".to_owned()), "{ev:?}");
    assert_eq!(lives(&log), 1, "huge frames must not trigger restarts");
    let snapshot = f.supervisor.metrics_for_uid(f.uid, 64).unwrap();
    assert_eq!(snapshot.totals.input_tokens, 24, "exact despite the flood");
    assert_eq!(snapshot.totals.output_tokens, 24);
    f.supervisor.stop_session(f.uid).await.unwrap();
    wait_for("life summary", || {
        !details(&f.telemetry, f.uid, "session-stream-summary").is_empty()
    })
    .await;
    let s = summary(&f.telemetry, f.uid);
    assert!(field(&s, "malformed") >= 4, "{s}");
    // No trace carries multi-megabyte content (details are capped/scrubbed).
    for trace in traces(&f.telemetry, f.uid) {
        assert!(
            trace.detail.len() < 512,
            "{} detail is not bounded: {} bytes",
            trace.event,
            trace.detail.len()
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Secrets hygiene
// ---------------------------------------------------------------------------

/// `.env` material NEVER lands where the agent (or an archive) can read
/// it: after a full live session with all four secret channels populated,
/// no file in the tempdir — tree, charter, queue, TITLE.md, sled database
/// bytes, the FakeAgent's own log — contains a secret value, and no trace
/// detail does either. The exec env is the only channel (SPEC §11.4,
/// CONVENTIONS §6).
#[tokio::test(flavor = "multi_thread")]
async fn secrets_never_reach_files_charter_or_sled() {
    let f = fixture(|_| {});
    let log = f.log_path("secrets");
    let script = f.write_script("secrets", json!({ "log": log }));
    let secrets = Secrets::from_vars([
        ("HAMLET_API_KEY_ANTHROPIC", "test-key-do-not-use"),
        ("HAMLET_API_KEY_OPENAI", "sk-fake-second-key-do-not-use"),
        ("HAMLET_MODEL_API_KEY", "test-key-do-not-use-2"),
        ("HAMLET_MODEL_ENDPOINT", "https://models.example/v1"),
    ]);
    let forbidden = [
        "test-key-do-not-use",
        "sk-fake-second-key-do-not-use",
        "test-key-do-not-use-2",
        "https://models.example/v1",
    ];

    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), "work", &secrets)
        .await
        .unwrap();
    f.supervisor
        .enqueue(f.uid, QueuedKind::Mail, "and more work")
        .unwrap();
    wait_for("drained", || {
        f.queue_len() == 0 && f.status() == SessionStatus::Idle
    })
    .await;
    f.supervisor.stop_session(f.uid).await.unwrap();

    // Every trace detail this session produced.
    for trace in traces(&f.telemetry, f.uid) {
        for secret in forbidden {
            assert!(
                !trace.detail.contains(secret),
                "secret leaked into trace {}: {}",
                trace.event,
                trace.detail
            );
        }
        assert!(
            !trace.detail.contains("API_KEY"),
            "argv/env shape leaked into trace {}: {}",
            trace.event,
            trace.detail
        );
    }

    // Every file under the tempdir: the .hamlet tree (charter, queues,
    // TITLE.md, parcels), the .hamlet-data sled database BYTES, the
    // FakeAgent's log, and the script itself.
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(f.tmp.path(), &mut files);
    assert!(files.len() >= 5, "sanity: {}", files.len());
    for path in &files {
        let bytes = fs::read(path).unwrap();
        for secret in &forbidden {
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "secret {secret:?} found in {}",
                path.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Backpressure: typed bounce + evidence caps under a flood
// ---------------------------------------------------------------------------

/// Overflow answers the typed bounce at the configured depth and the
/// refused envelope never touches the file; then a flooded, garbage-
/// interleaved, unknown-type-polluted stream proves the per-life evidence
/// caps hold (≤8 malformed traces + a suppression rollup, ≤32 unknown
/// types) while the ADDITIVE accounting stays lossless (`dropped=0`, exact
/// tokens) — no unbounded memory growth on any tracked collection
/// (ADR-0017 cl. 2–3).
#[tokio::test(flavor = "multi_thread")]
async fn overflow_bounces_and_evidence_caps_hold_under_flood() {
    let f = fixture(|cfg| cfg.sessions.queue_depth = 3);

    // Typed bounce at the bound.
    for text in ["one", "two", "three"] {
        f.supervisor.enqueue(f.uid, QueuedKind::Mail, text).unwrap();
    }
    match f
        .supervisor
        .enqueue(f.uid, QueuedKind::Mail, "fourth-refused")
    {
        Err(SuperviseError::Queue(QueueError::Overflow { uid, depth })) => {
            assert_eq!((uid, depth), (1001, 3));
        }
        other => panic!("expected typed overflow bounce, got {other:?}"),
    }
    let raw = fs::read_to_string(f.tree.session_queue(f.uid)).unwrap();
    assert!(!raw.contains("fourth-refused"), "refused item persisted");
    assert_eq!(raw.lines().count(), 3);
    assert!(events(&f.telemetry, f.uid).contains(&"session-queue-overflow".to_owned()));

    // Flood the stream while the queue drains.
    let log = f.log_path("flood");
    let unknown: Vec<String> = (0..40).map(|i| format!("hostile_record_{i}")).collect();
    let script = f.write_script(
        "flood",
        json!({
            "log": log,
            "flood_frames": 1500,
            "malformed_every_n": 50,
            "unknown_records": unknown,
        }),
    );
    f.start(&script, "").await;
    // Deterministic settle: tokens hit 4500 only when the LAST flood frame
    // of the third run was parsed, and Idle afterwards means its
    // agent_settled was parsed too — every frame of every run is in the
    // daemon's hands before the stop (stop-mid-run is the QA05-02 repro).
    wait_for("flood fully parsed", || {
        f.queue_len() == 0
            && f.supervisor
                .metrics_for_uid(f.uid, 1)
                .is_ok_and(|s| s.totals.input_tokens >= 4_500)
            && f.status() == SessionStatus::Idle
    })
    .await;
    f.supervisor.stop_session(f.uid).await.unwrap();
    wait_for("life summary written", || {
        !details(&f.telemetry, f.uid, "session-stream-summary").is_empty()
    })
    .await;

    let s = summary(&f.telemetry, f.uid);
    // 3 runs (mails) + the life's frames: 40 unknown records per turn × 3.
    assert_eq!(field(&s, "unknown"), 120, "{s}");
    assert!(field(&s, "malformed") >= 80, "{s}");
    assert_eq!(
        field(&s, "dropped"),
        0,
        "additive accounting must be lossless: {s}"
    );

    // Evidence caps: ≤8 malformed traces + the suppression rollup, and
    // ≤32 distinct unknown types traced (the rest counted in the summary).
    let malformed_traces = details(&f.telemetry, f.uid, "session-stream-malformed");
    assert!(
        malformed_traces.len() <= 8,
        "malformed trace cap breached: {}",
        malformed_traces.len()
    );
    assert!(
        !details(&f.telemetry, f.uid, "session-stream-malformed-suppressed").is_empty(),
        "suppressed-count rollup missing: {s}"
    );
    let unknown_traces = details(&f.telemetry, f.uid, "session-stream-unknown-record");
    assert_eq!(
        unknown_traces.len(),
        32,
        "unknown-type cap must trace exactly the first 32: {unknown_traces:?}"
    );

    // Lossless despite the flood: exact totals (3 turns × cumulative 1500).
    let snapshot = f.supervisor.metrics_for_uid(f.uid, 256).unwrap();
    assert_eq!(snapshot.totals.input_tokens, 4_500, "{:?}", snapshot.totals);
    assert_eq!(snapshot.totals.output_tokens, 4_500);
}

/// QA05-02 REGRESSION (fixed — see HANDOFF sprint 03, finding QA05-02): a
/// graceful stop that lands while the agent is mid-run races the reaper.
/// `tokio::select!` in `run_life_inner` may take the `exit` arm while stdout
/// frames are still buffered in the pipe/`BufReader`; `Step::Exit` used to
/// return WITHOUT draining them, silently dropping the telemetry tail
/// (undercounted tokens/unknowns in the very summary that claims exact
/// accounting). Measured before the fix: 6/15 runs lost the tail.
///
/// The fix is `drain_tail`: after the process exits, stdout is read to EOF
/// (bounded by one stop-grace window, traced if it exceeds it) folding every
/// frame through telemetry and the pure status reducer — no delivery, since
/// stdin is gone and a pop-then-failed-write would lose the envelope.
/// Invariant asserted: every frame the agent emitted before exit is folded
/// (tokens_in == 3000, the exact scripted total — ADR-0017 cl. 2).
#[tokio::test(flavor = "multi_thread")]
async fn repro_qa05_02_stop_midrun_loses_telemetry_tail() {
    let f = fixture(|_| {});
    f.supervisor
        .enqueue(f.uid, QueuedKind::Mail, "one long run")
        .unwrap();
    let log = f.log_path("tail-race");
    let script = f.write_script(
        "tail-race",
        json!({ "log": log, "flood_frames": 3000, "malformed_every_n": 50 }),
    );
    f.start(&script, "").await;
    // Land the stop INSIDE the run's ~300ms emission window (fast poll —
    // the 20ms helper can step over it). The agent keeps flushing its
    // remaining frames, sees the EOF the stop caused, and exits with the
    // tail still in the pipe/BufReader — the select race decides whether
    // those frames are ever folded. A legal operator stop mid-run.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if f.status() == SessionStatus::Busy {
            break;
        }
        assert!(Instant::now() < deadline, "run never went busy");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    f.supervisor.stop_session(f.uid).await.unwrap();
    wait_for("life summary", || {
        !details(&f.telemetry, f.uid, "session-stream-summary").is_empty()
    })
    .await;
    let s = summary(&f.telemetry, f.uid);
    assert_eq!(field(&s, "tokens_in"), 3_000, "TAIL LOST: {s}");
    assert_eq!(field(&s, "tokens_out"), 3_000, "TAIL LOST: {s}");
}
