//! Session supervision integration tests (Sprint 03 step 02; SPEC §6,
//! ADR-0015 — CONVENTIONS §9 daemon-behavior doctrine).
//!
//! Every scenario runs the REAL supervision code against the **FakeAgent**
//! binary (`src/bin/fakeagent.rs`, ADR-0015 cl. 5) spawned through
//! [`HostProcessSpawner`] — no podman. The podman-side surface (session
//! argv, `--user`/`--workdir`/`-e` composition, secret redaction,
//! `--continue` on restart) is asserted through [`PodmanSpawner`] +
//! `FakeRuntime`, which records argv built by the same pure builders as
//! production. Container *semantics* live in `supervise_guarded.rs`.
//!
//! Coverage: clean run (ready → brief → settle → stop), crash-restart with
//! incarnation bump, restart-budget exhaustion (TITLE.md lineage + sled
//! traces), readiness timeout, graceful stop, grace-window kill, hard kill
//! mid-task, stop-during-backoff, secret env injection, daemon-death
//! recovery (CONVENTIONS §3 r6), and multi-session shutdown.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::charter::{Charter, CommissionStatus};
use hamlet_core::config::{Config, Secrets};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{SessionId, SessionRecord, SessionStatus};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::title_stub;
use hamlet_daemon::podman::{FakeRuntime, Runtime};
use hamlet_daemon::supervise::{
    HostProcessSpawner, ParcelRef, PodmanSpawner, SessionSpawner, SuperviseError, Supervisor,
};
use hamlet_daemon::telemetry::{AgentKey, Telemetry, TraceRecord};
use serde_json::{json, Value};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const BRIEF: &str = "Do the thing.";
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

enum Mode {
    /// Real FakeAgent processes over the host-process spawner seam.
    HostFake,
    /// `FakeRuntime` (argv recording, in-memory handles) behind
    /// [`PodmanSpawner`].
    PodmanFake,
}

struct Fixture {
    tmp: tempfile::TempDir,
    tree: HamletTree,
    config: Config,
    telemetry: Arc<Telemetry>,
    supervisor: Supervisor,
    runtime: Option<Arc<FakeRuntime>>,
    uid: u32,
}

fn fixture(tweak: impl FnOnce(&mut Config), mode: Mode) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    let uid = charter
        .allocate_slot(name("scribe"), Some(COMMISSION))
        .unwrap();
    assert_eq!(uid, 1001);
    charter
        .allocate_slot(name("copier"), Some(COMMISSION))
        .unwrap();
    charter.save_atomic(&tree.charter()).unwrap();
    for (uid, who) in [(1001, "scribe"), (1002, "copier")] {
        fs::create_dir_all(tree.parcel(uid)).unwrap();
        let title = title_stub(&name(who), uid, COMMISSION, 1_700_000_000);
        fs::write(tree.parcel_title(uid), title).unwrap();
    }

    let mut config = Config::default();
    tweak(&mut config);
    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let (spawner, runtime): (Arc<dyn SessionSpawner>, Option<Arc<FakeRuntime>>) = match mode {
        Mode::HostFake => (Arc::new(HostProcessSpawner::new(FAKEAGENT)), None),
        Mode::PodmanFake => {
            let runtime = Arc::new(FakeRuntime::new());
            let spawner: Arc<dyn SessionSpawner> =
                Arc::new(PodmanSpawner::new(Arc::clone(&runtime) as Arc<dyn Runtime>));
            (spawner, Some(runtime))
        }
    };
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
        runtime,
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

    fn parcel(&self, uid: u32, container: &str, script: Option<&Path>) -> ParcelRef {
        let mut env = BTreeMap::new();
        if let Some(script) = script {
            env.insert(
                "FAKEAGENT_SCRIPT".to_owned(),
                script.to_string_lossy().into_owned(),
            );
        }
        ParcelRef::new(uid, name(container), COMMISSION).with_env(env)
    }

    fn scribe_parcel(&self, script: Option<&Path>) -> ParcelRef {
        self.parcel(self.uid, "scribe", script)
    }
}

