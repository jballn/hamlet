//! Daemon-parsed stream telemetry (Sprint 03 step 04; SPEC §6, §11.2,
//! ADR-0016/0017 — CONVENTIONS §9 daemon-behavior doctrine).
//!
//! Real supervision + real sled against the **FakeAgent** binary over
//! [`HostProcessSpawner`] — no podman, and no agent self-report anywhere:
//! every number asserted here was parsed by the daemon off the JSON-RPC
//! stdout stream.
//!
//! Coverage: exact token/cost totals from a scripted stream, the
//! latency-spike threshold as config, malformed-frame resilience (garbage
//! interleaved, content never leaked), a 10k-frame soak that must not
//! starve the delivery writer, interrupt-acknowledgment joining on the
//! correlation id, `metrics_for(session|uid|commission)`, and
//! kill/restart resumability of the metrics themselves.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::charter::{Charter, CommissionStatus};
use hamlet_core::config::{Config, Secrets};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{
    InterruptPriority, InterruptSource, QueuedKind, SessionId, SessionStatus,
};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::title_stub;
use hamlet_daemon::supervise::{HostProcessSpawner, ParcelRef, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::{
    AgentKey, MetricsScope, SessionKey, Telemetry, TokenTally, TraceRecord,
};
use serde_json::{json, Value};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const COMMISSION: u64 = 7;
const BRIEF: &str = "Keep the ledger.";
/// The soak size the step-04 prompt fixes: 10k frames per run.
const SOAK_FRAMES: u32 = 10_000;

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
        .insert(COMMISSION, CommissionStatus::Open);
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

    fn parcel(&self, script: &Path) -> ParcelRef {
        let mut env = BTreeMap::new();
        env.insert(
            "FAKEAGENT_SCRIPT".to_owned(),
            script.to_string_lossy().into_owned(),
        );
        ParcelRef::new(self.uid, name("scribe"), COMMISSION).with_env(env)
    }

    async fn start(&self, script: &Path, brief: &str) -> SessionId {
        self.supervisor
            .start_session(&self.parcel(script), brief, &Secrets::default())
            .await
            .unwrap()
    }

    fn session_key(&self, session: SessionId) -> SessionKey {
        SessionKey::new(COMMISSION, self.uid, session.as_u64())
    }
}

fn status_of(supervisor: &Supervisor, uid: u32) -> SessionStatus {
    supervisor.session_record(uid).unwrap().unwrap().status
}

