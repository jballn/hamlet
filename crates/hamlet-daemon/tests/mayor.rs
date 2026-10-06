//! Mayor lifecycle integration tests (Sprint 05 step 02; CONVENTIONS §9).
//!
//! FakeAgent-mayor: the scripted pi stand-in (`HostProcessSpawner` over
//! `CARGO_BIN_EXE_fakeagent`) replaces pi, so the whole wake → packet →
//! single-turn → sleep chain runs against a real tempdir `.hamlet/` tree and
//! a real child process, with no podman anywhere.
//!
//! What these pin:
//!
//! - provisioning: the manor is parcel-shaped for UID 1000, `MAYOR.md` is
//!   seeded from the committed template exactly once and adopted forever,
//!   and the container argv is the SPEC §4 Mayor mount table (no git bind);
//! - **the packet is the turn**: the bytes pi receives are byte-equal to the
//!   rendered context packet (golden, assembled independently here);
//! - **sleep is zero-token**: after the turn the process is gone, nothing
//!   more is written to the session, and the engine reports itself asleep;
//! - merging: five mails arriving together become ONE wake carrying five
//!   artifacts, not five turns;
//! - serialization: under a concurrent event storm at most one Mayor process
//!   is ever alive, and every event is evaluated exactly once;
//! - boundedness: the queue refuses at its cap instead of growing;
//! - restart-resumability: an orphaned live charter record is healed by
//!   `recover()` and the next wake resumes on a NEW incarnation.

use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::decree::TreeRelPath;
use hamlet_core::dispatch::{file_name as inbox_file_name, InboxResponse, DISPATCH_VERSION};
use hamlet_core::mail::{Mail, MailHeader, MailSender, SPOOL_EXT};
use hamlet_core::packet::{EventArtifact, EventKind, StateGrid, WakeReason};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{SessionId, SessionRecord, SessionStatus, Usage};
use hamlet_core::slots::{SlotStatus, MAYOR_UID};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::hostperm::{MODE_MAIL_SPOOL, MODE_PARCEL_DOC, MODE_PARCEL_HOME};
use hamlet_daemon::mayor::{
    mayor_identity, MayorEngine, MayorError, WakeEvent, WakeOutcome, MAYOR_MD_TEMPLATE,
};
use hamlet_daemon::podman::SessionExecSpec;
use hamlet_daemon::podman::{BoxFuture, ExecHandle, FakeRuntime, PodmanError};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner};

// ---------------------------------------------------------------------------
// Spawner instrumentation: "one evaluation at a time" is a process-level
// claim, so it is measured at the process level.
// ---------------------------------------------------------------------------

struct CountingSpawner {
    inner: HostProcessSpawner,
    live: Arc<AtomicUsize>,
    max_live: Arc<AtomicUsize>,
}

impl CountingSpawner {
    fn new(max_live: &Arc<AtomicUsize>) -> (Self, Arc<AtomicUsize>) {
        let live = Arc::new(AtomicUsize::new(0));
        (
            Self {
                inner: HostProcessSpawner::new(env!("CARGO_BIN_EXE_fakeagent")),
                live: Arc::clone(&live),
                max_live: Arc::clone(max_live),
            },
            live,
        )
    }
}

/// Decrements the live-process gauge exactly once, when the engine is done
/// with the process (after `wait`).
struct CountingHandle {
    inner: Box<dyn ExecHandle>,
    live: Arc<AtomicUsize>,
}

impl ExecHandle for CountingHandle {
    fn take_pipes(&mut self) -> Option<hamlet_daemon::podman::ExecPipes> {
        self.inner.take_pipes()
    }
    fn kill(&mut self) -> Result<(), PodmanError> {
        self.inner.kill()
    }
    fn wait<'a>(&'a mut self) -> BoxFuture<'a, Result<i32, PodmanError>> {
        let inner = &mut self.inner;
        let live = Arc::clone(&self.live);
        Box::pin(async move {
            let out = inner.wait().await;
            live.fetch_sub(1, Ordering::SeqCst);
            out
        })
    }
}

