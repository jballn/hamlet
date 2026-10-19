//! Adversarial mayor suite — QA prompt 05/04 (SPEC §7; ADR-0026/0027/
//! 0028/0029; ADR-0011; CONVENTIONS §2, §3, §9).
//!
//! The Mayor is the highest-power agent in the village: it writes files that
//! the daemon executes without asking. Everything here is written from the
//! attacker's side — an inbound mail artifact, a non-mayor writer, a torn
//! write, a crash, a storm, a replay — and asserts what the daemon *must*
//! do. FakeAgent is the Mayor (`CARGO_BIN_EXE_fakeagent`); no podman is
//! required anywhere in this file.
//!
//! Findings pinned by name (see `roadmap/sprints/05-mayor-loop/HANDOFF.md`
//! step 04):
//!
//! - **QA05-01** the executor verifies no provenance of the decree file
//!   (no owner check, no openat/`O_NOFOLLOW`); only the 0700 + mount
//!   boundary of the manor home stands between a foreign writer and
//!   `podman`-level execution.
//! - **QA05-02** `write_ack` / `quarantine` write into
//!   `manor/home/decrees/`, which `apply_manor_home` makes 0700 owned by
//!   subuid 1000 — the daemon's own user cannot write there outside
//!   `podman unshare` (ADR-0011 cl. 4). The failure is only *traced*.
//! - **QA05-03** ack-timeout wakes fire once per *process* per decree, not
//!   once per timeout, so a decree that stays unanswered is never re-escalated.
//! - **QA05-04** the ack `atomic_write` uses a **fixed, predictable** temp
//!   name and `create(true).truncate(true)` — which follows a planted
//!   symlink, letting the Mayor clobber any file the daemon can write
//!   (notably `charter.json`).
//! - **QA05-05** quarantine is a destructive `rename`: a torn in-place write
//!   loses the decree permanently.
//! - **QA05-06** the wake queue deduplicates on artifact equality, so two
//!   distinct real-world events that serialize identically are merged
//!   silently (counted, not dropped-without-trace).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{Charter, DecreeStatus};
use hamlet_core::config::Config;
use hamlet_core::decree::{
    Decree, DecreeAck, DecreeId, EscalationNotice, RejectReason, TreeRelPath, DECREE_VERSION,
};
use hamlet_core::mail::{Mail, MailHeader, MailSender, SPOOL_EXT};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::SessionId;
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::decrees::{
    AgentProvisioner, DecreeExecutor, DecreeOutcome, DecreeRefusal, ProvisionOutcome,
    ProvisionRequest,
};
use hamlet_daemon::mayor::{MayorEngine, WakeEvent, WakeOutcome};
use hamlet_daemon::podman::{
    BoxFuture, ExecHandle, FakeRuntime, PodmanError, Runtime, SessionExecSpec,
};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn name(value: &str) -> SafeName {
    sanitize_name(value).expect("test name must sanitize")
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Every stdin record the FakeAgent logged, parsed.
fn logged(log: &Path) -> Vec<serde_json::Value> {
    let text = fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The prompt messages the Mayor's session received, in order.
fn prompts(log: &Path) -> Vec<String> {
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

/// Live **fakeagent** direct children of this test process *for one village*
/// — "is a Mayor process alive?".
///
/// Two filters, both required, because the test binary runs many tests in
/// parallel threads and they all spawn the same `CARGO_BIN_EXE_fakeagent`
/// binary: by argv (otherwise the child list is noise) and by the village
/// root in the child's **environ** (`FAKEAGENT_SCRIPT` lives inside the
/// village's tempdir, so this is an exact ownership tag). Without the second
/// filter the idle-window assertion below compares process-wide pids and goes
/// red whenever a *sibling* test's Mayor happens to exit during the 1.5 s
/// window — a cross-test race that has nothing to do with the invariant under
/// test (reproducible only under some timing/TMPDIR shapes).
fn live_mayor_pids(village_root: &Path) -> Vec<u32> {
    let me = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_string_lossy().parse().ok()?;
            (pid != me).then_some(pid)
        })
        .filter(|pid| {
            // /proc/<pid>/stat: `pid (comm) state ppid …`; comm may contain
            // spaces and parens, so split after the LAST ')'.
            let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            stat.rsplit_once(") ")
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .and_then(|ppid| ppid.parse::<u32>().ok())
                == Some(me)
        })
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .map(|raw| String::from_utf8_lossy(&raw).contains("fakeagent"))
                .unwrap_or(false)
        })
        .filter(|pid| {
            let tag = village_root.to_string_lossy();
            fs::read(format!("/proc/{pid}/environ"))
                .map(|raw| String::from_utf8_lossy(&raw).contains(tag.as_ref()))
                .unwrap_or(false)
        })
        .collect()
}

/// A `SessionSpawner` that records every argv the engine asked for and
/// measures how many Mayor processes were ever alive at once.
struct SpySpawner {
    inner: Arc<HostProcessSpawner>,
    live: Arc<AtomicUsize>,
    max_live: Arc<AtomicUsize>,
    cmds: Arc<Mutex<Vec<Vec<String>>>>,
}

struct SpyHandle {
    inner: Box<dyn ExecHandle>,
    live: Arc<AtomicUsize>,
}

impl ExecHandle for SpyHandle {
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

impl SessionSpawner for SpySpawner {
    fn spawn_session<'a>(
        &'a self,
        spec: &'a SessionExecSpec,
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        let handle = Arc::clone(&self.inner);
        let live = Arc::clone(&self.live);
        let max_live = Arc::clone(&self.max_live);
        let cmds = Arc::clone(&self.cmds);
        let cmd = spec.cmd.clone();
        Box::pin(async move {
            cmds.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(cmd.to_vec());
            let inner = handle.spawn_session(spec).await?;
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            max_live.fetch_max(now, Ordering::SeqCst);
            Ok(Box::new(SpyHandle { inner, live }) as Box<dyn ExecHandle>)
        })
    }
}

/// A village with the manor provisioned, a FakeAgent mayor, and a default
/// decree executor.
struct Village {
    tree: HamletTree,
    config: Config,
    executor: Arc<DecreeExecutor>,
    engine: Arc<MayorEngine>,
    log: PathBuf,
    cmds: Arc<Mutex<Vec<Vec<String>>>>,
    max_live: Arc<AtomicUsize>,
    _tmp: tempfile::TempDir,
}

fn config_with(depth: usize) -> Config {
    let mut config = Config::default();
    config.mayor.wake_queue_depth = depth;
    config.sessions.ready_timeout_ms = 10_000;
    config
}

impl Village {
    async fn new(config: Config) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        Charter::empty().save_atomic(&tree.charter()).unwrap();

        let runtime = Arc::new(FakeRuntime::new());
        hamlet_daemon::mayor::ManorProvisioner::new(
            tree.clone(),
            config.clone(),
            Arc::clone(&runtime) as Arc<dyn Runtime>,
        )
        .provision(&Blueprint::default_village_alpine().unwrap())
        .await
        .expect("manor provisioning must succeed");

        // Evidence an escalation decree may cite.
        let brief = tree.register_brief(&name("scribe"));
        fs::create_dir_all(brief.parent().unwrap()).unwrap();
        fs::write(&brief, "# scribe — the register brief\n").unwrap();

        let log = tmp.path().join("fakeagent-mayor.log");
        let script = tmp.path().join("fakeagent-mayor.json");
        fs::write(
            &script,
            serde_json::json!({ "log": log.to_string_lossy() }).to_string(),
        )
        .unwrap();