fn record(supervisor: &Supervisor, uid: u32) -> SessionRecord {
    supervisor
        .session_record(uid)
        .unwrap()
        .unwrap_or_else(|| panic!("no session record for uid {uid}"))
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

/// Poll until `pred` holds; fail the test with context on timeout.
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

// ---------------------------------------------------------------------------
// Clean run
// ---------------------------------------------------------------------------

/// Clean run: spawn → readiness probe answered (`starting` → `running`) →
/// brief delivered as one tasking envelope → run settles at the task
/// boundary (`idle`) → graceful stop. Wire bytes are the ADR-0015 goldens;
/// the correlation seq comes from the sled counter.
#[tokio::test]
async fn clean_run_ready_brief_settle_stop() {
    let f = fixture(|_| {}, Mode::HostFake);
    let log = f.log_path("clean");
    let script = f.write_script("clean", json!({ "log": log }));
    let session = f
        .supervisor
        .start_session(&f.scribe_parcel(Some(&script)), BRIEF, &Secrets::default())
        .await
        .unwrap();
    assert_eq!(session, SessionId::new(1001, 0).unwrap());

    wait_for("session idle after the brief run", || {
        record(&f.supervisor, f.uid).status == SessionStatus::Idle
    })
    .await;
    let rec = record(&f.supervisor, f.uid);
    assert_eq!(rec.id.incarnation, 0);
    assert_eq!(rec.restart_count, 0);

    // Wire evidence: the exact pi-dialect bytes reached the agent —
    // readiness probe first, then the brief as a tasking prompt with the
    // sled-allocated correlation (t-<seq>-<uid>.<inc>).
    let logged = fs::read_to_string(&log).unwrap();
    assert!(
        logged.contains(r#"{"type":"get_state","id":"rdy-1001.0"}"#),
        "probe missing: {logged}"
    );
    assert!(
        logged.contains(r#"{"type":"prompt","id":"t-1-1001.0:p","message":"Do the thing."}"#),
        "brief prompt missing: {logged}"
    );

    // A second start while the task is live is refused.
    assert!(matches!(
        f.supervisor
            .start_session(&f.scribe_parcel(Some(&script)), "", &Secrets::default())
            .await,
        Err(SuperviseError::AlreadyActive(1001))
    ));

    // Graceful stop → `stopped`, no restart, task gone.
    assert_eq!(
        f.supervisor.stop_session(f.uid).await.unwrap(),
        SessionStatus::Stopped
    );
    assert_eq!(record(&f.supervisor, f.uid).status, SessionStatus::Stopped);
    assert!(!f.supervisor.is_live(f.uid));

    for expected in [
        "session-started",
        "session-ready",
        "session-enqueued",
        "session-delivered",
        "session-stop-requested",
        "session-stopped",
    ] {
        assert!(
            events(&f.telemetry, f.uid).contains(&expected.to_owned()),
            "trace {expected} missing: {:?}",
            events(&f.telemetry, f.uid)
        );
    }
}

// ---------------------------------------------------------------------------
// Restart policy
// ---------------------------------------------------------------------------

/// Crash → auto-restart: the exit is classified `crashed`, the backoff
/// schedule runs off the persisted counter, and the new life bumps the
/// incarnation (SessionId per ADR-0015 cl. 4). The brief is delivered only
/// in the first life of the chain.
#[tokio::test]
async fn crash_restarts_with_new_incarnation() {
    let f = fixture(
        |cfg| {
            cfg.sessions.restart.backoff_initial_ms = 100;
            cfg.sessions.restart.backoff_max_ms = 100;
        },
        Mode::HostFake,
    );
    let log = f.log_path("crash");
    let script = f.write_script(
        "crash",
        json!({ "log": log, "crash_after_commands": 2, "crash_exit_code": 7 }),
    );
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), BRIEF, &Secrets::default())
        .await
        .unwrap();

    wait_for("restarted incarnation running", || {
        let rec = record(&f.supervisor, f.uid);
        rec.id.incarnation == 1 && rec.status == SessionStatus::Running
    })
    .await;
    assert_eq!(record(&f.supervisor, f.uid).restart_count, 1);

    // Two lives in the log; the second saw only its readiness probe
    // (probe id carries the new incarnation token).
    let logged = fs::read_to_string(&log).unwrap();
    let lives: Vec<&str> = logged.split("# fakeagent start").collect();
    assert_eq!(lives.len(), 3, "expected two lives: {logged}");
    assert!(lives[2].contains(r#""id":"rdy-1001.1""#), "{logged}");
    assert!(!lives[2].contains("prompt"), "brief re-delivered: {logged}");

    // Crash telemetry is daemon-parsed, not agent-reported.
    let traces = traces(&f.telemetry, f.uid);
    assert!(
        traces
            .iter()
            .any(|t| t.event == "session-crashed" && t.detail.contains("exit code 7")),
        "crash trace missing: {traces:?}"
    );
    assert!(
        traces
            .iter()
            .any(|t| t.event == "session-restart-scheduled" && t.detail.contains("backoff_ms=100")),
        "restart trace missing: {traces:?}"
    );

    assert_eq!(
        f.supervisor.stop_session(f.uid).await.unwrap(),
        SessionStatus::Stopped
    );
}

/// Budget exhaustion → permanently `crashed`: TITLE.md lineage stamp +
/// sled trace (SPEC §6). The counter reached `max_restarts` without ever
/// settling, so `backoff_ms` returns None and the chain ends.
#[tokio::test]
async fn restart_limit_exhaustion_is_permanent_with_lineage() {
    let f = fixture(
        |cfg| {
            cfg.sessions.restart.max_restarts = 2;
            cfg.sessions.restart.backoff_initial_ms = 100;
            cfg.sessions.restart.backoff_max_ms = 100;
        },
        Mode::HostFake,
    );
    let script = f.write_script(
        "exhaust",
        json!({ "crash_after_commands": 1, "crash_exit_code": 70 }),
    );
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), "", &Secrets::default())
        .await
        .unwrap();

    wait_for("permanent crash", || {
        record(&f.supervisor, f.uid).status == SessionStatus::Crashed
            && !f.supervisor.is_live(f.uid)
    })
    .await;
    let rec = record(&f.supervisor, f.uid);
    assert_eq!(rec.id.incarnation, 2, "lives: inc 0, 1, 2");
    assert_eq!(rec.restart_count, 2);

    // TITLE.md lineage (crash lineage goes to TITLE.md, SPEC §6).
    let title = fs::read_to_string(f.tree.parcel_title(f.uid)).unwrap();
    assert!(
        title.contains("session 1001.2 permanently crashed after 2 auto-restart(s)"),
        "lineage stamp missing: {title}"
    );
    assert!(title.contains("exit code 70"), "{title}");

    let events = events(&f.telemetry, f.uid);
    assert!(
        events.contains(&"session-permanently-crashed".to_owned()),
        "{events:?}"
    );
    assert_eq!(
        events.iter().filter(|e| *e == "session-crashed").count(),
        3,
        "one crash trace per life: {events:?}"
    );
}

/// Readiness window expiry (`sessions.ready_timeout_ms`) → `crashed` →
/// restart policy; with `max_restarts = 0` it is immediately permanent.
#[tokio::test]
async fn readiness_timeout_crashes_and_respects_zero_budget() {
    let f = fixture(
        |cfg| {
            cfg.sessions.ready_timeout_ms = 1_000;
            cfg.sessions.restart.max_restarts = 0;
        },
        Mode::HostFake,
    );
    let script = f.write_script("neverready", json!({ "never_ready": true }));
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), "", &Secrets::default())
        .await
        .unwrap();

    wait_for("readiness-timeout crash", || {
        record(&f.supervisor, f.uid).status == SessionStatus::Crashed
            && !f.supervisor.is_live(f.uid)
    })
    .await;
    let events = events(&f.telemetry, f.uid);
    assert!(
        events.contains(&"session-readiness-timeout".to_owned()),
        "{events:?}"
    );
    let title = fs::read_to_string(f.tree.parcel_title(f.uid)).unwrap();
    assert!(
        title.contains("permanently crashed after 0 auto-restart(s)"),
        "{title}"
    );
}