fn status(f: &Fixture) -> SessionStatus {
    status_of(&f.supervisor, f.uid)
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

/// The last `session-stream-summary` detail (one per session life).
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

/// A cumulative usage snapshot exactly as pi wires it.
fn usage(input: u64, output: u64, total_usd: f64) -> Value {
    json!({
        "input": input,
        "output": output,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": input + output,
        "cost": {
            "input": 0.0, "output": 0.0, "cacheRead": 0.0,
            "cacheWrite": 0.0, "total": total_usd,
        },
    })
}

/// Poll until `pred` holds; fail with context on timeout.
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

// ---------------------------------------------------------------------------
// Exact totals from a scripted stream
// ---------------------------------------------------------------------------

/// Golden: two turns of cumulative `message_update` snapshots plus an
/// authoritative `message_end` land in sled as EXACT totals. The agent
/// reported nothing; the daemon differenced the cumulative snapshots.
#[tokio::test]
async fn exact_token_and_cost_totals_from_a_scripted_stream() {
    let f = fixture(|_| {});
    let log = f.log_path("totals");
    let script = f.write_script(
        "totals",
        json!({
            "log": log,
            "turns_per_run": 2,
            "emit_message_end": true,
            "usage_sequence": [usage(10, 2, 0.000001), usage(10, 5, 0.000004)],
        }),
    );
    let session = f.start(&script, BRIEF).await;
    wait_for("idle after the brief run", || {
        status(&f) == SessionStatus::Idle
    })
    .await;
    f.supervisor.stop_session(f.uid).await.unwrap();

    let key = f.session_key(session);
    // Per turn: final snapshot 10 in / 5 out / 4 micro-USD. Two turns.
    assert_eq!(
        f.telemetry.session_tokens(key).unwrap(),
        Some(TokenTally {
            input_tokens: 20,
            output_tokens: 10,
        })
    );
    assert_eq!(f.telemetry.session_cost_micro_usd(key).unwrap(), 8);

    let detail = summary(&f.telemetry, f.uid);
    assert!(detail.contains("tokens_in=20"), "{detail}");
    assert!(detail.contains("tokens_out=10"), "{detail}");
    assert!(detail.contains("cost_micro_usd=8"), "{detail}");
    assert!(detail.contains("turns=2"), "{detail}");
    assert!(detail.contains("dropped=0"), "{detail}");
    // Nothing leaked into the charter: telemetry is sled-only (§11.2).
    let charter = fs::read_to_string(f.tree.charter()).unwrap();
    assert!(!charter.contains("tokens"), "telemetry in charter.json");
}

/// The live query surface answers all three scopes off the same run.
#[tokio::test]
async fn metrics_for_session_uid_and_commission() {
    let f = fixture(|_| {});
    let script = f.write_script(
        "scopes",
        json!({ "usage_sequence": [usage(3, 4, 0.000002)] }),
    );
    let session = f.start(&script, BRIEF).await;
    wait_for("idle", || status(&f) == SessionStatus::Idle).await;
    f.supervisor.stop_session(f.uid).await.unwrap();

    let by_session = f
        .supervisor
        .metrics_for_session(session, COMMISSION, 10)
        .unwrap();
    assert_eq!(by_session.totals.input_tokens, 3);
    assert_eq!(by_session.totals.output_tokens, 4);
    assert_eq!(by_session.totals.cost_micro_usd, 2);
    assert_eq!(by_session.sessions.len(), 1);

    let by_uid = f.supervisor.metrics_for_uid(f.uid, 10).unwrap();
    assert_eq!(by_uid.totals.input_tokens, 3);
    assert_eq!(by_uid.agents.len(), 1);
    assert!(
        by_uid
            .recent_traces
            .iter()
            .any(|t| t.event == "session-stream-summary"),
        "summary trace missing from the uid scope"
    );

    let by_commission = f.supervisor.metrics_for_commission(COMMISSION, 10).unwrap();
    assert_eq!(by_commission.totals.input_tokens, 3);
    assert_eq!(by_commission.agents.len(), 1);

    // The same numbers straight off the Sprint-01 store API.
    let direct = f
        .telemetry
        .metrics_for(MetricsScope::Agent(agent_key(f.uid)), 10)
        .unwrap();
    assert_eq!(direct.totals, by_uid.totals);
}

// ---------------------------------------------------------------------------
// Latency spikes
// ---------------------------------------------------------------------------

/// The threshold is config (`sessions.latency_spike_ms`, v4): a 300 ms tool
/// call is a spike at 50 ms and invisible at an hour.
#[tokio::test]
async fn latency_spikes_follow_the_configured_threshold() {
    let script_value = json!({
        "tool_calls": [{ "name": "bash", "delay_ms": 300, "is_error": false }],
        "usage_sequence": [usage(1, 1, 0.0)],
    });

    let low = fixture(|config| config.sessions.latency_spike_ms = 50);
    let script = low.write_script("spike-low", script_value.clone());
    let session = low.start(&script, BRIEF).await;
    wait_for("idle (low threshold)", || {
        status(&low) == SessionStatus::Idle
    })
    .await;
    low.supervisor.stop_session(low.uid).await.unwrap();

    let snapshot = low
        .telemetry
        .metrics_for(MetricsScope::Session(low.session_key(session)), 50)
        .unwrap();
    let operations: Vec<&str> = snapshot
        .recent_spikes
        .iter()
        .map(|spike| spike.operation.as_str())
        .collect();
    assert!(
        operations.contains(&"tool-call bash"),
        "tool-call spike missing: {operations:?}"
    );
    let tool = snapshot
        .recent_spikes
        .iter()
        .find(|spike| spike.operation == "tool-call bash")
        .unwrap();
    // Spans are measured between frame *reads*, so a scripted delay can read
    // back short of its nominal value — and the shortfall grows with host
    // load, not with the delay (QA05-11: under battery load the old 120 ms
    // script came back as 97 ms against a >= 100 floor — a coin flip at the
    // gate). The scripted delay is now wide enough that this floor still
    // catches a collapsed span while no host shape can sink it.
    assert!(
        tool.duration_ms >= 100,
        "duration far below the scripted delay: {tool:?}"
    );
    // The enclosing turn and task spans breach the same threshold.
    assert!(operations.contains(&"agent-turn"), "{operations:?}");
    assert!(snapshot.totals.latency_spikes >= 2, "{snapshot:?}");

    // Same stream, threshold out of reach → no spikes at all.
    let high = fixture(|config| config.sessions.latency_spike_ms = 3_600_000);
    let script = high.write_script("spike-high", script_value);
    let session = high.start(&script, BRIEF).await;
    wait_for("idle (high threshold)", || {
        status(&high) == SessionStatus::Idle
    })
    .await;
    high.supervisor.stop_session(high.uid).await.unwrap();
    let snapshot = high
        .telemetry
        .metrics_for(MetricsScope::Session(high.session_key(session)), 50)
        .unwrap();
    assert!(
        snapshot.recent_spikes.is_empty(),
        "spikes over a 1 h threshold: {:?}",
        snapshot.recent_spikes
    );
    assert!(summary(&high.telemetry, high.uid).contains("spikes=0"));
}

// ---------------------------------------------------------------------------
// Malformed-frame resilience
// ---------------------------------------------------------------------------

/// Garbage interleaved through a real run: the session still reaches the
/// task boundary, the totals stay exact, malformed frames are traced and
/// counted — and their content never reaches sled (CONVENTIONS §6).
#[tokio::test]
async fn malformed_frames_are_skipped_traced_and_never_leaked() {
    let f = fixture(|_| {});
    let secret = "test-key-do-not-use";
    let script = f.write_script(
        "garbage",
        json!({
            "usage_sequence": [usage(6, 7, 0.000003)],
            "malformed_every_n": 2,
            "malformed": [
                format!("###not json### {secret}"),
                "{\"type\": ",
                "[1,2,3]",
                "{\"nope\":true}",
            ],
        }),
    );
    let session = f.start(&script, BRIEF).await;
    wait_for("idle despite the garbage", || {
        status(&f) == SessionStatus::Idle
    })
    .await;
    f.supervisor.stop_session(f.uid).await.unwrap();

    // Supervision is unaffected: no crash, no restart, exact accounting.
    assert_eq!(status(&f), SessionStatus::Stopped);
    let record = f.supervisor.session_record(f.uid).unwrap().unwrap();
    assert_eq!(record.id, session);
    assert_eq!(record.restart_count, 0);
    assert_eq!(
        f.telemetry.session_tokens(f.session_key(session)).unwrap(),
        Some(TokenTally {
            input_tokens: 6,
            output_tokens: 7,
        })
    );

    let detail = summary(&f.telemetry, f.uid);
    assert!(
        field(&detail, "malformed") > 0,
        "garbage was not counted: {detail}"
    );
    assert!(field(&detail, "tokens_in") == 6, "{detail}");
    assert!(
        events(&f.telemetry, f.uid).contains(&"session-stream-malformed".to_owned()),
        "no malformed trace: {:?}",
        events(&f.telemetry, f.uid)
    );
    // No trace value carries the frame content or the planted secret.
    for record in traces(&f.telemetry, f.uid) {
        assert!(
            !record.detail.contains(secret) && !record.detail.contains("###"),
            "frame content leaked: {:?}",
            record
        );
    }
}

/// Unmodelled record types are the pi-version-drift signal: traced once per
/// type, counted in the summary, never fatal.
#[tokio::test]
async fn unknown_record_types_are_the_drift_signal() {
    let f = fixture(|_| {});
    let script = f.write_script(
        "drift",
        json!({
            "usage_sequence": [usage(1, 1, 0.0)],
            "unknown_records": ["queue_update", "thinking_level_changed", "queue_update"],
        }),
    );
    let session = f.start(&script, BRIEF).await;
    wait_for("idle", || status(&f) == SessionStatus::Idle).await;
    f.supervisor.stop_session(f.uid).await.unwrap();

    let unknown = details(&f.telemetry, f.uid, "session-stream-unknown-record");
    assert_eq!(unknown, vec!["queue_update", "thinking_level_changed"]);
    let detail = summary(&f.telemetry, f.uid);
    assert!(field(&detail, "unknown") == 3, "{detail}");
    assert!(
        detail.contains("unknown_types=[queue_update:2,thinking_level_changed:1]"),
        "{detail}"
    );
    assert_eq!(
        f.telemetry
            .session_tokens(f.session_key(session))
            .unwrap()
            .map(|t| t.input_tokens),
        Some(1)
    );
}

// ---------------------------------------------------------------------------
// High-throughput soak
// ---------------------------------------------------------------------------

/// 10k frames per run: exact totals, zero dropped accounting, and the
/// delivery writer keeps its turn — an interrupt injected mid-flood is
/// written to the pipe immediately and a queued mail is delivered at the
/// next boundary.
#[tokio::test]
async fn soak_10k_frames_without_starving_the_delivery_writer() {
    let f = fixture(|config| config.sessions.queue_depth = 16);
    let log = f.log_path("soak");
    let script = f.write_script(
        "soak",
        json!({
            "log": log,
            "flood_frames": SOAK_FRAMES,
            "flood_usage": usage(1, 1, 0.000001),
        }),
    );
    let session = f.start(&script, BRIEF).await;
    wait_for("busy (flood in flight)", || {
        status(&f) == SessionStatus::Busy
    })
    .await;

    // The Divine Interruption must reach the pipe WHILE 10k frames stream
    // in: `steer` only returns once the life task's select loop has taken
    // the control message and written the records.
    let injected = Instant::now();
    let correlation = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::Human,
            InterruptPriority::Normal,
            "Stop and summarise.",
        )
        .await
        .unwrap();
    assert_eq!(status(&f), SessionStatus::Busy, "interrupt raced the flood");
    assert!(
        injected.elapsed() < Duration::from_secs(10),
        "delivery writer starved: interrupt took {:?}",
        injected.elapsed()
    );
    // A queued mail enqueued mid-flood must survive and be delivered later.
    f.supervisor
        .enqueue(f.uid, QueuedKind::Mail, "Ledger check, please.")
        .unwrap();

    wait_for("queue drained after the flood", || {
        f.supervisor.queues().len(f.uid).unwrap_or(usize::MAX) == 0
    })
    .await;
    wait_for("idle after the soak", || status(&f) == SessionStatus::Idle).await;
    f.supervisor.stop_session(f.uid).await.unwrap();

    // Both interrupt records were written to the wire (single-writer funnel
    // kept its framing under load).
    let logged = fs::read_to_string(&log).unwrap();
    assert!(
        logged.contains(&format!("{correlation}:s")),
        "steer record missing from the wire log"
    );
    assert!(
        logged.contains(&format!("{correlation}:a")),
        "abort record missing from the wire log"
    );
    assert!(
        logged.contains("Ledger check, please."),
        "queued mail was starved out"
    );

    // Accounting is exact and lossless: the summary's own counters must
    // equal what landed in sled, and each flooded run must equal
    // frames × step.
    let detail = summary(&f.telemetry, f.uid);
    let runs = field(&detail, "runs");
    assert!(runs >= 2, "expected the brief plus the mail run: {detail}");
    assert!(
        field(&detail, "frames") >= u64::from(SOAK_FRAMES) * runs,
        "frames undercounted: {detail}"
    );
    assert_eq!(field(&detail, "dropped"), 0, "{detail}");
    assert_eq!(
        field(&detail, "tokens_in"),
        u64::from(SOAK_FRAMES) * runs,
        "{detail}"
    );
    assert_eq!(
        field(&detail, "tokens_out"),
        u64::from(SOAK_FRAMES) * runs,
        "{detail}"
    );
    assert_eq!(
        field(&detail, "cost_micro_usd"),
        u64::from(SOAK_FRAMES) * runs,
        "{detail}"
    );

    let snapshot = f
        .telemetry
        .metrics_for(MetricsScope::Session(f.session_key(session)), 100)
        .unwrap();
    assert_eq!(snapshot.totals.input_tokens, u64::from(SOAK_FRAMES) * runs);
    assert_eq!(snapshot.totals.output_tokens, u64::from(SOAK_FRAMES) * runs);
    assert_eq!(
        snapshot.totals.cost_micro_usd,
        u64::from(SOAK_FRAMES) * runs
    );
    // The flood is cumulative-within-a-message, so idle gaps are not stalls
    // and the run stays inside the default 30 s threshold.
    assert!(
        snapshot.recent_spikes.is_empty(),
        "soak produced spikes: {:?}",
        snapshot.recent_spikes
    );
}