        let max_live = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let cmds: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let spawner = Arc::new(SpySpawner {
            inner: Arc::new(HostProcessSpawner::new(env!("CARGO_BIN_EXE_fakeagent"))),
            live: Arc::clone(&live),
            max_live: Arc::clone(&max_live),
            cmds: Arc::clone(&cmds),
        });
        let engine = Arc::new(
            MayorEngine::new(tree.clone(), config.clone(), spawner).with_env(BTreeMap::from([(
                "FAKEAGENT_SCRIPT".to_owned(),
                script.to_string_lossy().into_owned(),
            )])),
        );
        let executor = Arc::new(DecreeExecutor::new(tree.clone(), config.clone()));
        Self {
            tree,
            config,
            executor,
            engine,
            log,
            cmds,
            max_live,
            _tmp: tmp,
        }
    }

    async fn with_delay(depth: usize, event_delay_ms: u64) -> Self {
        let v = Self::new(config_with(depth)).await;
        // Re-point the script at a delayed variant (the FakeAgent reads it at
        // process start, so this is enough for subsequent spawns).
        let script = v.log.parent().unwrap().join("delayed.json");
        fs::write(
            &script,
            serde_json::json!({
                "log": v.log.to_string_lossy(),
                "event_delay_ms": event_delay_ms,
            })
            .to_string(),
        )
        .unwrap();
        let delayed = MayorEngine::new(
            v.tree.clone(),
            v.config.clone(),
            Arc::new(SpySpawner {
                inner: Arc::new(HostProcessSpawner::new(env!("CARGO_BIN_EXE_fakeagent"))),
                live: Arc::new(AtomicUsize::new(0)),
                max_live: Arc::clone(&v.max_live),
                cmds: Arc::clone(&v.cmds),
            }),
        )
        .with_env(BTreeMap::from([(
            "FAKEAGENT_SCRIPT".to_owned(),
            script.to_string_lossy().into_owned(),
        )]));
        Self {
            engine: Arc::new(delayed),
            ..v
        }
    }

    /// Write a real spool file and build the wake from it, exactly as the
    /// daemon's mail bridge does.
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
        WakeEvent::from_mail_spool(&self.tree, &spool.join(&file))
            .expect("the spool file the daemon just wrote must load")
    }
}

fn escalation_decree(seq: u64, summary: &str, evidence: &[&str]) -> Decree {
    let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
    Decree::EscalateToHuman {
        decree_id: placeholder,
        decree_version: DECREE_VERSION,
        evidence_refs: evidence
            .iter()
            .map(|p| TreeRelPath::parse(p).unwrap())
            .collect(),
        summary: summary.to_owned(),
    }
    .stamp(seq)
    .unwrap()
}

fn write_decree(tree: &HamletTree, decree: &Decree) -> PathBuf {
    let path = tree
        .manor_decrees()
        .join(decree.id().decree_file_name().unwrap().as_str());
    fs::write(&path, decree.render().unwrap()).unwrap();
    path
}

fn read_ack(tree: &HamletTree, id: &DecreeId) -> DecreeAck {
    DecreeAck::parse(
        fs::read_to_string(tree.manor_decree_ack(id))
            .unwrap()
            .as_str(),
    )
    .unwrap()
}

fn outbox_files(tree: &HamletTree) -> Vec<PathBuf> {
    let dir = tree.courier_dispatch_outbox();
    fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| dir.join(entry.file_name()))
                .collect()
        })
        .unwrap_or_default()
}

// ===========================================================================
// 1. Decree forgery / injection
// ===========================================================================

/// **QA05-01 — regression: decree provenance is verified, not assumed.**
///
/// The finding: `read_decree` validated only "regular file, size, parse", so
/// a decree directory *replaced by a symlink* into an attacker-controlled
/// tree was read and executed, and the daemon filed an escalation notice for
/// a decree it could not attribute. Today `verify_decree_dir` refuses the
/// whole pass when `manor/home/decrees/` is not a plain directory inside the
/// manor home, or is group/world-writable, and `read_decree` opens every file
/// `O_NOFOLLOW` and requires its owner to be the directory's owner.
///
/// The braces that already held (0700 manor home, no manor mount for
/// subagents) are still asserted — defence in depth is the point, and this
/// test proves the *logic* holds even if a mount ever widens.
#[tokio::test]
async fn qa05_01_a_symlinked_decree_directory_is_refused_whole_pass() {
    let v = Village::new(Config::default()).await;

    // Plant the file from OUTSIDE the manor home: an attacker-controlled
    // directory that merely *replaces* the live decree directory.
    let foreign = v._tmp.path().join("attacker-decrees");
    fs::create_dir_all(&foreign).unwrap();
    let decree = escalation_decree(
        1,
        "written by a non-mayor writer",
        &["guild/register/scribe.md"],
    );
    fs::write(
        foreign.join(decree.id().decree_file_name().unwrap().as_str()),
        decree.render().unwrap(),
    )
    .unwrap();

    // Point the live directory at it.
    let live = v.tree.manor_decrees();
    fs::remove_dir_all(&live).unwrap();
    std::os::unix::fs::symlink(&foreign, &live).unwrap();

    let error = v
        .executor
        .scan()
        .await
        .expect_err("QA05-01: a symlinked decree directory must fail the pass closed");
    assert!(
        matches!(
            error,
            hamlet_daemon::decrees::ExecutorError::Provenance { .. }
        ),
        "the refusal is a typed provenance error, got {error:?}"
    );

    // Nothing happened: no execution, no ack, no escalation notice, no
    // sequence consumed — and the foreign file is untouched.
    assert!(
        !v.tree.manor_decree_ack(decree.id()).exists(),
        "no ack for an unattributable decree"
    );
    assert!(
        !v.tree.manor_escalation_file(decree.id()).exists(),
        "no escalation filed on the Mayor's behalf by a foreign writer"
    );
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert!(charter.decrees.is_empty(), "no sequence consumed");
    assert_eq!(charter.next_decree_seq(), 1);
    assert_eq!(
        fs::read_dir(&foreign).unwrap().count(),
        1,
        "the daemon wrote nothing into a directory it could not vouch for"
    );

    // The two braces that DO hold, asserted so a future regression is loud.
    use hamlet_daemon::hostperm::MODE_PARCEL_HOME;
    assert_eq!(
        mode_of(&v.tree.manor_home()),
        MODE_PARCEL_HOME,
        "0700 manor home (ADR-0011 cl. 3) is the outer provenance boundary"
    );
    let agent_mounts = hamlet_core::mount::MountSet::for_agent(1001, 1).unwrap();
    let rendered = format!("{:?}", agent_mounts);
    assert!(
        !rendered.contains("manor"),
        "no subagent container may reach the manor at all: {rendered}"
    );
}