/// A stop/kill request landing inside a restart-backoff window cancels the
/// restart; crashed→stopped is not a legal transition (ADR-0015 cl. 4), so
/// the session **stays crashed** — recorded decision, traced.
#[tokio::test]
async fn stop_during_backoff_cancels_restart_and_stays_crashed() {
    let f = fixture(
        |cfg| {
            cfg.sessions.restart.backoff_initial_ms = 60_000;
            cfg.sessions.restart.backoff_max_ms = 60_000;
        },
        Mode::HostFake,
    );
    let script = f.write_script("backoff", json!({ "crash_after_commands": 1 }));
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), "", &Secrets::default())
        .await
        .unwrap();

    // Wait for the crash to be persisted while the task is still live
    // (sleeping out a 60s backoff).
    wait_for("crashed and in backoff", || {
        record(&f.supervisor, f.uid).status == SessionStatus::Crashed && f.supervisor.is_live(f.uid)
    })
    .await;

    let status = f.supervisor.stop_session(f.uid).await.unwrap();
    assert_eq!(status, SessionStatus::Crashed, "stays crashed");
    wait_for("task ended", || !f.supervisor.is_live(f.uid)).await;
    let rec = record(&f.supervisor, f.uid);
    assert_eq!(rec.id.incarnation, 0, "no restart happened");
    assert!(events(&f.telemetry, f.uid).contains(&"session-stop-during-backoff".to_owned()));
}