// ---------------------------------------------------------------------------
// Interrupt acknowledgments
// ---------------------------------------------------------------------------

/// The correlation id joins the injection to the agent's answer: both
/// interrupt records (`:s` steer, `:a` abort) come back as acked traces.
#[tokio::test]
async fn interrupt_acknowledgments_join_on_the_correlation() {
    let f = fixture(|config| config.sessions.latency_spike_ms = 1);
    let script = f.write_script(
        "interrupt",
        json!({ "settle_after_ms": 300, "usage_sequence": [usage(1, 1, 0.0)] }),
    );
    let session = f.start(&script, BRIEF).await;
    wait_for("busy (held run)", || status(&f) == SessionStatus::Busy).await;

    let correlation = f
        .supervisor
        .steer(
            f.uid,
            InterruptSource::Mayor,
            InterruptPriority::Critical,
            "Drop everything.",
        )
        .await
        .unwrap();
    wait_for("interrupt acked", || {
        details(&f.telemetry, f.uid, "session-interrupt-acked").len() >= 2
    })
    .await;
    f.supervisor.stop_session(f.uid).await.unwrap();

    let acks = details(&f.telemetry, f.uid, "session-interrupt-acked");
    // Critical priority binds to THREE records (ADR-0015 cl. 3):
    // clear_queue, steer, abort — each individually correlatable and acked.
    assert_eq!(acks.len(), 3, "{acks:?}");
    for (suffix, command) in [("q", "clear_queue"), ("s", "steer"), ("a", "abort")] {
        assert!(
            acks.iter().any(|detail| detail
                .starts_with(&format!("{correlation} record={suffix} command={command} "))),
            "no ack for the :{suffix} record: {acks:?}"
        );
    }
    // The injection trace and the ack trace share the correlation id — the
    // join key step 03 reserved for this step.
    let injected = details(&f.telemetry, f.uid, "session-interrupt-injected");
    assert!(
        injected.iter().any(|d| d.contains(correlation.as_str())),
        "{injected:?}"
    );
    // Ack latency was measured against the 1 ms threshold.
    let snapshot = f
        .telemetry
        .metrics_for(MetricsScope::Session(f.session_key(session)), 100)
        .unwrap();
    assert!(
        snapshot
            .recent_spikes
            .iter()
            .any(|spike| spike.operation == "interrupt-ack"),
        "no interrupt-ack spike: {:?}",
        snapshot.recent_spikes
    );
}