/// **QA05-01 (b) — a foreign *file* inside a genuine decree directory is
/// refused individually, without quarantine.**
///
/// The directory checks in the test above are the outer wall; this is the
/// inner one: the Mayor's own directory can still hold a file someone else
/// wrote (a shared backup, a stray `rsync`, a mount that widened). The file
/// is left exactly where it is — no ack, no quarantine, no sequence — because
/// the daemon is not entitled to move or answer a file it cannot attribute.
#[tokio::test]
async fn qa05_01_a_foreign_owned_file_in_the_decrees_dir_is_refused() {
    use std::os::unix::fs::{chown, MetadataExt};

    let v = Village::new(Config::default()).await;
    let mine = fs::metadata(v.tree.manor_decrees()).unwrap().uid();
    let decree = escalation_decree(
        1,
        "authored by somebody else",
        &["guild/register/scribe.md"],
    );
    let path = v
        .tree
        .manor_decrees()
        .join(decree.id().decree_file_name().unwrap().as_str());
    fs::write(&path, decree.render().unwrap()).unwrap();

    // Re-own the *file* to a different uid (root in this container can, and
    // that is exactly the shape: same directory, different author). Skip
    // when we are not privileged enough to make the distinction.
    if chown(&path, Some(mine + 1), None).is_err() {
        eprintln!("QA05-01b skipped: cannot change file ownership as this uid");
        return;
    }

    let report = v.executor.scan().await.unwrap();
    assert_eq!(
        report.unattributable(),
        1,
        "QA05-01b: a file the daemon cannot attribute to the Mayor slot is refused"
    );
    assert_eq!(report.executed(), 0);
    assert_eq!(
        report.quarantined(),
        0,
        "never quarantined: not ours to move"
    );
    assert!(path.exists(), "the file is left exactly where it was");
    assert!(!v.tree.manor_decree_ack(decree.id()).exists(), "no ack");
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert!(charter.decree_receipts.is_empty(), "no sequence consumed");
    assert!(
        matches!(
            report.outcomes.first(),
            Some(DecreeOutcome::Unattributable {
                reason: hamlet_core::decree::RejectReason::ForeignProvenance,
                ..
            })
        ),
        "the outcome names the typed reason: {:?}",
        report.outcomes.first()
    );
}

/// **QA05-04 — regression: the ack write must not follow a planted
/// symlink** (finding raised by the step-04 adversarial suite, fixed in
/// `decrees.rs::atomic_write`).
///
/// The Mayor owns `manor/home/decrees/` and nothing else in the village, and
/// an inbound mail artifact is an untrusted prompt input. A temp file the
/// daemon *predicts* and opens with `create(true).truncate(true)` is an
/// invitation: one planted symlink makes the **daemon** truncate any file it
/// can write — including `charter.json`, the single serialized state map
/// (SPEC §1). The fix is ADR-0007 create-new discipline (`O_CREAT|O_EXCL`,
/// which fails on an existing entry *including a symlink*) plus a bounded
/// collision walk. This test is the standing proof that the daemon no longer
/// follows the link — and that the planted link itself is left untouched.
#[tokio::test]
async fn qa05_04_ack_write_refuses_a_planted_symlink_temp_file() {
    let v = Village::new(Config::default()).await;

    let decree = escalation_decree(1, "clobber the charter", &["guild/register/scribe.md"]);
    write_decree(&v.tree, &decree);

    // The Mayor pre-plants the temp file the ack writer *would* have used, as
    // a symlink to the charter. The name is fully determined by the ack path.
    let ack_name = v
        .tree
        .manor_decree_ack(decree.id())
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let planted = v.tree.manor_decrees().join(format!(".{ack_name}.tmp"));
    let charter_path = v.tree.charter();
    std::os::unix::fs::symlink(&charter_path, &planted).unwrap();

    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.executed(), 1, "the decree itself is perfectly valid");

    // The charter is still the charter: valid, and still carrying its own
    // groups (the only change is the decree row the executor legitimately
    // wrote).
    let charter_after = Charter::load(&charter_path).expect("charter.json must stay loadable");
    assert!(
        !charter_after.decree_receipts.is_empty(),
        "the executor must still have recorded its receipt"
    );
    let bytes = fs::read_to_string(&charter_path).unwrap();
    assert!(
        !bytes.contains("decree_id") && bytes.contains("\"charter_version\""),
        "charter.json must never hold ack bytes: {}",
        &bytes.chars().take(200).collect::<String>()
    );

    // The planted symlink was skipped, never opened: it is still a symlink
    // pointing at the charter, and the ack landed on the real path instead.
    assert!(
        fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the planted temp name must be left as the Mayor wrote it"
    );
    let ack = v.tree.manor_decree_ack(decree.id());
    let ack_text = fs::read_to_string(&ack).expect("the ack is still written");
    assert!(
        hamlet_core::decree::DecreeAck::parse(&ack_text).is_ok(),
        "the ack is written on the real path: {ack_text}"
    );
}

/// Malformed / partial decree JSON: rejected, quarantined, **no partial
/// execution**, no ack, no sequence consumed.
#[tokio::test]
async fn malformed_and_partial_decrees_never_execute_partially() {
    let v = Village::new(Config::default()).await;
    let good = escalation_decree(1, "the real one", &["guild/register/scribe.md"]);
    let rendered = good.render().unwrap();
    let cut = rendered.len() / 2;

    let dir = v.tree.manor_decrees();
    let name = good.id().decree_file_name().unwrap().to_string();

    // (a) A torn in-place write: half the canonical bytes.
    fs::write(dir.join(&name), &rendered.as_bytes()[..cut]).unwrap();
    // (b) A valid-JSON non-object.
    fs::write(dir.join("d-000000000002-aaaaaaaaaaaaaaaa.json"), "[]").unwrap();
    // (c) Raw garbage.
    fs::write(
        dir.join("d-000000000003-bbbbbbbbbbbbbbbb.json"),
        "\u{0}\u{feff}",
    )
    .unwrap();
    // (d) Deeply nested JSON (serde_json's recursion cap must catch it, not
    //     the stack).
    let deep = format!(
        "{{\"decree_version\":1,\"decree_id\":\"d-000000000004-cccccccccccccccc\",\"decree_type\":\"escalate_to_human\",\"summary\":{}",
        "[".repeat(400)
    );
    fs::write(
        dir.join("d-000000000004-cccccccccccccccc.json"),
        format!("{deep}}}"),
    )
    .unwrap();
    // (e) Oversize.
    fs::write(
        dir.join("d-000000000005-dddddddddddddddd.json"),
        "x".repeat(17 * 1024),
    )
    .unwrap();

    // First pass: a file that has never parsed is only *suspected* of being
    // finished — it may still be a torn write, so nothing is quarantined
    // (QA05-05) and, above all, nothing executes.
    let first_pass = v.executor.scan().await.unwrap();
    assert_eq!(first_pass.executed(), 0, "nothing malformed may execute");
    assert_eq!(first_pass.parse_deferred(), 5, "all five are left in place");
    assert_eq!(first_pass.quarantined(), 0);

    // Second pass: unchanged since the refusal, so they are finished bad
    // files and are quarantined.
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.executed(), 0, "nothing malformed may execute");
    assert_eq!(report.quarantined(), 5, "every one is quarantined");
    for outcome in &report.outcomes {
        assert!(
            matches!(outcome, DecreeOutcome::Quarantined { .. }),
            "{outcome:?}"
        );
    }

    // No partial execution: no ack anywhere, no notice, no charter row, and
    // the sequence is still available for the Mayor to re-issue correctly.
    let acks: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".ack"))
        .collect();
    assert!(acks.is_empty(), "no ack may exist: {acks:?}");
    assert!(
        !v.tree.manor_escalations().exists() || {
            fs::read_dir(v.tree.manor_escalations())
                .map(|d| d.count())
                .unwrap_or(0)
                == 0
        }
    );
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert!(charter.decrees.is_empty(), "no sequence consumed");
    assert!(charter.decree_receipts.is_empty());
    assert_eq!(charter.next_decree_seq(), 1);

    // The operator keeps the evidence.
    assert_eq!(
        fs::read_dir(v.tree.manor_decrees_rejected())
            .unwrap()
            .count(),
        5
    );
}