// ---------------------------------------------------------------------------
// Stop / kill paths (Sprint 07 hibernation consumes both)
// ---------------------------------------------------------------------------

/// Graceful stop at a boundary: stdin closes (pi's orderly-shutdown
/// signal), the agent exits on EOF, charter → `stopped`.
#[tokio::test]
async fn graceful_stop_at_boundary() {
    let f = fixture(|_| {}, Mode::HostFake);
    let log = f.log_path("graceful");
    let script = f.write_script("graceful", json!({ "log": log }));
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), "", &Secrets::default())
        .await
        .unwrap();
    wait_for("running", || {
        record(&f.supervisor, f.uid).status == SessionStatus::Running
    })
    .await;

    assert_eq!(
        f.supervisor.stop_session(f.uid).await.unwrap(),
        SessionStatus::Stopped
    );
    let logged = fs::read_to_string(&log).unwrap();
    assert!(
        !logged.contains("prompt"),
        "no work was delivered: {logged}"
    );
    assert_eq!(record(&f.supervisor, f.uid).id.incarnation, 0);
}

/// Graceful stop mid-task with an agent that ignores EOF: the grace window
/// (`sessions.stop_grace_ms`) expires, the supervisor hard-kills, charter →
/// `stopped` (daemon-intended end, never `crashed`).
#[tokio::test]
async fn graceful_stop_kills_after_grace_window() {
    let f = fixture(
        |cfg| {
            cfg.sessions.stop_grace_ms = 500;
        },
        Mode::HostFake,
    );
    let script = f.write_script("grace", json!({ "slow_task": true, "ignore_eof": true }));
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), BRIEF, &Secrets::default())
        .await
        .unwrap();
    wait_for("busy", || {
        record(&f.supervisor, f.uid).status == SessionStatus::Busy
    })
    .await;

    let t0 = Instant::now();
    assert_eq!(
        f.supervisor.stop_session(f.uid).await.unwrap(),
        SessionStatus::Stopped
    );
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(450),
        "grace window must be honored: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(10), "stop hung: {elapsed:?}");
    let events = events(&f.telemetry, f.uid);
    assert!(
        events.contains(&"session-grace-expired".to_owned()),
        "{events:?}"
    );
    assert!(events.contains(&"session-killed".to_owned()), "{events:?}");
}