// ---------------------------------------------------------------------------
// Restart resumability (CONVENTIONS §3 r6, §9)
// ---------------------------------------------------------------------------

/// Metrics survive daemon death: after `abandon()` (kill -9) and a fresh
/// Supervisor over the same tree + sled dir, the first life's numbers are
/// intact, and the restarted incarnation accumulates under its OWN session
/// key while the agent rollup grows.
#[tokio::test]
async fn metrics_survive_daemon_death_and_accumulate_per_incarnation() {
    let f = fixture(|_| {});
    let script = f.write_script(
        "restart",
        json!({ "usage_sequence": [usage(11, 22, 0.000005)] }),
    );
    let first = f.start(&script, BRIEF).await;
    wait_for("idle (life 0)", || status(&f) == SessionStatus::Idle).await;
    let key0 = f.session_key(first);
    wait_for("life 0 metrics in sled", || {
        f.telemetry.session_tokens(key0).unwrap().is_some()
    })
    .await;

    // kill -9 the daemon WITH the session alive: no charter writes, no
    // graceful sink finish — the stale `idle` status is left behind for
    // `recover()` (CONVENTIONS §3 r6).
    f.supervisor.abandon();
    // The sink task's channel closed with the aborted life task, so it
    // drains and checkpoints on its own.
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        f.telemetry.session_tokens(key0).unwrap(),
        Some(TokenTally {
            input_tokens: 11,
            output_tokens: 22,
        }),
        "metrics lost across a kill -9"
    );
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor2 = Supervisor::new(
        f.tree.clone(),
        f.config.clone(),
        spawner,
        Arc::clone(&f.telemetry),
    );
    let healed = supervisor2.recover().unwrap();
    assert_eq!(healed, vec![f.uid]);

    // Life 0's metrics are still exactly what was checkpointed.
    let after_restart = supervisor2.metrics_for_uid(f.uid, 10).unwrap();
    assert_eq!(after_restart.totals.input_tokens, 11);
    assert_eq!(after_restart.totals.output_tokens, 22);
    assert_eq!(after_restart.totals.cost_micro_usd, 5);

    // Operator restart → incarnation 1 → its own session key.
    let second = supervisor2
        .start_session(&f.parcel(&script), BRIEF, &Secrets::default())
        .await
        .unwrap();
    assert_eq!(second.incarnation, 1);
    wait_for("idle (life 1)", || {
        status_of(&supervisor2, f.uid) == SessionStatus::Idle
    })
    .await;
    supervisor2.stop_session(f.uid).await.unwrap();

    let key1 = SessionKey::new(COMMISSION, f.uid, second.as_u64());
    assert_ne!(key0, key1, "incarnations must not share a session key");
    assert_eq!(
        f.telemetry.session_tokens(key1).unwrap(),
        Some(TokenTally {
            input_tokens: 11,
            output_tokens: 22,
        })
    );
    let rolled = supervisor2.metrics_for_uid(f.uid, 20).unwrap();
    assert_eq!(rolled.sessions.len(), 2, "{:?}", rolled.sessions);
    assert_eq!(rolled.totals.input_tokens, 22);
    assert_eq!(rolled.totals.output_tokens, 44);
    assert_eq!(rolled.totals.cost_micro_usd, 10);
    // The commission rollup agrees with the per-uid view.
    let commission = supervisor2.metrics_for_commission(COMMISSION, 20).unwrap();
    assert_eq!(commission.totals.input_tokens, 22);
}