/// **QA05-05 — regression: quarantine is not destructive to a write in
/// flight.** A torn in-place write used to be renamed into `.rejected/` on
/// the first parse failure, so the writer's remaining bytes landed in the
/// quarantine inode: the decree was lost forever, unexecutable, with no ack
/// and only an ack timeout to show for it. The executor now records the
/// failing file's (len, mtime) and only quarantines when a *later* pass sees
/// the same shape — proof that the writer stopped. A file still changing is
/// left alone and re-read.
#[tokio::test]
async fn qa05_05_a_torn_in_place_write_is_not_quarantined_under_its_writer() {
    let v = Village::new(Config::default()).await;
    let good = escalation_decree(1, "written in place, torn", &["guild/register/scribe.md"]);
    let rendered = good.render().unwrap();
    let name = good.id().decree_file_name().unwrap().to_string();
    let path = v.tree.manor_decrees().join(&name);

    // The Mayor holds the file open and is mid-write when the watcher fires.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    use std::io::Write;
    file.write_all(&rendered.as_bytes()[..rendered.len() / 3])
        .unwrap();
    file.sync_all().unwrap();

    // Pass 1: the file has never parsed, but it is *changing*. Nothing is
    // done to it.
    let first = v.executor.scan().await.unwrap();
    assert_eq!(first.parse_deferred(), 1, "the torn write is left in place");
    assert_eq!(first.quarantined(), 0);
    assert!(path.exists(), "the live file is still the live file");

    // Pass 2 without the writer finishing: still torn, still deferred.
    file.write_all(&rendered.as_bytes()[rendered.len() / 3..rendered.len() * 2 / 3])
        .unwrap();
    file.sync_all().unwrap();
    let second = v.executor.scan().await.unwrap();
    assert_eq!(second.parse_deferred(), 1, "still writing, still deferred");
    assert!(path.exists());

    // The Mayor finishes the decree.
    file.write_all(&rendered.as_bytes()[rendered.len() * 2 / 3..])
        .unwrap();
    file.sync_all().unwrap();
    drop(file);

    // Pass 3: now it parses — and it *executes*, which is the whole point.
    let third = v.executor.scan().await.unwrap();
    assert_eq!(third.executed(), 1, "QA05-05: the completed decree runs");
    assert_eq!(third.quarantined(), 0);
    assert!(
        matches!(
            read_ack(&v.tree, good.id()).result,
            hamlet_core::decree::AckResult::Done
        ),
        "and the Mayor gets its ack"
    );

    // A file that never becomes parseable *is* quarantined — on the second
    // observation, not the first, so a slow writer is never robbed.
    let garbage = escalation_decree(2, "garbage forever", &["guild/register/scribe.md"]);
    let garbage_name = garbage.id().decree_file_name().unwrap().to_string();
    let garbage_path = v.tree.manor_decrees().join(&garbage_name);
    fs::write(&garbage_path, "{not json").unwrap();
    let g1 = v.executor.scan().await.unwrap();
    assert_eq!(g1.parse_deferred(), 1, "first refusal: not yet quarantined");
    assert!(garbage_path.exists());
    let g2 = v.executor.scan().await.unwrap();
    assert_eq!(
        g2.quarantined(),
        1,
        "unchanged since the refusal → quarantine"
    );
    assert!(!garbage_path.exists());
    assert!(v
        .tree
        .manor_decrees_rejected()
        .join(&garbage_name)
        .is_file());
    assert!(
        !v.tree.manor_decree_ack(garbage.id()).exists(),
        "a file with no identity is still never acknowledged"
    );
}

/// The digest binds the file *name* to the body, so a delivered decree cannot
/// be edited in place and re-executed under its own (already-receipted) id,
/// and a body cannot be re-labelled onto a fresh sequence with a stale digest
/// (ADR-0026 cl. 3).
#[tokio::test]
async fn the_digest_binding_rejects_a_tampered_or_relabelled_decree() {
    let v = Village::new(Config::default()).await;
    let first = escalation_decree(1, "the real one", &["guild/register/scribe.md"]);
    write_decree(&v.tree, &first);
    assert_eq!(v.executor.scan().await.unwrap().executed(), 1);
    let notice_after_first = fs::read(v.tree.manor_escalation_file(first.id())).unwrap();

    // (a) Edit the delivered decree in place: same id, different body.
    let mut value: serde_json::Value = serde_json::from_str(&first.render().unwrap()).unwrap();
    value["summary"] = serde_json::json!("a much better summary, after the fact");
    fs::write(
        v.tree
            .manor_decrees()
            .join(first.id().decree_file_name().unwrap().as_str()),
        serde_json::to_string_pretty(&value).unwrap() + "\n",
    )
    .unwrap();
    // Pass 1 refuses it (as a possible torn write); pass 2, seeing it
    // unchanged, quarantines it (QA05-05). Either way it never executes.
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.executed(), 0, "a tampered body never re-executes");
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.executed(), 0, "a tampered body never re-executes");
    assert!(
        matches!(
            report.outcomes.last(),
            Some(DecreeOutcome::Quarantined { .. })
        ),
        "the digest binding rejects it: {:?}",
        report.outcomes
    );
    assert_eq!(
        fs::read(v.tree.manor_escalation_file(first.id())).unwrap(),
        notice_after_first,
        "the executed effect is unchanged"
    );

    // (b) Re-label a *different* body onto the next sequence while keeping
    //     the previous sequence's digest — the cheapest forgery available.
    let honest = escalation_decree(
        2,
        "an honest seq-2 escalation",
        &["guild/register/scribe.md"],
    );
    let forged_id = DecreeId::new(2, first.id().digest()).unwrap();
    let forged = Decree::EscalateToHuman {
        decree_id: forged_id,
        decree_version: DECREE_VERSION,
        evidence_refs: vec![TreeRelPath::parse("guild/register/scribe.md").unwrap()],
        summary: "an honest seq-2 escalation".to_owned(),
    };
    fs::write(
        v.tree
            .manor_decrees()
            .join(honest.id().decree_file_name().unwrap().as_str()),
        forged.render().unwrap(),
    )
    .unwrap();
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.executed(), 0, "a mis-digested body never executes");
    // QA05-05: pass one only *suspects* a torn write, pass two (unchanged)
    // quarantines.
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.executed(), 0, "a mis-digested body never executes");
    assert!(
        matches!(
            report.outcomes.last(),
            Some(DecreeOutcome::Quarantined { .. })
        ),
        "digest binding rejects at parse: {:?}",
        report.outcomes
    );
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert_eq!(charter.decrees.get(&2), None, "no sequence consumed");
}

// ===========================================================================
// 2. Replay
// ===========================================================================