/// Hard kill mid-task: immediate SIGKILL semantics, no drain, no grace —
/// charter → `stopped`.
#[tokio::test]
async fn hard_kill_mid_task() {
    let f = fixture(|_| {}, Mode::HostFake);
    let script = f.write_script("kill", json!({ "slow_task": true }));
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), BRIEF, &Secrets::default())
        .await
        .unwrap();
    wait_for("busy", || {
        record(&f.supervisor, f.uid).status == SessionStatus::Busy
    })
    .await;

    let t0 = Instant::now();
    assert_eq!(
        f.supervisor.kill_session(f.uid).await.unwrap(),
        SessionStatus::Stopped
    );
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "kill must not wait out the grace window: {:?}",
        t0.elapsed()
    );
    assert_eq!(record(&f.supervisor, f.uid).status, SessionStatus::Stopped);
    assert!(events(&f.telemetry, f.uid).contains(&"session-killed".to_owned()));
}

/// Restart-resumability (CONVENTIONS §3 r6): a `kill -9`ed daemon leaves
/// stale *alive* charter statuses; the next daemon marks them `crashed`
/// (re-attach over `podman exec` stdio is impossible) and can restart the
/// slot with a bumped incarnation.
#[tokio::test]
async fn daemon_death_recovery_marks_orphans_crashed() {
    let f = fixture(|_| {}, Mode::HostFake);
    let script = f.write_script("recover", json!({}));
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), "", &Secrets::default())
        .await
        .unwrap();
    wait_for("running", || {
        record(&f.supervisor, f.uid).status == SessionStatus::Running
    })
    .await;

    // Simulate abrupt daemon death: children killed, tasks aborted, charter
    // untouched (still claims a live session).
    f.supervisor.abandon();
    assert_eq!(record(&f.supervisor, f.uid).status, SessionStatus::Running);

    // Fresh daemon over the same tree.
    let sup2 = Supervisor::new(
        f.tree.clone(),
        f.config.clone(),
        Arc::new(HostProcessSpawner::new(FAKEAGENT)) as Arc<dyn SessionSpawner>,
        Arc::clone(&f.telemetry),
    );
    assert_eq!(sup2.recover().unwrap(), vec![f.uid]);
    let rec = record(&sup2, f.uid);
    assert_eq!(rec.status, SessionStatus::Crashed);
    assert_eq!(rec.id.incarnation, 0, "recovery does not bump");
    assert!(sup2.recover().unwrap().is_empty(), "idempotent");
    assert!(events(&f.telemetry, f.uid).contains(&"session-orphaned-recovered".to_owned()));

    // Operator restart after recovery → new incarnation, fresh budget.
    let session = sup2
        .start_session(&f.scribe_parcel(Some(&script)), "", &Secrets::default())
        .await
        .unwrap();
    assert_eq!(session, SessionId::new(1001, 1).unwrap());
    wait_for("restarted running", || {
        record(&sup2, f.uid).status == SessionStatus::Running
    })
    .await;
    assert_eq!(record(&sup2, f.uid).restart_count, 0);
    sup2.stop_session(f.uid).await.unwrap();
}

// ---------------------------------------------------------------------------
// Secrets & podman-side composition
// ---------------------------------------------------------------------------