/// A crash mid-run still records the spans that were open when the process
/// died — [`finish`](hamlet_daemon::metrics::StreamExtractor::finish) runs
/// on every life-end path, not just clean stops.
#[tokio::test]
async fn crash_mid_task_still_records_a_summary() {
    let f = fixture(|config| {
        config.sessions.latency_spike_ms = 1;
        config.sessions.restart.max_restarts = 0; // no restart noise
    });
    let script = f.write_script(
        "crash",
        json!({
            // Die four frames into the run (probe response, prompt
            // response, agent_start, turn_start) with 30 ms between frames,
            // so the task span is open AND over the 1 ms threshold.
            "crash_after_frames": 4,
            "event_delay_ms": 30,
            "crash_exit_code": 70,
            "usage_sequence": [usage(1, 1, 0.0)],
        }),
    );
    let session = f.start(&script, BRIEF).await;
    wait_for("crashed", || status(&f) == SessionStatus::Crashed).await;
    wait_for("summary written", || {
        !details(&f.telemetry, f.uid, "session-stream-summary").is_empty()
    })
    .await;

    let detail = summary(&f.telemetry, f.uid);
    assert!(field(&detail, "frames") > 0, "{detail}");
    // The task was in flight when it died: the open span is a spike.
    let snapshot = f
        .telemetry
        .metrics_for(MetricsScope::Session(f.session_key(session)), 50)
        .unwrap();
    assert!(
        snapshot
            .recent_spikes
            .iter()
            .any(|spike| spike.operation == "agent-task"),
        "no agent-task spike for the killed run: {:?}",
        snapshot.recent_spikes
    );
    assert!(
        events(&f.telemetry, f.uid).contains(&"session-crashed".to_owned()),
        "{:?}",
        events(&f.telemetry, f.uid)
    );
}