/// The same decree delivered twice with a daemon restart in between is
/// executed **exactly once**. (ADR-0026 cl. 3–4.)
#[tokio::test]
async fn replay_across_a_daemon_restart_has_exactly_one_effect() {
    let mut config = Config::default();
    config.mayor.escalation_webhook_url = Some("https://hooks.example/notify".to_owned());
    let v = Village::new(config).await;

    let decree = escalation_decree(1, "exactly once", &["guild/register/scribe.md"]);
    write_decree(&v.tree, &decree);
    assert_eq!(v.executor.scan().await.unwrap().executed(), 1);

    let ack = fs::read(v.tree.manor_decree_ack(decree.id())).unwrap();
    let notice = fs::read(v.tree.manor_escalation_file(decree.id())).unwrap();
    assert_eq!(outbox_files(&v.tree).len(), 1);

    // --- daemon restart: a brand-new executor over the same tree ---------
    for pass in 0..3 {
        let restarted = DecreeExecutor::new(v.tree.clone(), v.config.clone());
        let report = restarted.scan().await.unwrap();
        assert_eq!(
            report.replayed(),
            1,
            "pass {pass}: a rescan of unchanged state is a no-op"
        );
        assert_eq!(report.executed(), 0);
        assert_eq!(
            fs::read(v.tree.manor_decree_ack(decree.id())).unwrap(),
            ack,
            "the re-ack is byte-identical"
        );
        assert_eq!(
            fs::read(v.tree.manor_escalation_file(decree.id())).unwrap(),
            notice
        );
        assert_eq!(outbox_files(&v.tree).len(), 1, "no second webhook POST");
    }

    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert_eq!(charter.decrees.get(&1), Some(&DecreeStatus::Delivered));
    assert_eq!(charter.next_decree_seq(), 2);
}

/// An ack file is *output*, never input: the Mayor forging one neither
/// advances nor steals state.
#[tokio::test]
async fn a_forged_ack_file_is_never_treated_as_state() {
    let v = Village::new(Config::default()).await;

    // (a) A Mayor-forged `done` ack for a decree that was never executed.
    let decree = escalation_decree(1, "forged ack attempt", &["guild/register/scribe.md"]);
    let forged = DecreeAck::done(decree.id());
    fs::write(
        v.tree.manor_decree_ack(decree.id()),
        forged.render().unwrap(),
    )
    .unwrap();
    write_decree(&v.tree, &decree);

    let report = v.executor.scan().await.unwrap();
    assert_eq!(
        report.executed(),
        1,
        "the daemon is the governor: a pre-planted ack does not skip execution"
    );
    assert!(v.tree.manor_escalation_file(decree.id()).is_file());
    assert_eq!(
        read_ack(&v.tree, decree.id()),
        read_ack(&v.tree, decree.id())
    );
    assert!(matches!(
        read_ack(&v.tree, decree.id()).result,
        hamlet_core::decree::AckResult::Done
    ));

    // (b) A Mayor-forged `rejected` ack for a delivered decree is overwritten
    //     by the daemon's truthful one on the next pass.
    let lying = DecreeAck::rejected(decree.id(), RejectReason::Malformed);
    fs::write(
        v.tree.manor_decree_ack(decree.id()),
        lying.render().unwrap(),
    )
    .unwrap();
    v.executor.scan().await.unwrap();
    assert!(
        matches!(
            read_ack(&v.tree, decree.id()).result,
            hamlet_core::decree::AckResult::Done
        ),
        "the daemon's verdict wins over the tree file the Mayor can write"
    );
}

// ===========================================================================
// 3. Zero-token sleep
// ===========================================================================

/// Over a monitored idle window the Mayor holds no process, writes nothing,
/// and consumes nothing.
#[tokio::test]
async fn the_idle_window_holds_no_process_and_writes_nothing() {
    let v = Village::new(config_with(8)).await;
    v.engine
        .wake(v.write_mail(1, "wake once", "body"))
        .await
        .unwrap();

    let sleep = v.engine.sleep_state();
    assert!(!sleep.awake);
    assert_eq!(sleep.turns, 1);

    let charter_before = fs::read(v.tree.charter()).unwrap();
    let log_before = fs::read_to_string(&v.log).unwrap();
    let children_before = live_mayor_pids(v._tmp.path());

    // --- the monitored idle window ------------------------------------
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    // -----------------------------------------------

    assert_eq!(
        fs::read_to_string(&v.log).unwrap(),
        log_before,
        "SPEC §7: no stream activity between events"
    );
    assert_eq!(
        fs::read(v.tree.charter()).unwrap(),
        charter_before,
        "sleep writes nothing at all — not even a heartbeat"
    );
    assert_eq!(v.engine.sleep_state().turns, 1, "no wake fired on its own");
    assert_eq!(v.engine.sleep_state().pending, 0);
    assert_eq!(
        live_mayor_pids(v._tmp.path()),
        children_before,
        "SPEC §7: the Mayor sleeps at zero token cost because no process exists"
    );
    assert_eq!(v.max_live.load(Ordering::SeqCst), 1);
}