/// `.env` secrets ride the exec env out-of-band (never files in the home):
/// `HAMLET_API_KEY_<P>` → `<P>_API_KEY`, model key/endpoint passthrough,
/// pi hygiene vars — verified end-to-end inside the spawned agent.
#[tokio::test]
async fn secrets_reach_the_session_environment_only() {
    let f = fixture(|_| {}, Mode::HostFake);
    let log = f.log_path("secrets");
    let script = f.write_script(
        "secrets",
        json!({
            "log": log,
            "expect_env": {
                "ANTHROPIC_API_KEY": "test-key-do-not-use",
                "HAMLET_MODEL_API_KEY": "test-key-do-not-use-2",
                "HAMLET_MODEL_ENDPOINT": "https://models.example/v1",
                "PI_TELEMETRY": "0",
                "PI_SKIP_VERSION_CHECK": "1",
            },
        }),
    );
    let secrets = Secrets::from_vars([
        ("HAMLET_API_KEY_ANTHROPIC", "test-key-do-not-use"),
        ("HAMLET_MODEL_API_KEY", "test-key-do-not-use-2"),
        ("HAMLET_MODEL_ENDPOINT", "https://models.example/v1"),
    ]);
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script)), "", &secrets)
        .await
        .unwrap();
    wait_for("running", || {
        record(&f.supervisor, f.uid).status == SessionStatus::Running
    })
    .await;

    let logged = fs::read_to_string(&log).unwrap();
    for key in [
        "ANTHROPIC_API_KEY",
        "HAMLET_MODEL_API_KEY",
        "HAMLET_MODEL_ENDPOINT",
        "PI_TELEMETRY",
        "PI_SKIP_VERSION_CHECK",
    ] {
        assert!(
            logged.contains(&format!("env-check ok:{key}")),
            "env {key} not injected: {logged}"
        );
    }
    assert!(!logged.contains("mismatch"), "{logged}");
    assert!(!logged.contains("missing"), "{logged}");
    // The fake's diagnostics never carry values, and neither do ours.
    assert!(!logged.contains("test-key-do-not-use"), "{logged}");
    f.supervisor.stop_session(f.uid).await.unwrap();
}

/// Podman path: the session argv is the ADR-0015 cl. 1 template with
/// sorted `-e` flags — every value masked in the recorded (redacted) argv.
#[tokio::test]
async fn podman_session_argv_is_templated_and_redacted() {
    let f = fixture(
        |cfg| {
            cfg.sessions.restart.max_restarts = 0;
        },
        Mode::PodmanFake,
    );
    let secrets = Secrets::from_vars([("HAMLET_API_KEY_ANTHROPIC", "test-key-do-not-use")]);
    f.supervisor
        .start_session(&f.scribe_parcel(None), "", &secrets)
        .await
        .unwrap();
    // The in-memory handle has empty stdout → EOF before readiness →
    // crashed → budget 0 → permanently crashed (chain ends fast).
    wait_for("chain ended", || !f.supervisor.is_live(f.uid)).await;

    let runtime = f.runtime.as_ref().unwrap();
    let argvs = runtime.argv_log();
    let session = argvs
        .iter()
        .find(|argv| argv.contains(&"--user".to_owned()))
        .unwrap_or_else(|| panic!("no session argv recorded: {argvs:?}"));
    assert_eq!(
        session,
        &vec![
            "podman".to_owned(),
            "exec".to_owned(),
            "-i".to_owned(),
            "--user".to_owned(),
            "1001:1001".to_owned(),
            "--workdir".to_owned(),
            "/home/1001".to_owned(),
            "-e".to_owned(),
            "ANTHROPIC_API_KEY=<redacted>".to_owned(),
            "-e".to_owned(),
            "PI_SKIP_VERSION_CHECK=<redacted>".to_owned(),
            "-e".to_owned(),
            "PI_TELEMETRY=<redacted>".to_owned(),
            "scribe".to_owned(),
            "pi".to_owned(),
            "--mode".to_owned(),
            "rpc".to_owned(),
        ]
    );
    // No secret value ever reaches the recorded argv (CONVENTIONS §6).
    assert!(
        !argvs
            .iter()
            .flatten()
            .any(|token| token.contains("test-key-do-not-use")),
        "{argvs:?}"
    );
}