/// Exact accounting through a **crash**: the agent dies mid-flood, and every
/// record it managed to emit must still be folded, with the task span that
/// was open at death closed and scored by `finish()`.
///
/// Not a QA05-02 race guard — with a flood this size the daemon reaches EOF
/// first and takes the (already-correct) `Step::Eof` arm, so this passes with
/// or without the exit-drain fix. The race itself is pinned by the
/// un-ignored `repro_qa05_02_stop_midrun_loses_telemetry_tail`, which shares
/// the very same `Step::Exit` arm a crash takes; what this test pins is the
/// crash-path *arithmetic* (frame budget → exact tokens/cost) and the
/// open-span closure.
#[tokio::test]
async fn crash_mid_flood_accounts_every_frame_and_closes_the_span() {
    let f = fixture(|config| {
        config.sessions.latency_spike_ms = 1;
        config.sessions.restart.max_restarts = 0; // terminal: one summary
    });
    // Frame budget: 1 probe response + 1 prompt response + agent_start +
    // turn_start = 4, then cumulative message_updates until the scripted
    // crash fires after record 1000 → 996 usage snapshots, so the final
    // cumulative snapshot is exactly 996 tokens in / 996 out / 996 micro-USD.
    let script = f.write_script(
        "crash-tail",
        json!({
            "flood_frames": 2_000,
            "flood_usage": usage(1, 1, 0.000001),
            "crash_after_frames": 1_000,
            "crash_exit_code": 70,
        }),
    );
    let session = f.start(&script, BRIEF).await;
    wait_for("crashed", || status(&f) == SessionStatus::Crashed).await;
    wait_for("summary written", || {
        !details(&f.telemetry, f.uid, "session-stream-summary").is_empty()
    })
    .await;

    let detail = summary(&f.telemetry, f.uid);
    assert_eq!(field(&detail, "frames"), 1_000, "tail lost: {detail}");
    assert_eq!(field(&detail, "tokens_in"), 996, "tail lost: {detail}");
    assert_eq!(field(&detail, "tokens_out"), 996, "tail lost: {detail}");
    assert_eq!(field(&detail, "cost_micro_usd"), 996, "{detail}");
    assert_eq!(field(&detail, "dropped"), 0, "{detail}");

    // What landed in sled must equal what the stream carried.
    let snapshot = f
        .telemetry
        .metrics_for(MetricsScope::Session(f.session_key(session)), 50)
        .unwrap();
    assert_eq!(snapshot.totals.input_tokens, 996);
    assert_eq!(snapshot.totals.output_tokens, 996);
    assert_eq!(snapshot.totals.cost_micro_usd, 996);
    // The run was open when the process died: `finish` closed and scored it.
    assert!(
        snapshot
            .recent_spikes
            .iter()
            .any(|spike| spike.operation == "agent-task"),
        "no agent-task spike for the crashed run: {:?}",
        snapshot.recent_spikes
    );
    assert!(
        events(&f.telemetry, f.uid).contains(&"session-permanently-crashed".to_owned()),
        "{:?}",
        events(&f.telemetry, f.uid)
    );
}