/// Only registered triggers wake the Mayor.
#[tokio::test]
async fn unregistered_triggers_never_wake_the_mayor() {
    use hamlet_daemon::freight::{FreightEscalation, MayorWake};
    use hamlet_daemon::web::{WebhookEvent, WebhookWake};

    // `github` is deliberately NOT in `webhooks.routes` (ADR-0029 cl. 1).
    let mut config = config_with(8);
    config.mayor.crash_wake = hamlet_core::config::CrashWakePolicy::Never;
    let v = Village::new(config).await;

    // An unconfigured webhook topic.
    hamlet_daemon::mayor::WebhookWakeBridge::new(Arc::clone(&v.engine)).webhook(&WebhookEvent {
        topic: "github".to_owned(),
        correlation: hamlet_core::session::CorrelationId::for_dispatch(
            &SessionId::first(MAYOR_UID).unwrap(),
            1,
        )
        .unwrap(),
    });
    // A freight violation (registered — this one *should* wake).
    hamlet_daemon::mayor::FreightWakeBridge::new(Arc::clone(&v.engine)).wake(&FreightEscalation {
        owner: 1042,
        quarantined: vec!["exchange".to_owned()],
        audit_id: 7,
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while v.engine.sleep_state().pending > 0 {
        assert!(Instant::now() < deadline, "the freight hook never fired");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let delivered = prompts(&v.log);
    assert_eq!(delivered.len(), 1, "exactly one wake, from the one trigger");
    assert!(delivered[0].contains("wake_reason: freight_violation"));
    assert!(
        !delivered[0].contains("topic: github"),
        "an unconfigured webhook topic must never burn a turn"
    );

    // An ack-timeout poll with no outstanding decree fires nothing.
    let mut timeouts = hamlet_daemon::decrees::AckTimeouts::new();
    let now = Instant::now();
    assert!(timeouts
        .poll(
            &v.tree,
            v.config.mayor.ack_timeout_ms,
            v.config.mayor.ack_retry_ms,
            now,
        )
        .is_empty());
}

// ===========================================================================
// 4. Packet hygiene
// ===========================================================================

/// The charter projection carries no secret material: no `.env` values, no
/// `.env` key names, no PEM blocks, no bearer tokens (CONVENTIONS §6,
/// SPEC §11.4; ADR-0027 cl. 2 "a subset, not the charter dump").
#[tokio::test]
async fn the_charter_projection_carries_no_secret_material() {
    let mut config = config_with(8);
    // A webhook route: config.json references the shared secret by *key
    // name* only, and that key name must not reach the packet either.
    config.webhooks.routes.insert(
        "github".to_owned(),
        hamlet_core::config::WebhookRoute {
            dispatch_inbox: true,
            mail_to: vec![],
        },
    );
    config.webhooks.auth_env_key = "HAMLET_WEBHOOK_SECRET".to_owned();
    let v = Village::new(config).await;

    // Real-looking (but fake) secrets in the village `.env`, plus a stray PEM.
    let env = v.tree.root().parent().unwrap().join(".env");
    fs::write(
        &env,
        "ANTHROPIC_API_KEY=test-key-do-not-use-aaa\n\
         HAMLET_WEBHOOK_SECRET=test-key-do-not-use-bbb\n\
         OPENAI_API_KEY=test-key-do-not-use-ccc\n",
    )
    .unwrap();
    fs::write(
        v.tree.manor_mayor_md().parent().unwrap().join("DEED.md"),
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----\n",
    )
    .unwrap();

    // A charter row carrying an attacker-chosen identity that *looks* like a
    // credential: the projection must carry it verbatim (it is identity) but
    // never anything from the secret surface.
    let mut charter = Charter::load(&v.tree.charter()).unwrap();
    charter.commissions.insert(
        1,
        hamlet_core::commission::CommissionRecord::draft(name("commission")),
    );
    charter.allocate_slot(name("scribe"), Some(1)).unwrap();
    charter.save_atomic(&v.tree.charter()).unwrap();

    v.engine
        .wake(v.write_mail(1, "hygiene", "body"))
        .await
        .unwrap();
    let packet = prompts(&v.log).remove(0);

    for forbidden in [
        "test-key-do-not-use-aaa",
        "test-key-do-not-use-bbb",
        "test-key-do-not-use-ccc",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "HAMLET_WEBHOOK_SECRET",
        ".env",
        "BEGIN RSA PRIVATE KEY",
        "MIIEow",
    ] {
        assert!(
            !packet.contains(forbidden),
            "secret material `{forbidden}` leaked into the Mayor's context packet"
        );
    }

    // The grid is an allowlisted projection, not a charter dump: pin the key
    // set so a future field addition cannot silently widen it.
    let header = "--- CHARTER STATE GRID ---\n";
    let grid_start = packet.find(header).expect("packet carries a state grid") + header.len();
    let grid_end = packet
        .find("--- EVENT ---")
        .expect("packet carries an event section");
    let grid: serde_json::Value =
        serde_json::from_str(&packet[grid_start..grid_end]).expect("grid is canonical JSON");
    let mut keys: Vec<&str> = grid
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "charter_version",
            "commissions",
            "next_decree_seq",
            "open_decrees",
            "parcels",
            "sessions",
            "slots",
        ],
        "the projection widened — re-audit it for secret-bearing fields"
    );
}

/// Ten sequential wakes = ten invocations = ten complete single-turn packets,
/// with no thread accumulation between them (SPEC §7; ADR-0028 cl. 1).
#[tokio::test]
async fn ten_sequential_wakes_are_ten_single_turn_packets_with_no_thread() {
    let v = Village::new(config_with(8)).await;

    for i in 1..=10u64 {
        let event = v.write_mail(i, &format!("wake-{i:02}"), &format!("body-{i:02}"));
        let outcome = v.engine.wake(event).await.unwrap();
        let WakeOutcome::Evaluated(turn) = outcome else {
            panic!("a lone wake must evaluate: {outcome:?}");
        };
        assert_eq!(turn.events, 1, "a lone wake carries exactly one artifact");
        assert!(turn.is_settled());
    }

    let delivered = prompts(&v.log);
    assert_eq!(
        delivered.len(),
        10,
        "invocation count == wake count: no wake is served without a fresh process"
    );
    assert_eq!(v.engine.sleep_state().turns, 10);

    for (index, packet) in delivered.iter().enumerate() {
        let own = format!("wake-{:02}", index + 1);
        assert!(
            packet.starts_with("=== HAMLET EXECUTIVE PACKET v1 ==="),
            "packet {index} is a whole packet"
        );
        assert!(
            packet.trim_end().ends_with("=== END OF PACKET ==="),
            "packet {index} is a whole packet"
        );
        assert_eq!(
            packet.matches(&format!("subject: {own}\n")).count(),
            1,
            "packet {index} carries exactly its own artifact header"
        );
        for other in 1..=10u64 {
            let foreign = format!("wake-{other:02}");
            if foreign != own {
                assert!(
                    !packet.contains(&foreign),
                    "packet {index} accumulated thread from turn {other}"
                );
            }
        }
        assert!(
            !packet.contains("--continue"),
            "no packet may hint at resuming a prior pi context"
        );
    }

    // Every invocation ran without `--continue`, ever (ADR-0028 cl. 1).
    let cmds = v.cmds.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(cmds.len(), 10);
    for cmd in cmds.iter() {
        assert!(
            !cmd.iter().any(|a| a == "--continue"),
            "a continued pi process would carry a thread across wakes: {cmd:?}"
        );
    }
}

// ===========================================================================
// 5. Wake storms
// ===========================================================================

/// 500 events: merged and serialized, bounded queue, FIFO by arrival, every
/// event either evaluated exactly once or explicitly refused. No interleaved
/// evaluations, no silent drops (ADR-0028 cl. 2–3).
#[tokio::test]
async fn five_hundred_events_stay_serial_merged_bounded_and_fifo() {
    const TOTAL: u64 = 500;
    let v = Village::with_delay(256, 15).await;

    let mut tasks = Vec::new();
    for i in 1..=TOTAL {
        let event = v.write_mail(i, &format!("storm-{i:04}"), &format!("body-{i:04}"));
        let engine = Arc::clone(&v.engine);
        tasks.push(tokio::spawn(async move { engine.wake(event).await }));
    }

    let mut evaluated = 0usize;
    let mut refused = 0usize;
    for task in tasks {
        match task.await.unwrap().unwrap() {
            WakeOutcome::Evaluated(turn) => {
                assert!(turn.is_settled(), "{turn:?}");
                assert!(turn.events <= 256, "a batch never exceeds the cap");
                evaluated += turn.events;
            }
            WakeOutcome::Coalesced => {}
            WakeOutcome::Overflow { cap } => {
                assert_eq!(cap, 256);
                refused += 1;
            }
        }
    }

    assert_eq!(
        evaluated + refused,
        TOTAL as usize,
        "every event is either evaluated exactly once or explicitly refused — none vanish"
    );
    let stats = v.engine.queue_stats();
    assert_eq!(stats.depth, 0, "the queue drains completely");
    assert!(stats.overflowed as usize <= refused);
    assert!(
        stats.merged_into_batch > 0,
        "a 500-event storm must merge, not cost 500 turns"
    );
    assert_eq!(
        v.max_live.load(Ordering::SeqCst),
        1,
        "never two Mayor evaluations at once"
    );

    // FIFO: inside every merged batch, members appear in arrival order.
    let delivered = prompts(&v.log);
    let mut covered = 0usize;
    for packet in &delivered {
        let stamps: Vec<usize> = packet
            .match_indices("subject: storm-")
            .map(|(at, _)| {
                packet[at + "subject: storm-".len()..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap()
            })
            .collect();
        let unique: std::collections::BTreeSet<usize> = stamps.iter().copied().collect();
        assert_eq!(stamps.len(), unique.len(), "no event appears twice");
        let mut sorted = stamps.clone();
        sorted.sort_unstable();
        if stamps.len() > 1 {
            assert_eq!(
                stamps, sorted,
                "merged batch members are not FIFO by arrival:\n{packet}"
            );
        }
        covered += stamps.len();
    }
    assert_eq!(
        covered, evaluated,
        "every evaluated event is in exactly one packet"
    );
}

/// **QA05-06 — regression: the wake queue dedupes on source identity, not
/// on rendered content.**
///
/// The finding: `WakeQueue::push` treated `artifact == artifact` as a
/// duplicate arrival, so two genuinely distinct events that serialize
/// identically — the same sandbox quarantined twice for the same owner, two
/// identical crash notices — collapsed into one wake and the Mayor was never
/// told the second happened. Dedupe is now on a per-plane *identity* (audit
/// id, webhook correlation, spool path, decree id + notice number, crash
/// `uid/session/restarts`), which is what a double fire and a second real
/// event look like differently.
#[test]
fn qa05_06_distinct_events_with_identical_bodies_are_both_queued() {
    use hamlet_daemon::freight::FreightEscalation;
    use hamlet_daemon::mayor::{PushOutcome, WakeQueue};

    // Two separate audits of the *same* violation: identical payloads,
    // different occurrences.
    let escalation = |audit_id| FreightEscalation {
        owner: 1042,
        quarantined: vec!["exchange".to_owned()],
        audit_id,
    };
    let first = WakeEvent::from_freight_escalation(&escalation(1)).unwrap();
    let second = WakeEvent::from_freight_escalation(&escalation(2)).unwrap();
    assert_eq!(
        first.body(),
        second.body(),
        "the payloads really are byte-identical"
    );
    let mut queue = WakeQueue::new(8);
    assert!(matches!(
        queue.push(first),
        PushOutcome::Queued { depth: 1 }
    ));
    assert!(
        matches!(queue.push(second), PushOutcome::Queued { depth: 2 }),
        "QA05-06: the second, distinct violation is its own wake"
    );
    assert_eq!(queue.depth(), 2, "two wakes, not one");
    assert_eq!(
        queue.stats().merged_duplicates,
        0,
        "nothing was folded away"
    );

    // The same escalation delivered twice (a hook retry) is still ONE event:
    // dedupe is not disabled, it is aimed at the right thing.
    let again = WakeEvent::from_freight_escalation(&escalation(2)).unwrap();
    assert!(matches!(
        queue.push(again),
        PushOutcome::MergedDuplicate { seq: 2 }
    ));
    assert_eq!(queue.stats().merged_duplicates, 1, "counted telemetry");

    // Contrast: a *different* violation is a separate event.
    let other = WakeEvent::from_freight_escalation(&FreightEscalation {
        owner: 1042,
        quarantined: vec!["exchange".to_owned(), "forge".to_owned()],
        audit_id: 3,
    })
    .unwrap();
    assert!(matches!(
        queue.push(other),
        PushOutcome::Queued { depth: 3 }
    ));
    assert_eq!(queue.take_batch().unwrap().len(), 3, "all three events");
}

// ===========================================================================
// 6. Executor crash resilience
// ===========================================================================

/// A `provision_agent` whose provisioner is killed mid-flight leaves exactly
/// the state a `kill -9` leaves: the slot allocated and recorded, the parcel
/// half-built, no session. The restart must finish the decree without
/// duplicating the slot or the parcel (CONVENTIONS §3 r5/r6).
struct CrashingProvisioner {
    tree: HamletTree,
    identity: SafeName,
    crashed: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    uids: Arc<Mutex<Vec<u32>>>,
}

impl AgentProvisioner for CrashingProvisioner {
    fn provision<'a>(
        &'a self,
        _request: ProvisionRequest,
    ) -> BoxFuture<'a, Result<ProvisionOutcome, DecreeRefusal>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Sprint 02 allocates the slot before anything else; this is
            // where the "crash" lands — after the durable slot write, before
            // the parcel lease and the session.
            let mut charter = Charter::load(&self.tree.charter()).unwrap();
            let uid = charter
                .allocate_slot(self.identity.clone(), Some(1))
                .unwrap();
            charter.save_atomic(&self.tree.charter()).unwrap();
            let parcel = self.tree.parcel(uid);
            fs::create_dir_all(parcel.join("home")).unwrap();
            if self.crashed.swap(false, Ordering::SeqCst) {
                return Err(DecreeRefusal::execution_failed(
                    "kill -9 mid-ProvisionAgent",
                ));
            }
            charter = Charter::load(&self.tree.charter()).unwrap();
            charter.parcels.insert(
                uid,
                hamlet_core::charter::ParcelLease {
                    identity: self.identity.clone(),
                    stasis: false,
                },
            );
            charter.save_atomic(&self.tree.charter()).unwrap();
            self.uids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(uid);
            Ok(ProvisionOutcome {
                uid,
                incarnation: 1,
            })
        })
    }
}