/// Restarted incarnations pass pi `--continue` (`continue_on_restart`,
/// ADR-0015 cl. 4) — asserted on the recorded podman argv of both lives.
#[tokio::test]
async fn restarted_incarnation_argv_carries_continue() {
    let f = fixture(
        |cfg| {
            cfg.sessions.restart.max_restarts = 1;
            cfg.sessions.restart.backoff_initial_ms = 100;
            cfg.sessions.restart.backoff_max_ms = 100;
        },
        Mode::PodmanFake,
    );
    f.supervisor
        .start_session(&f.scribe_parcel(None), "", &Secrets::default())
        .await
        .unwrap();
    wait_for("chain ended", || {
        !f.supervisor.is_live(f.uid)
            && record(&f.supervisor, f.uid).status == SessionStatus::Crashed
    })
    .await;
    assert_eq!(record(&f.supervisor, f.uid).id.incarnation, 1);

    let runtime = f.runtime.as_ref().unwrap();
    let log = runtime.argv_log();
    let sessions: Vec<&Vec<String>> = log
        .iter()
        .filter(|argv| argv.contains(&"--user".to_owned()))
        .collect();
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    fn tail(argv: &[String], n: usize) -> Vec<&str> {
        argv[argv.len() - n..].iter().map(String::as_str).collect()
    }
    assert_eq!(tail(sessions[0], 3), ["pi", "--mode", "rpc"]);
    assert_eq!(tail(sessions[1], 4), ["pi", "--mode", "rpc", "--continue"]);
}

// ---------------------------------------------------------------------------
// API surface
// ---------------------------------------------------------------------------

/// `parcel_ref` derives the target from the charter; missing slots are
/// typed errors — both through `parcel_ref` and `start_session`.
#[tokio::test]
async fn parcel_ref_derives_from_charter_and_rejects_unknown_slots() {
    let f = fixture(|_| {}, Mode::HostFake);
    let parcel = f.supervisor.parcel_ref(1001).unwrap();
    assert_eq!(parcel.uid, 1001);
    assert_eq!(parcel.container, name("scribe"));
    assert_eq!(parcel.commission, COMMISSION);
    assert!(parcel.env.is_empty());

    assert!(matches!(
        f.supervisor.parcel_ref(1500),
        Err(SuperviseError::NoSlot(1500))
    ));
    let ghost = ParcelRef::new(1500, name("ghost"), 0);
    assert!(matches!(
        f.supervisor
            .start_session(&ghost, "", &Secrets::default())
            .await,
        Err(SuperviseError::NoSlot(1500))
    ));
    // Stop/kill on a slot that was never live is typed, not a panic.
    assert!(matches!(
        f.supervisor.stop_session(1500).await,
        Err(SuperviseError::NotLive(1500))
    ));
}

/// Shutdown drains every live session (sorted), leaving `stopped` records —
/// the daemon-exit path Sprint 07 hibernation builds on.
#[tokio::test]
async fn shutdown_stops_every_live_session() {
    let f = fixture(|_| {}, Mode::HostFake);
    let script_a = f.write_script("shut-a", json!({}));
    let script_b = f.write_script("shut-b", json!({}));
    f.supervisor
        .start_session(&f.scribe_parcel(Some(&script_a)), "", &Secrets::default())
        .await
        .unwrap();
    f.supervisor
        .start_session(
            &f.parcel(1002, "copier", Some(&script_b)),
            "",
            &Secrets::default(),
        )
        .await
        .unwrap();
    wait_for("both running", || {
        record(&f.supervisor, 1001).status == SessionStatus::Running
            && record(&f.supervisor, 1002).status == SessionStatus::Running
    })
    .await;

    let outcomes = f.supervisor.shutdown().await;
    assert_eq!(outcomes.len(), 2);
    for (uid, outcome) in &outcomes {
        assert_eq!(
            outcome.as_ref().unwrap(),
            &SessionStatus::Stopped,
            "uid {uid}"
        );
    }
    assert_eq!(record(&f.supervisor, 1001).status, SessionStatus::Stopped);
    assert_eq!(record(&f.supervisor, 1002).status, SessionStatus::Stopped);
    assert!(!f.supervisor.is_live(1001) && !f.supervisor.is_live(1002));
}