/// Live query surface, part 2: totals must be exact **while the session is
/// still alive and idle** — no `stop_session`.
///
/// Regression for the flush-retry tick. Pushing is non-blocking (telemetry
/// may never stall the delivery writer), so a full observation channel
/// leaves accounting coalesced in the handle; retrying only when the *next
/// frame* arrives meant a stream that went quiet at a task boundary kept
/// publishing a stale total to `metrics_for` until the life ended. Observed
/// as a ~9% flake across QA's flood scenarios (e.g. 4454 of 4500 tokens
/// visible at idle; the missing tail appeared the moment `finish()` ran).
///
/// Honest scope: the stale-tail window needs the *sink* to be descheduled
/// while the producer finishes, so it reproduces under parallel load rather
/// than from flood volume alone — this test asserts the product property
/// (exact live totals at a boundary) but is not by itself a deterministic
/// guard. Those are `metrics::tests::pending_state_is_visible_and_
/// retry_flush_publishes_it` (mechanism) and the `e2e-03-adversarial.sh` T4
/// ratchet (wiring), plus the QA flood scenarios that surfaced it
/// (HANDOFF finding QA05-04).
#[tokio::test]
async fn live_totals_are_exact_at_a_boundary_without_stopping() {
    let f = fixture(|_| {});
    let script = f.write_script(
        "live",
        json!({
            "flood_frames": 6_000,
            "flood_usage": usage(1, 1, 0.000001),
        }),
    );
    let session = f.start(&script, BRIEF).await;
    wait_for("idle after the flood", || status(&f) == SessionStatus::Idle).await;

    let key = f.session_key(session);
    let exact = TokenTally {
        input_tokens: 6_000,
        output_tokens: 6_000,
    };
    // The session is live and quiet: the live surface must already be exact.
    wait_for("live totals exact without stopping", || {
        f.telemetry.session_tokens(key).unwrap() == Some(exact)
    })
    .await;
    assert_eq!(f.telemetry.session_cost_micro_usd(key).unwrap(), 6_000);

    let snapshot = f
        .supervisor
        .metrics_for_session(session, COMMISSION, 10)
        .unwrap();
    assert_eq!(snapshot.totals.input_tokens, 6_000);
    assert_eq!(snapshot.totals.cost_micro_usd, 6_000);

    // Stopping must not change the totals — nothing was left unpublished.
    f.supervisor.stop_session(f.uid).await.unwrap();
    let detail = summary(&f.telemetry, f.uid);
    assert_eq!(field(&detail, "tokens_in"), 6_000, "{detail}");
    assert_eq!(field(&detail, "dropped"), 0, "{detail}");
    assert_eq!(
        f.telemetry.session_tokens(key).unwrap(),
        Some(exact),
        "finish() published something the live view had not"
    );
}