#[tokio::test]
async fn crash_mid_provision_leaves_no_duplicate_slot_or_parcel() {
    let v = Village::new(Config::default()).await;

    let blueprint_src =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../defaults/village-alpine.toml");
    let bp = v.tree.guild_blueprint(&name("scribe"));
    fs::create_dir_all(bp.parent().unwrap()).unwrap();
    fs::copy(&blueprint_src, &bp).unwrap();
    // An open commission 1 for the decree to point at.
    let mut charter = Charter::load(&v.tree.charter()).unwrap();
    charter.commissions.insert(
        1,
        hamlet_core::commission::CommissionRecord::draft(name("commission")),
    );
    charter.save_atomic(&v.tree.charter()).unwrap();

    let crashed = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let uids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let provisioner = Arc::new(CrashingProvisioner {
        tree: v.tree.clone(),
        identity: name("scribe"),
        crashed: Arc::clone(&crashed),
        calls: Arc::clone(&calls),
        uids: Arc::clone(&uids),
    });

    let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
    let decree = Decree::ProvisionAgent {
        decree_id: placeholder,
        decree_version: DECREE_VERSION,
        blueprint_ref: name("scribe"),
        commission: Some(1),
        register_ref: name("scribe"),
    }
    .stamp(1)
    .unwrap();
    write_decree(&v.tree, &decree);

    // --- pass 1: the daemon dies mid-provision -------------------------
    let dying = DecreeExecutor::new(v.tree.clone(), v.config.clone())
        .with_provisioner(Arc::clone(&provisioner) as Arc<dyn AgentProvisioner>);
    dying.scan().await.unwrap();
    assert!(
        matches!(
            read_ack(&v.tree, decree.id()).result,
            hamlet_core::decree::AckResult::Rejected { .. }
        ),
        "the interrupted decree was acknowledged, not silently dropped"
    );
    // --- daemon restart ------------------------------------------------
    let restarted = DecreeExecutor::new(v.tree.clone(), v.config.clone())
        .with_provisioner(Arc::clone(&provisioner) as Arc<dyn AgentProvisioner>);
    // The pass that ships a working provisioner re-runs it. A decree the
    // first pass marked `Failed` is re-acknowledged with the same verdict
    // (deterministic), so the crash case we care about is the *queued* one:
    // restore it, which is exactly what a daemon that died between
    // admission and `set_decree_status` leaves behind.
    let mut charter = Charter::load(&v.tree.charter()).unwrap();
    charter.decrees.insert(1, DecreeStatus::Queued);
    charter.save_atomic(&v.tree.charter()).unwrap();
    let report = restarted.scan().await.unwrap();
    assert_eq!(report.executed(), 1, "the queued decree resumes");

    // Exactly one slot, one parcel, one lease for the identity.
    let charter = Charter::load(&v.tree.charter()).unwrap();
    let slots_for_identity: Vec<u32> = charter
        .slots
        .iter()
        .filter(|(_, slot)| slot.identity == name("scribe"))
        .map(|(uid, _)| *uid)
        .collect();
    assert_eq!(
        slots_for_identity.len(),
        1,
        "UID slots are permanent per identity — a resume must never re-allocate"
    );
    let uid = slots_for_identity[0];
    assert!(
        (1001..=2000).contains(&uid),
        "the resumed slot is a subagent slot, not 1000: {uid}"
    );
    assert_eq!(
        *uids.lock().unwrap_or_else(|e| e.into_inner()),
        vec![uid],
        "one successful completion, one uid"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one attempt, one resume");
    let parcel = v.tree.parcel(uid);
    let entries: Vec<_> = fs::read_dir(&parcel).unwrap().flatten().collect();
    assert_eq!(
        entries.len(),
        1,
        "no duplicate parcel content after the resume: {parcel:?}"
    );
    assert!(charter.parcels.contains_key(&uid));
    assert_eq!(charter.decrees.get(&1), Some(&DecreeStatus::Delivered));
}

// ===========================================================================
// 7. Escalation
// ===========================================================================

/// `escalate_to_human` is observable from the tree alone, and the notice is
/// digest-bound to the decree that caused it (ADR-0029 cl. 4).
#[tokio::test]
async fn escalation_is_tree_observable_and_digest_bound() {
    let v = Village::new(Config::default()).await;
    let decree = escalation_decree(
        1,
        "parcel 1042 has been stuck since dawn and I will not guess",
        &["guild/register/scribe.md"],
    );
    write_decree(&v.tree, &decree);
    assert_eq!(v.executor.scan().await.unwrap().executed(), 1);

    let path = v.tree.manor_escalation_file(decree.id());
    assert!(
        path.is_file(),
        "a human must find it without asking the daemon"
    );
    let bytes = fs::read(&path).unwrap();
    let notice = EscalationNotice::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
    assert_eq!(notice.decree_id, *decree.id());
    assert_eq!(
        notice.summary,
        "parcel 1042 has been stuck since dawn and I will not guess"
    );
    assert_eq!(
        notice.evidence_refs,
        vec![TreeRelPath::parse("guild/register/scribe.md").unwrap()],
        "evidence refs are re-resolved by the daemon, not trusted from the Mayor"
    );
    // Canonical bytes: sorted keys, trailing newline, re-render is identical.
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["decree_id"],
        serde_json::json!(decree.id().to_string())
    );
    value["summary"] = serde_json::json!("tampered by the mayor");
    let tampered = serde_json::to_string_pretty(&value).unwrap();
    assert!(
        EscalationNotice::parse(&format!("{tampered}\n")).is_err()
            || notice.summary != "tampered by the mayor",
        "a notice must not be forgeable by rewriting its file"
    );

    // The Mayor cannot reach the escalation directory at all: only home/ is
    // bound into the Mayor's container (SPEC §4 row for UID 1000).
    assert!(
        !format!("{:?}", hamlet_core::mount::MountSet::for_mayor()).contains("escalations"),
        "the Mayor has no mount over manor/escalations/"
    );
    assert!(
        v.tree.manor_escalations().starts_with(v.tree.manor()),
        "notices are filed inside the manor"
    );

    // ... and the CLI surface. SPEC §12.1 lists `decree {list|show}`; that
    // subcommand lands in Sprint 08 (`crates/hamlet-cli/src/main.rs`
    // currently answers `init` only). Recorded as OBS-05-04, not asserted
    // here: pinning a non-existent CLI would make the suite lie.
    assert!(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../hamlet-cli/src/main.rs")
            .exists(),
        "the CLI crate is present; the escalation hook is a Sprint 08 deliverable"
    );
}