impl SessionSpawner for CountingSpawner {
    fn spawn_session<'a>(
        &'a self,
        spec: &'a SessionExecSpec,
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        Box::pin(async move {
            let handle = self.inner.spawn_session(spec).await?;
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_live.fetch_max(now, Ordering::SeqCst);
            Ok(Box::new(CountingHandle {
                inner: handle,
                live: Arc::clone(&self.live),
            }) as Box<dyn ExecHandle>)
        })
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn name(value: &str) -> SafeName {
    sanitize_name(value).expect("test name must sanitize")
}

fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Every stdin record the FakeAgent logged, parsed.
fn logged(log: &std::path::Path) -> Vec<serde_json::Value> {
    let text = fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The prompt messages pi received, in order.
fn prompts(log: &std::path::Path) -> Vec<String> {
    logged(log)
        .into_iter()
        .filter(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("prompt"))
        .filter_map(|v| {
            v.get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

fn config_with(depth: usize) -> Config {
    let mut config = Config::default();
    config.mayor.wake_queue_depth = depth;
    config.sessions.ready_timeout_ms = 10_000;
    config
}

struct Fixture {
    tree: HamletTree,
    runtime: Arc<FakeRuntime>,
    engine: Arc<MayorEngine>,
    script_log: std::path::PathBuf,
    max_live: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    async fn new(config: Config, event_delay_ms: u64) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        Charter::empty().save_atomic(&tree.charter()).unwrap();

        let runtime = Arc::new(FakeRuntime::new());
        let provisioner = hamlet_daemon::mayor::ManorProvisioner::new(
            tree.clone(),
            config.clone(),
            Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
        );
        let blueprint = Blueprint::default_village_alpine().unwrap();
        provisioner
            .provision(&blueprint)
            .await
            .expect("manor provisioning must succeed");

        let script_log = tmp.path().join("fakeagent-mayor.log");
        let script = tmp.path().join("fakeagent-mayor.json");
        fs::write(
            &script,
            serde_json::json!({
                "log": script_log.to_string_lossy(),
                "event_delay_ms": event_delay_ms,
                "usage": { "input": 11, "output": 7, "totalTokens": 18 },
            })
            .to_string(),
        )
        .unwrap();

        let max_live = Arc::new(AtomicUsize::new(0));
        let (spawner, live) = CountingSpawner::new(&max_live);
        let spawner = Arc::new(spawner);
        let engine = MayorEngine::new(tree.clone(), config, spawner).with_env(BTreeMap::from([(
            "FAKEAGENT_SCRIPT".to_owned(),
            script.to_string_lossy().into_owned(),
        )]));
        Self {
            tree,
            runtime,
            engine: Arc::new(engine),
            script_log,
            live: Arc::clone(&live),
            max_live,
            _tmp: tmp,
        }
    }

    /// Write a real spool file and build the wake from it, exactly as the
    /// daemon's watcher does.
    fn write_mail(&self, seq: u64, subject: &str, body: &str) -> WakeEvent {
        let file = format!("0000000000{seq:02}-1001-ab12.{SPOOL_EXT}");
        let spool = self.tree.courier_mail_recipient(&name("1000"));
        fs::create_dir_all(&spool).unwrap();
        let mail = Mail {
            header: MailHeader {
                from: MailSender::Uid(1001),
                to: MAYOR_UID,
                subject: subject.to_owned(),
                date: "1970-01-01T00:00:00Z".to_owned(),
            },
            body: body.to_owned(),
        };
        fs::write(spool.join(&file), mail.render()).unwrap();
        let event = WakeEvent::from_mail_spool(&self.tree, &spool.join(&file))
            .expect("the spool file the daemon just wrote must load");
        assert_eq!(
            event.kind(),
            &EventKind::MailReceived {
                from: MailSender::Uid(1001),
                subject: subject.to_owned(),
                spool: TreeRelPath::parse(&format!("courier/mail/1000/{file}")).unwrap(),
            }
        );
        event
    }
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manor_provisioning_is_parcel_shaped_for_uid_1000() {
    let f = Fixture::new(config_with(16), 0).await;
    assert!(f.tree.manor().is_dir(), "keep/census/manor/ exists");
    assert!(f.tree.manor_home().is_dir(), "manor home exists");
    assert!(
        f.tree.manor_decrees().is_dir(),
        "decrees/ is static (ADR-0026 cl. 2 — a fresh-dir race must not swallow the first decree)"
    );

    // MAYOR.md is seeded from the committed template, byte-identical.
    let seeded = fs::read_to_string(f.tree.manor_mayor_md()).unwrap();
    assert_eq!(seeded, MAYOR_MD_TEMPLATE);
    assert_eq!(
        seeded,
        fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../defaults/MAYOR.md")
        )
        .unwrap()
    );
    assert_eq!(mode_of(&f.tree.manor_mayor_md()), MODE_PARCEL_DOC);
    assert!(seeded.contains("decree_id = d-<seq:012>-<digest16>"));

    // Host permission braces (ADR-0011 cl. 3): 0700 manor home + mail spool.
    assert_eq!(mode_of(&f.tree.manor_home()), MODE_PARCEL_HOME);
    assert_eq!(
        mode_of(&f.tree.courier_mail_recipient(&name("1000"))),
        MODE_MAIL_SPOOL
    );

    // Charter: the permanent trunk slot under identity `manor` (SPEC §4).
    let charter = Charter::load(&f.tree.charter()).unwrap();
    let slot = charter.slots.get(&MAYOR_UID).expect("slot 1000 reserved");
    assert_eq!(slot.identity, mayor_identity().unwrap());
    assert_eq!(slot.status, SlotStatus::Active);

    // Container argv is the SPEC §4 Mayor mount table — no git bind (the
    // Mayor is multi-commission; diffs arrive inside packets).
    let run_argv = f
        .runtime
        .argv_log()
        .into_iter()
        .find(|argv| argv.get(1).map(String::as_str) == Some("run"))
        .expect("a podman run was issued");
    let joined = run_argv.join(" ");
    assert!(
        joined.contains("keep/census/manor/home:/home/1000/"),
        "{joined}"
    );
    assert!(joined.contains("courier/mail:/var/mail/"), "{joined}");
    assert!(joined.contains("courier/freight:/var/freight/"), "{joined}");
    assert!(
        joined.contains("market/pkgcache:/var/pkgcache/"),
        "{joined}"
    );
    assert!(
        joined.contains("tavern/gossip:/var/context/:ro"),
        "{joined}"
    );
    assert!(
        !joined.contains("/var/git"),
        "no git bind for the Mayor: {joined}"
    );
    assert!(joined.contains("--user 1000:1000"), "{joined}");
}

/// QA04-23: the manor home brace must reach `home/decrees/`, and it must
/// land **before** the container starts.
///
/// A non-recursive `unshare chown` on the home left the daemon-created
/// `decrees/` daemon-owned. Nothing on the host looks wrong — the daemon can
/// always write there — but inside the container the Mayor owns `/home/1000`
/// and not `/home/1000/decrees`, so every decree it authored died with
/// `Permission denied` (the guarded battery's claim 1). The host is
/// exonerated: same mount, `--user 1000:1000`, the hand-repro writes fine
/// once the child is owned too.
#[tokio::test]
async fn manor_home_brace_reaches_decrees_and_precedes_the_container() {
    let f = Fixture::new(config_with(16), 0).await;

    // The watched decree directory carries the home's privacy explicitly.
    assert_eq!(mode_of(&f.tree.manor_decrees()), MODE_PARCEL_HOME);

    let log = f.runtime.argv_log();
    let brace = log
        .iter()
        .position(|argv| {
            argv.get(1).map(String::as_str) == Some("unshare")
                && argv.get(2).map(String::as_str) == Some("chown")
                // `-RP`, not a bare `-R`: recurse, and never dereference a
                // symlink in a tree the Mayor can write to (ADR-0034 cl. 5).
                && argv.get(3).map(String::as_str) == Some("-RP")
                && argv.get(4).map(String::as_str) == Some("1000:1000")
                && argv.last().map(|p| p.ends_with("keep/census/manor/home")) == Some(true)
        })
        .unwrap_or_else(|| panic!("no recursive manor-home chown in {log:#?}"));
    let start = log
        .iter()
        .position(|argv| argv.get(1).map(String::as_str) == Some("run"))
        .unwrap_or_else(|| panic!("no podman run in {log:#?}"));
    assert!(
        brace < start,
        "the brace must land before the Mayor container starts (ADR-0011 cl. 2)"
    );

    // Re-converging re-asserts the same brace: idempotent, not one-shot.
    let before = f.runtime.argv_log().len();
    hamlet_daemon::mayor::ManorProvisioner::new(
        f.tree.clone(),
        Config::default(),
        Arc::clone(&f.runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    )
    .provision(&Blueprint::default_village_alpine().unwrap())
    .await
    .unwrap();
    assert!(
        f.runtime.argv_log()[before..].iter().any(|argv| {
            argv.get(3).map(String::as_str) == Some("-RP")
                && argv.last().map(|p| p.ends_with("keep/census/manor/home")) == Some(true)
        }),
        "re-provisioning re-asserts the recursive brace"
    );
}

#[tokio::test]
async fn manor_provisioning_adopts_an_edited_mayor_md_and_is_idempotent() {
    let f = Fixture::new(config_with(16), 0).await;
    // The trunk prompt is operator-owned once written (SPEC §2).
    let edited = format!("{MAYOR_MD_TEMPLATE}\n<!-- operator edit -->\n");
    fs::write(f.tree.manor_mayor_md(), &edited).unwrap();

    let provisioner = hamlet_daemon::mayor::ManorProvisioner::new(
        f.tree.clone(),
        Config::default(),
        Arc::clone(&f.runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    let report = provisioner
        .provision(&Blueprint::default_village_alpine().unwrap())
        .await
        .unwrap();
    assert_eq!(report.uid, MAYOR_UID);
    assert!(
        !report.seeded_mayor_md,
        "an existing regular MAYOR.md is adopted, never clobbered"
    );
    assert_eq!(
        fs::read_to_string(f.tree.manor_mayor_md()).unwrap(),
        edited,
        "the operator's MAYOR.md survives re-provisioning"
    );
    // Slot reservation is idempotent per identity (SPEC §4: one identity, one
    // slot, forever).
    let charter = Charter::load(&f.tree.charter()).unwrap();
    assert_eq!(charter.slots.len(), 1);
    assert!(charter.slots.contains_key(&MAYOR_UID));
}

// ---------------------------------------------------------------------------
// The wake → packet → single turn → sleep chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_becomes_exactly_one_turn_with_the_exact_packet_then_sleeps() {
    let f = Fixture::new(config_with(16), 0).await;
    let event = f.write_mail(
        7,
        "Freight report ready",
        "artifact ada-1 landed in /var/freight",
    );

    let outcome = f.engine.wake(event).await.unwrap();
    let WakeOutcome::Evaluated(turn) = outcome else {
        panic!("a lone wake must evaluate, not coalesce: {outcome:?}");
    };
    assert_eq!(turn.end, hamlet_daemon::mayor::TurnEnd::Settled);
    assert!(turn.is_settled());
    assert_eq!(turn.wake_reason, WakeReason::MailToMayor);
    assert_eq!(turn.events, 1, "one unmerged event");
    assert_eq!(turn.session, SessionId::first(MAYOR_UID).unwrap());
    assert_eq!(turn.malformed, 0);
    assert_eq!(
        turn.usage,
        Some(Usage {
            input: 11,
            output: 7,
            total_tokens: 18,
            ..Usage::default()
        }),
        "the turn's token cost is observable, and only while a turn runs"
    );
    assert_eq!(turn.exit_code, 0);

    // --- golden: the bytes pi received ARE the rendered packet ---------
    let delivered = prompts(&f.script_log);
    assert_eq!(delivered.len(), 1, "exactly one prompt per wake");
    let charter = Charter::load(&f.tree.charter()).unwrap();
    // The grid in the packet is the snapshot taken BEFORE this turn's own
    // session record exists (the packet is built first, then the one-shot
    // process is spawned) — so the Mayor never sees itself mid-turn. A
    // *later* wake does see the row; the restart test pins that.
    let mut snapshot = charter.clone();
    snapshot.sessions.clear();
    let grid = StateGrid::from_charter(&snapshot);
    let expected = format!(
        "=== HAMLET EXECUTIVE PACKET v1 ===\n\
wake_reason: mail_to_mayor\n\
\n\
--- MAYOR.md ---\n\
{M}\n\
\n\
--- CHARTER STATE GRID ---\n\
{grid}\n\
--- EVENT ---\n\
kind: mail_to_mayor\n\
from: 1001\n\
subject: Freight report ready\n\
spool: courier/mail/1000/000000000007-1001-ab12.mail\n\
\n\
--- ARTIFACT BODY ---\n\
From: 1001\n\
To: 1000\n\
Subject: Freight report ready\n\
Date: 1970-01-01T00:00:00Z\n\
\n\
artifact ada-1 landed in /var/freight\n\
\n\
--- DECREE PROTOCOL ---\n\
decree_dir: /home/1000/decrees\n\
file_name: <decree_id>.json\n\
decree_id: d-<seq:012>-<digest16>\n\
next_decree_seq: 1\n\
digest: fnv1a64 hex of the canonical body with decree_id removed\n\
seq_rule: must equal next_decree_seq; the charter counter orders decrees, never a clock\n\
ack: the daemon writes <decree_id>.ack next to the decree (done | rejected+reason)\n\
emission: one turn, one decision — write decree files only, never podman or host commands\n\
\n\
=== END OF PACKET ===\n",
        M = MAYOR_MD_TEMPLATE.trim_end_matches('\n'),
        grid = grid.render(),
    );
    assert_eq!(delivered[0], expected, "the prompt IS the packet, verbatim");

    // Two writes to the session's stdin, one turn: the readiness probe and
    // the packet. No follow-up prompt, no steering, no drip.
    let records = logged(&f.script_log);
    assert_eq!(records.len(), 2, "probe + exactly one prompt");
    assert_eq!(records[0]["type"], "get_state");
    assert_eq!(records[1]["type"], "prompt");

    // --- the tree shows the whole life ---------------------------------
    let record = charter.sessions.get(&MAYOR_UID).copied().unwrap();
    assert_eq!(record.id, SessionId::first(MAYOR_UID).unwrap());
    assert_eq!(
        record.status,
        hamlet_core::session::SessionStatus::Stopped,
        "one incarnation per wake; no live session between events"
    );

    // --- zero-token sleep ----------------------------------------------
    let sleep = f.engine.sleep_state();
    assert!(!sleep.awake, "nothing holds the turn lock after a wake");
    assert_eq!(sleep.pending, 0);
    assert_eq!(sleep.turns, 1);
    let before = fs::read_to_string(&f.script_log).unwrap().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fs::read_to_string(&f.script_log).unwrap().len(),
        before,
        "between events nothing is written to the Mayor's session — the sleep is idle-silent"
    );
    assert_eq!(
        f.live.load(Ordering::SeqCst),
        0,
        "the process is gone: sleep holds no process, so it cannot consume tokens"
    );
    assert_eq!(prompts(&f.script_log).len(), 1);
}

#[tokio::test]
async fn five_mails_are_one_wake_with_five_artifacts() {
    let f = Fixture::new(config_with(16), 60).await;
    let mut tasks = Vec::new();
    for seq in 1..=5u64 {
        let event = f.write_mail(seq, &format!("report {seq}"), &format!("body {seq}"));
        let engine = Arc::clone(&f.engine);
        tasks.push(tokio::spawn(async move { engine.wake(event).await }));
    }
    let mut evaluated = 0usize;
    let mut coalesced = 0usize;
    for task in tasks {
        match task.await.unwrap().unwrap() {
            WakeOutcome::Evaluated(turn) => {
                assert!(turn.is_settled(), "{turn:?}");
                evaluated += turn.events;
            }
            WakeOutcome::Coalesced => coalesced += 1,
            WakeOutcome::Overflow { cap } => panic!("cap 16 must hold five mails: {cap}"),
        }
    }
    assert_eq!(
        evaluated, 5,
        "every mail is evaluated exactly once — merging never drops one"
    );
    assert!(coalesced <= 4, "coalesced callers waited for the holder");

    // Two turns, five events: the first caller ran alone, everything that
    // arrived during its turn merged into the next packet.
    let delivered = prompts(&f.script_log);
    assert!(
        delivered.len() < 5,
        "five mails must not cost five turns: {} turn(s)",
        delivered.len()
    );
    let covered: usize = delivered
        .iter()
        .map(|p| p.matches("subject: report").count())
        .sum();
    assert_eq!(
        covered, 5,
        "every mail appears exactly once, in exactly one packet"
    );
    let merged: Vec<&String> = delivered
        .iter()
        .filter(|p| p.contains("merged: "))
        .collect();
    assert!(
        !merged.is_empty(),
        "the storm produced a merged batch artifact:\n{}",
        delivered.join("\n----\n")
    );
    for packet in &merged {
        let members = packet.matches("--- artifact ").count();
        assert!(
            members >= 2,
            "a merged packet carries >=2 artifacts: {members}"
        );
        assert!(packet.contains("member 1:\n  kind: mail_to_mayor"));
        assert!(packet.contains("  from: 1001"));
        assert!(packet.contains("wake_reason: mail_to_mayor"));
        // Fenced bodies stay in queue order and verbatim.
        let first = packet.find("Subject: report").unwrap();
        let last = packet.rfind("Subject: report").unwrap();
        assert!(first < last);
    }
    let stats = f.engine.queue_stats();
    assert_eq!(stats.depth, 0, "the queue drains completely");
    assert_eq!(stats.overflowed, 0);
    assert!(
        stats.merged_into_batch > 0,
        "the merge counter is observable telemetry"
    );
}

#[tokio::test]
async fn the_wake_queue_is_bounded_and_refuses_overflow() {
    let f = Fixture::new(config_with(2), 120).await;
    let mut tasks = Vec::new();
    for seq in 1..=6u64 {
        let event = f.write_mail(seq, &format!("m{seq}"), &format!("body {seq}"));
        let engine = Arc::clone(&f.engine);
        tasks.push(tokio::spawn(async move { engine.wake(event).await }));
    }
    let mut overflow = 0usize;
    let mut evaluated = 0usize;
    for task in tasks {
        match task.await.unwrap().unwrap() {
            WakeOutcome::Evaluated(turn) => {
                evaluated += turn.events;
                assert!(turn.events <= 2, "a batch never exceeds the cap");
            }
            WakeOutcome::Coalesced => {}
            WakeOutcome::Overflow { cap } => {
                assert_eq!(cap, 2);
                overflow += 1;
            }
        }
    }
    let stats = f.engine.queue_stats();
    assert_eq!(stats.cap, 2);
    assert!(stats.depth <= 2, "the queue never grows past its bound");
    assert_eq!(
        stats.overflowed as usize, overflow,
        "refusals are counted telemetry, never silent drops"
    );
    assert!(
        evaluated + overflow >= 5,
        "at most the refused events were lost: evaluated={evaluated} overflow={overflow}"
    );
}

#[tokio::test]
async fn event_storm_never_runs_two_evaluations_at_once() {
    let f = Fixture::new(config_with(64), 20).await;
    let mut tasks = Vec::new();
    for seq in 1..=24u64 {
        let event = f.write_mail(seq, &format!("storm {seq}"), &format!("body {seq}"));
        let engine = Arc::clone(&f.engine);
        tasks.push(tokio::spawn(async move { engine.wake(event).await }));
    }
    let mut events = 0usize;
    for task in tasks {
        if let WakeOutcome::Evaluated(turn) = task.await.unwrap().unwrap() {
            assert!(turn.is_settled(), "{turn:?}");
            events += turn.events;
        }
    }
    assert_eq!(events, 24, "every event evaluated exactly once");
    assert_eq!(
        f.max_live.load(Ordering::SeqCst),
        1,
        "the Mayor is serialized: never two processes alive at once"
    );
    let charter = Charter::load(&f.tree.charter()).unwrap();
    let record = charter.sessions.get(&MAYOR_UID).copied().unwrap();
    assert_eq!(
        record.status,
        SessionStatus::Stopped,
        "the last turn ended; the Mayor is asleep"
    );
    assert!(
        record.id.incarnation >= 1,
        "each turn is its own incarnation: {}",
        record.id.token()
    );
}

#[tokio::test]
async fn restart_heals_an_orphaned_mayor_session_and_resumes_on_a_new_incarnation() {
    let f = Fixture::new(config_with(16), 0).await;
    let event = f.write_mail(1, "first", "body");
    let WakeOutcome::Evaluated(first) = f.engine.wake(event).await.unwrap() else {
        panic!("a lone wake evaluates");
    };

    // Simulate `kill -9`: the charter still claims the session is alive (what
    // a SIGKILLed daemon leaves behind) and a fresh engine owns the tree.
    let mut charter = Charter::load(&f.tree.charter()).unwrap();
    charter.sessions.insert(
        MAYOR_UID,
        SessionRecord {
            id: first.session,
            status: SessionStatus::Busy,
            restart_count: 0,
        },
    );
    charter.save_atomic(&f.tree.charter()).unwrap();

    let max_live = Arc::new(AtomicUsize::new(0));
    let restarted = MayorEngine::new(
        f.tree.clone(),
        config_with(16),
        Arc::new(CountingSpawner::new(&max_live).0),
    )
    .with_env(BTreeMap::from([(
        "FAKEAGENT_SCRIPT".to_owned(),
        f.script_log
            .parent()
            .unwrap()
            .join("fakeagent-mayor.json")
            .to_string_lossy()
            .into_owned(),
    )]));
    assert_eq!(restarted.recover().unwrap(), vec![MAYOR_UID]);
    assert_eq!(
        Charter::load(&f.tree.charter())
            .unwrap()
            .sessions
            .get(&MAYOR_UID)
            .unwrap()
            .status,
        SessionStatus::Crashed,
        "an orphaned live record is healed, never trusted (CONVENTIONS §3 r6)"
    );
    // A second recover is a no-op: healing is idempotent.
    assert!(restarted.recover().unwrap().is_empty());

    let event = f.write_mail(2, "second", "body two");
    let WakeOutcome::Evaluated(second) = restarted.wake(event).await.unwrap() else {
        panic!("a lone wake evaluates");
    };
    assert_eq!(
        second.session.incarnation,
        first.session.incarnation + 1,
        "the next wake is a NEW incarnation — a resumed pi session would carry history"
    );
    let delivered = prompts(&f.script_log);
    assert_eq!(delivered.len(), 2);
    assert!(
        !delivered[1].contains("subject: first"),
        "the new life carries no thread from the old one"
    );
}

// ---------------------------------------------------------------------------
// Packet guard rails
// ---------------------------------------------------------------------------

#[tokio::test]
async fn packet_building_refuses_a_missing_or_empty_mayor_md() {
    let f = Fixture::new(config_with(16), 0).await;
    let builder = f.engine.packets().clone();
    let batch = hamlet_daemon::mayor::WakeBatch {
        seqs: vec![1],
        artifact: EventArtifact::new(EventKind::CommissionCreated { commission: 1 }, "brief\n")
            .unwrap(),
    };
    let rendered = builder.render(&batch).unwrap();
    assert!(rendered
        .text
        .starts_with("=== HAMLET EXECUTIVE PACKET v1 ==="));
    assert!(rendered
        .text
        .contains(MAYOR_MD_TEMPLATE.trim_end_matches('\n')));

    fs::remove_file(f.tree.manor_mayor_md()).unwrap();
    assert!(matches!(
        builder.render(&batch),
        Err(MayorError::MissingMayorMd { .. })
    ));

    fs::write(f.tree.manor_mayor_md(), "   \n").unwrap();
    // an empty trunk prompt is refused, never substituted
    assert!(matches!(builder.render(&batch), Err(MayorError::Packet(_))));

    // A symlink where MAYOR.md belongs is refused, never followed (ADR-0007).
    let elsewhere = f.tree.manor().join("elsewhere.md");
    fs::write(&elsewhere, "# not the prompt\n").unwrap();
    fs::remove_file(f.tree.manor_mayor_md()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, f.tree.manor_mayor_md()).unwrap();
    assert!(matches!(
        builder.render(&batch),
        Err(MayorError::NotRegularFile { .. })
    ));
}

// ---------------------------------------------------------------------------
// Wake-hook bridges (fire-and-forget, ADR-0021 cl. 5 / ADR-0022 cl. 6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn freight_and_webhook_hooks_deliver_wakes_from_the_tree() {
    use hamlet_daemon::freight::{FreightEscalation, MayorWake};
    use hamlet_daemon::web::{WebhookEvent, WebhookWake};

    // The webhook bridge is config-driven (ADR-0029 cl. 1): a topic that is
    // not in `webhooks.routes` never reaches the Mayor.
    let mut config = config_with(16);
    config.webhooks.routes.insert(
        "github".to_owned(),
        hamlet_core::config::WebhookRoute {
            dispatch_inbox: true,
            mail_to: vec![],
        },
    );
    let f = Fixture::new(config, 0).await;

    // Freight: an escalation becomes a wake whose artifact is loaded from
    // the audit's own payload.
    let escalation = FreightEscalation {
        owner: 1042,
        quarantined: vec!["exchange".to_owned()],
        audit_id: 1,
    };
    hamlet_daemon::mayor::FreightWakeBridge::new(Arc::clone(&f.engine)).wake(&escalation);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let delivered = prompts(&f.script_log);
    assert_eq!(delivered.len(), 1, "the freight hook fired one turn");
    assert!(delivered[0].contains("wake_reason: freight_violation"));
    assert!(delivered[0].contains("sandbox: exchange"));
    assert!(delivered[0].contains("owner: 1042"));
    assert!(delivered[0].contains("quarantined: exchange"));

    // Webhook: the artifact is the tree's own inbox envelope (status 202).
    let correlation =
        hamlet_core::session::CorrelationId::for_dispatch(&SessionId::first(MAYOR_UID).unwrap(), 1)
            .unwrap();
    let file = inbox_file_name(&correlation);
    let inbox = f.tree.courier_dispatch_inbox().join(&file);
    fs::create_dir_all(inbox.parent().unwrap()).unwrap();
    fs::write(
        &inbox,
        InboxResponse::http(
            correlation.clone(),
            1,
            202,
            &BTreeMap::new(),
            "{\"issue\":\"build failed\"}",
        )
        .render()
        .unwrap(),
    )
    .unwrap();
    assert_eq!(DISPATCH_VERSION, 1);
    hamlet_daemon::mayor::WebhookWakeBridge::new(Arc::clone(&f.engine)).webhook(&WebhookEvent {
        topic: "github".to_owned(),
        correlation,
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    let delivered = prompts(&f.script_log);
    assert_eq!(delivered.len(), 2, "the webhook hook fired one more turn");
    assert!(delivered[1].contains("wake_reason: webhook_event"));
    assert!(delivered[1].contains("topic: github"));
    assert!(delivered[1].contains("status: 202"));
    assert!(delivered[1].contains("issue"));
    assert_eq!(
        f.engine.sleep_state().pending,
        0,
        "both wakes were consumed"
    );
}

// ---------------------------------------------------------------------------
// Crash-wake bridge (config-driven; ADR-0029 cl. 2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crash_wake_is_policy_filtered_and_delivers_the_notice_as_a_packet() {
    use hamlet_daemon::supervise::CrashWake;

    let f = Fixture::new(config_with(4), 0).await;
    let bridge = hamlet_daemon::decrees::CrashWakeBridge::new(Arc::clone(&f.engine));

    // Default policy (`budget_exhausted`): a restartable crash is
    // transparent to the Mayor — the daemon revives it on its own.
    let restartable = hamlet_daemon::supervise::CrashNotice {
        uid: 1042,
        session: SessionId::first(1042).unwrap(),
        restarts: 1,
        permanent: false,
        detail: "exit 1 (restartable)".to_owned(),
    };
    bridge.crashed(&restartable);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        prompts(&f.script_log).len(),
        0,
        "a restartable crash under budget_exhausted never wakes the Mayor"
    );

    // The restart budget is exhausted: the daemon will not revive the agent,
    // so exactly one wake carries the notice as its artifact.
    let permanent = hamlet_daemon::supervise::CrashNotice {
        uid: 1042,
        session: SessionId::first(1042).unwrap(),
        restarts: 3,
        permanent: true,
        detail: "exit 1 (budget exhausted)".to_owned(),
    };
    bridge.crashed(&permanent);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if !prompts(&f.script_log).is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the permanent crash never woke the Mayor"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let delivered = prompts(&f.script_log);
    assert_eq!(delivered.len(), 1, "one wake for one permanent crash");
    assert!(delivered[0].contains("wake_reason: session_crashed"));
    assert!(delivered[0].contains("uid: 1042"));
    assert!(delivered[0].contains("permanent: yes"));
    assert_eq!(f.engine.sleep_state().pending, 0, "the wake was consumed");
}