/// **QA05-03 — regression: an ack timeout wakes once per *timeout*, not once
/// per process.**
///
/// The finding: `AckTimeouts` kept a `woken` set that was never cleared
/// while the file lived, so a decree left unanswered for a day escalated to
/// the Mayor exactly once for the lifetime of the daemon and the only escape
/// was restarting the process. The policy now has two clocks — the
/// threshold (`mayor.ack_timeout_ms`: has the Mayor had a fair chance?) and
/// the re-notification cadence (`mayor.ack_retry_ms`, new in config v10: how
/// often is it worth asking again?) — and the tracker forgets everything when
/// the ack lands.
#[tokio::test]
async fn qa05_03_ack_timeouts_re_wake_once_per_timeout_and_forget_on_ack() {
    let v = Village::new(Config::default()).await;
    let decree = escalation_decree(1, "never acknowledged", &["guild/register/scribe.md"]);
    write_decree(&v.tree, &decree);
    assert_eq!(v.executor.scan().await.unwrap().executed(), 1);

    // Re-issue a *different* body under the same id so the daemon will never
    // write an ack (the honest "stuck decree" shape): delete the ack.
    fs::remove_file(v.tree.manor_decree_ack(decree.id())).unwrap();

    // Threshold 10 ms, re-notification every 60 ms: a cadence tight enough
    // to observe ten times over without sleeping for ten minutes.
    let timeout_ms = 10u64;
    let retry_ms = 60u64;
    let mut timeouts = hamlet_daemon::decrees::AckTimeouts::new();
    let t0 = Instant::now();
    // First poll arms the timer (a file is first *seen*, never late).
    assert!(timeouts
        .poll(&v.tree, timeout_ms, retry_ms, Instant::now())
        .is_empty());
    std::thread::sleep(Duration::from_millis(30));
    let first = timeouts.poll(&v.tree, timeout_ms, retry_ms, Instant::now());
    assert_eq!(first.len(), 1, "the first timeout past the threshold wakes");
    assert_eq!(first[0].id, *decree.id());
    assert_eq!(first[0].notice, 1);

    // Ten more timeouts go by. The Mayor is told about each one, and not
    // more often than the retry cadence.
    let mut notices = vec![1u64];
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(15));
        for timeout in timeouts.poll(&v.tree, timeout_ms, retry_ms, Instant::now()) {
            assert_eq!(timeout.id, *decree.id());
            notices.push(timeout.notice);
        }
    }
    assert!(
        notices.len() >= 2,
        "QA05-03: a still-unanswered decree is re-escalated, not forgotten \
         (notices seen: {notices:?}, elapsed {:?})",
        t0.elapsed()
    );
    assert!(
        notices.windows(2).all(|w| w[1] > w[0]),
        "notice numbers only ever increase: {notices:?}"
    );

    // The retry cadence is real: a much longer interval means silence, so
    // the policy still bounds the wake rate instead of spinning the queue.
    let mut quiet = hamlet_daemon::decrees::AckTimeouts::new();
    let start = std::time::Instant::now();
    quiet.poll(&v.tree, timeout_ms, 600_000, start);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        quiet
            .poll(&v.tree, timeout_ms, 600_000, std::time::Instant::now())
            .len(),
        1,
        "the first word is still owed regardless of the cadence"
    );
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(15));
        assert!(
            quiet
                .poll(&v.tree, timeout_ms, 600_000, std::time::Instant::now())
                .is_empty(),
            "a ten-minute retry interval does not fire every 15ms"
        );
    }

    // Once acknowledged, the tracker forgets entirely.
    let mut t = hamlet_daemon::decrees::AckTimeouts::new();
    let _ = t.poll(&v.tree, timeout_ms, retry_ms, std::time::Instant::now());
    let _ = t.poll(&v.tree, timeout_ms, retry_ms, std::time::Instant::now());
    fs::write(
        v.tree.manor_decree_ack(decree.id()),
        DecreeAck::done(decree.id()).render().unwrap(),
    )
    .unwrap();
    assert!(t
        .poll(&v.tree, timeout_ms, retry_ms, std::time::Instant::now())
        .is_empty());
}
