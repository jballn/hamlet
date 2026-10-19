//! Decree execution integration tests (Sprint 05 step 03; CONVENTIONS §9).
//!
//! The Mayor never runs anything — it writes files, and the daemon's
//! `DecreeExecutor` is the only interpreter. What these pin:
//!
//! - **the full fake loop**: a mail event wakes the Mayor, the context
//!   packet goes out as one prompt, the FakeAgent-mayor *emits* a decree
//!   file mid-turn (`emit_files` script), the real watcher's `DecreeWrite`
//!   reaction runs the executor, the escalation notice + ack land in the
//!   tree, and the engine is back asleep — no podman anywhere;
//! - **replay idempotence**: re-issuing a byte-identical decree re-acks
//!   without touching the world (no second webhook POST, no rewritten
//!   notice);
//! - **crash-mid-decree resume**: a charter left at `queued` — exactly the
//!   state a daemon that died between admission and execution leaves — is
//!   completed by the next pass;
//! - **refusals are honest**: out-of-order / replay-mismatch / invalid-target
//!   decrees get typed acks and never consume their sequence, and files with
//!   no usable identity are quarantined.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{Charter, DecreeStatus, ParcelLease};
use hamlet_core::commission::CommissionRecord;
use hamlet_core::config::Config;
use hamlet_core::decree::{
    Decree, DecreeAck, DecreeError, DecreeId, EscalationNotice, RejectReason, TreeRelPath,
    DECREE_VERSION,
};
use hamlet_core::mail::{Mail, MailHeader, MailSender};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::decrees::{
    spawn_decree_plane, DecreeExecutor, DecreeOutcome, DecreeRefusal, SessionRouter, Stasis,
};
use hamlet_daemon::mayor::{MayorEngine, WakeEvent};
use hamlet_daemon::podman::{BoxFuture, FakeRuntime, Runtime};
use hamlet_daemon::supervise::HostProcessSpawner;
use hamlet_daemon::watch::Dispatcher;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn name(value: &str) -> SafeName {
    sanitize_name(value).expect("test name must sanitize")
}

/// One escalation decree body stamped at `seq` — the exact transformation a
/// stateless Mayor applies (ADR-0026 cl. 3).
fn escalation_decree(seq: u64, summary: &str, evidence_refs: Vec<TreeRelPath>) -> Decree {
    let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
    Decree::EscalateToHuman {
        decree_id: placeholder,
        decree_version: DECREE_VERSION,
        evidence_refs,
        summary: summary.to_owned(),
    }
    .stamp(seq)
    .unwrap()
}

/// The evidence file every fixture tree carries (escalations must cite
/// something that exists).
fn evidence_ref() -> TreeRelPath {
    TreeRelPath::parse("guild/register/scribe.md").unwrap()
}

/// Write the decree exactly as the Mayor would: canonical bytes at the id's
/// own file name.
fn write_decree(tree: &HamletTree, decree: &Decree) -> PathBuf {
    let name = decree.id().decree_file_name().unwrap();
    let path = tree.manor_decrees().join(name.as_str());
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

/// Files in the dispatch outbox (the escalation webhook POSTs).
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

/// A village tree with the manor provisioned and a default executor.
struct Village {
    tree: HamletTree,
    config: Config,
    executor: Arc<DecreeExecutor>,
    _tmp: tempfile::TempDir,
}

impl Village {
    async fn new(config: Config) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        Charter::empty().save_atomic(&tree.charter()).unwrap();

        // The manor parcel: MAYOR.md, decrees/, escalations/ (SPEC §4).
        let runtime = Arc::new(FakeRuntime::new());
        hamlet_daemon::mayor::ManorProvisioner::new(
            tree.clone(),
            config.clone(),
            Arc::clone(&runtime) as Arc<dyn Runtime>,
        )
        .provision(&Blueprint::default_village_alpine().unwrap())
        .await
        .expect("manor provisioning must succeed");

        // Evidence the escalations can cite.
        let brief = tree.register_brief(&name("scribe"));
        fs::create_dir_all(brief.parent().unwrap()).unwrap();
        fs::write(&brief, "# scribe — the register brief\n").unwrap();

        let executor = Arc::new(DecreeExecutor::new(tree.clone(), config.clone()));
        Self {
            tree,
            config,
            executor,
            _tmp: tmp,
        }
    }
}

// ---------------------------------------------------------------------------
// The full fake loop (event → wake → packet → decree file → act → ack → sleep)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_fake_loop_mail_wake_decree_execution_ack_and_sleep() {
    let mut config = Config::default();
    config.sessions.ready_timeout_ms = 10_000;

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

    // The decree the fake Mayor will emit during its turn, precomputed: a
    // stateless Mayor stamps seq + content digest and renames into place.
    let brief = tree.register_brief(&name("scribe"));
    fs::create_dir_all(brief.parent().unwrap()).unwrap();
    fs::write(&brief, "# scribe — the register brief\n").unwrap();
    let decree = escalation_decree(
        1,
        "operator: parcel 1042 stuck since dawn",
        vec![evidence_ref()],
    );
    let rendered = decree.render().unwrap();
    let decree_path = tree
        .manor_decrees()
        .join(decree.id().decree_file_name().unwrap().as_str());

    // FakeAgent script: emit the decree file mid-run (atomic temp + rename).
    let script_log = tmp.path().join("fakeagent-mayor.log");
    let script = tmp.path().join("fakeagent-mayor.json");
    fs::write(
        &script,
        serde_json::json!({
            "log": script_log.to_string_lossy(),
            "emit_files": [ { "path": decree_path.to_string_lossy(), "content": rendered } ],
        })
        .to_string(),
    )
    .unwrap();

    let engine = Arc::new(
        MayorEngine::new(
            tree.clone(),
            config,
            Arc::new(HostProcessSpawner::new(env!("CARGO_BIN_EXE_fakeagent"))),
        )
        .with_env(BTreeMap::from([(
            "FAKEAGENT_SCRIPT".to_owned(),
            script.to_string_lossy().into_owned(),
        )])),
    );

    // The decree plane: real tree watcher + recovery pass + DecreeWrite
    // handler — the production wiring, no manual scan calls.
    let dispatcher = Dispatcher::new();
    let mut handle = spawn_decree_plane(
        Arc::new(DecreeExecutor::new(tree.clone(), engine.config().clone())),
        dispatcher,
    )
    .await
    .expect("the decree plane must start");

    // Event → wake: a spool file addressed to UID 1000.
    let spool = tree.courier_mail_recipient(&name("1000"));
    fs::create_dir_all(&spool).unwrap();
    let mail_file = "000000000001-1001-ab12.mail";
    let mail = Mail {
        header: MailHeader {
            from: MailSender::Uid(1001),
            to: MAYOR_UID,
            subject: "parcel stuck".to_owned(),
            date: "1970-01-01T00:00:00Z".to_owned(),
        },
        body: "parcel 1042 has been stuck since dawn\n".to_owned(),
    };
    fs::write(spool.join(mail_file), mail.render()).unwrap();
    let event = WakeEvent::from_mail_spool(&tree, &spool.join(mail_file))
        .expect("the spool file the daemon just wrote must load");

    // Wake → packet → one single turn (the FakeAgent emits the decree in it).
    engine.wake(event).await.expect("the wake must evaluate");
    let sleep = engine.sleep_state();
    assert_eq!(sleep.pending, 0, "back to sleep after the turn");
    assert!(!sleep.awake, "no evaluation holds the lock anymore");
    assert_eq!(sleep.turns, 1, "exactly one incarnation for one wake");
    let log = fs::read_to_string(&script_log).unwrap();
    assert!(
        log.contains("emit-file"),
        "the fake Mayor wrote the decree file during its turn"
    );

    // The watcher's DecreeWrite reaction ran the executor: poll for the ack.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if tree.manor_decree_ack(decree.id()).exists() {
            break;
        }
        assert!(Instant::now() < deadline, "the ack never appeared");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The executor acted: the escalation notice in the watched inbox …
    let notice = EscalationNotice::parse(
        fs::read_to_string(tree.manor_escalation_file(decree.id()))
            .unwrap()
            .as_str(),
    )
    .unwrap();
    assert_eq!(notice.decree_id, *decree.id());
    assert_eq!(notice.summary, "operator: parcel 1042 stuck since dawn");
    assert_eq!(notice.evidence_refs, vec![evidence_ref()]);
    // … the done ack …
    let ack = read_ack(&tree, decree.id());
    assert!(matches!(ack.result, hamlet_core::decree::AckResult::Done));
    // … and the charter row: delivered.
    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(charter.decrees.get(&1), Some(&DecreeStatus::Delivered));

    handle.stop().await;
}

// ---------------------------------------------------------------------------
// Replay idempotence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_is_idempotent() {
    let mut config = Config::default();
    // The optional escalation webhook: a replayed decree must not POST twice.
    config.mayor.escalation_webhook_url = Some("https://hooks.example/notify".to_owned());
    let v = Village::new(config).await;

    let decree = escalation_decree(1, "first escalation", vec![evidence_ref()]);
    write_decree(&v.tree, &decree);

    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.executed(), 1);
    let ack_bytes = fs::read(v.tree.manor_decree_ack(decree.id())).unwrap();
    let notice_bytes = fs::read(v.tree.manor_escalation_file(decree.id())).unwrap();
    assert_eq!(outbox_files(&v.tree).len(), 1, "one webhook POST");

    // The Mayor re-issues the same decree (byte-identical file): replay.
    write_decree(&v.tree, &decree);
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.replayed(), 1, "the second pass is a replay");
    assert_eq!(
        fs::read(v.tree.manor_decree_ack(decree.id())).unwrap(),
        ack_bytes,
        "the re-ack is byte-identical"
    );
    assert_eq!(
        fs::read(v.tree.manor_escalation_file(decree.id())).unwrap(),
        notice_bytes,
        "the notice is not rewritten on replay"
    );
    assert_eq!(outbox_files(&v.tree).len(), 1, "no second webhook POST");
}

// ---------------------------------------------------------------------------
// Crash-mid-decree resume
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crash_mid_decree_resumes_on_restart() {
    let mut config = Config::default();
    config.mayor.escalation_webhook_url = Some("https://hooks.example/notify".to_owned());
    let v = Village::new(config).await;

    let decree = escalation_decree(1, "crash window", vec![evidence_ref()]);
    write_decree(&v.tree, &decree);

    // The crash window: admitted (receipt + queued status in ONE atomic
    // save) but not executed — exactly the charter state a daemon that died
    // between admission and execution leaves behind.
    let mut charter = Charter::load(&v.tree.charter()).unwrap();
    charter.admit_decree(decree.id()).expect("admission");
    charter.save_atomic(&v.tree.charter()).unwrap();

    // "Daemon restart": a fresh executor over the same tree runs its
    // recovery pass. The queued decree resumes — executors are idempotent.
    let restarted = Arc::new(DecreeExecutor::new(v.tree.clone(), v.config.clone()));
    let report = restarted.scan().await.unwrap();
    assert_eq!(report.executed(), 1, "the queued decree resumed");

    // Terminal state: delivered + ack + notice + exactly one webhook POST.
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert_eq!(charter.decrees.get(&1), Some(&DecreeStatus::Delivered));
    assert!(matches!(
        read_ack(&v.tree, decree.id()).result,
        hamlet_core::decree::AckResult::Done
    ));
    assert!(v.tree.manor_escalation_file(decree.id()).is_file());
    assert_eq!(outbox_files(&v.tree).len(), 1);
}

// ---------------------------------------------------------------------------
// Refusals: ordering, replay mismatch, invalid targets, quarantine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn out_of_order_and_replay_mismatch_are_refused_without_consuming() {
    let v = Village::new(Config::default()).await;

    // Sequence 2 arrives before sequence 1: refused, and the sequence is NOT
    // consumed (the Mayor may re-issue under the correct counter).
    let second = escalation_decree(2, "arrived early", vec![evidence_ref()]);
    write_decree(&v.tree, &second);
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.rejected(), 1);
    assert!(matches!(
        report.outcomes[0],
        DecreeOutcome::Rejected {
            reason: RejectReason::OutOfOrder,
            ..
        }
    ));
    let ack = read_ack(&v.tree, second.id());
    assert_eq!(
        ack.result,
        hamlet_core::decree::AckResult::Rejected {
            reason: RejectReason::OutOfOrder
        }
    );
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert!(charter.decrees.is_empty(), "no sequence consumed");

    // Now sequence 1 (different content): one directory pass executes both
    // the new seq-1 decree and the still-living seq-2 file — the same
    // decision, admissible once seq 1 has landed.
    let first = escalation_decree(1, "the real first", vec![evidence_ref()]);
    write_decree(&v.tree, &first);
    let report = v.executor.scan().await.unwrap();
    assert_eq!(
        report.executed(),
        2,
        "one pass over the directory executes both admissible decrees"
    );
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert_eq!(charter.decrees.get(&1), Some(&DecreeStatus::Delivered));
    assert_eq!(charter.decrees.get(&2), Some(&DecreeStatus::Delivered));

    // Replay mismatch: sequence 1 again, but with different content.
    let impostor = escalation_decree(1, "a different body", vec![evidence_ref()]);
    write_decree(&v.tree, &impostor);
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.rejected(), 1);
    assert!(matches!(
        report.outcomes[0],
        DecreeOutcome::Rejected {
            reason: RejectReason::ReplayMismatch,
            ..
        }
    ));
    let ack = read_ack(&v.tree, impostor.id());
    assert_eq!(
        ack.result,
        hamlet_core::decree::AckResult::Rejected {
            reason: RejectReason::ReplayMismatch
        }
    );
}

#[tokio::test]
async fn invalid_target_is_acked_rejected_without_side_effects() {
    let v = Village::new(Config::default()).await;

    // An escalation citing evidence that does not exist in the tree.
    let ghost = TreeRelPath::parse("guild/register/ghost.md").unwrap();
    let decree = escalation_decree(1, "phantom evidence", vec![ghost]);
    write_decree(&v.tree, &decree);

    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.rejected(), 1);
    assert!(matches!(
        report.outcomes[0],
        DecreeOutcome::Rejected {
            reason: RejectReason::InvalidTarget,
            ..
        }
    ));
    let ack = read_ack(&v.tree, decree.id());
    assert_eq!(
        ack.result,
        hamlet_core::decree::AckResult::Rejected {
            reason: RejectReason::InvalidTarget
        }
    );

    // Nothing happened: no notice, no sequence consumed.
    assert!(!v.tree.manor_escalation_file(decree.id()).exists());
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert!(charter.decrees.is_empty());
    assert!(charter.decree_receipts.is_empty());
}

#[tokio::test]
async fn malformed_files_are_quarantined_not_acknowledged() {
    let v = Village::new(Config::default()).await;
    let dir = v.tree.manor_decrees();

    // A well-formed decree name with an unparseable body: no usable
    // identity, so it is quarantined — never acknowledged.
    fs::write(
        dir.join("d-000000000001-abcdef0123456789.json"),
        "not json {{{",
    )
    .unwrap();
    // A non-decree file in the directory is ignored entirely (not even an
    // outcome).
    fs::write(dir.join("notes.txt"), "scratch\n").unwrap();

    // Pass one only suspects a torn write (QA05-05): the file is still in
    // place, nothing was acknowledged. Pass two, seeing it unchanged,
    // quarantines it.
    let first = v.executor.scan().await.unwrap();
    assert_eq!(first.parse_deferred(), 1);
    assert_eq!(first.quarantined(), 0);
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.quarantined(), 1);
    assert_eq!(report.outcomes.len(), 1, "notes.txt is not a decree");
    assert!(matches!(
        report.outcomes[0],
        DecreeOutcome::Quarantined { .. }
    ));

    // Moved to .rejected/, gone from the live directory, no ack, no charter
    // row.
    let quarantined = v
        .tree
        .manor_decrees_rejected()
        .join("d-000000000001-abcdef0123456789.json");
    assert!(quarantined.is_file());
    assert!(!dir.join("d-000000000001-abcdef0123456789.json").exists());
    assert!(
        dir.join("notes.txt").is_file(),
        "non-decrees are never touched"
    );
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert!(charter.decrees.is_empty());
    assert!(charter.decree_receipts.is_empty());
}

// ---------------------------------------------------------------------------
// The other executors, behind their seams (recording doubles)
// ---------------------------------------------------------------------------

/// A [`SessionRouter`] double: records what the executor routed.
#[derive(Default)]
struct RecordingRouter {
    reassigned: std::sync::Mutex<Vec<(u32, u64, String, String)>>,
}

impl SessionRouter for RecordingRouter {
    fn reassign(
        &self,
        uid: u32,
        commission: u64,
        entry: &SafeName,
        text: &str,
    ) -> Result<(), DecreeRefusal> {
        self.reassigned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((uid, commission, entry.to_string(), text.to_owned()));
        Ok(())
    }

    fn steer<'a>(
        &'a self,
        _uid: u32,
        _directive: &'a str,
    ) -> BoxFuture<'a, Result<(), DecreeRefusal>> {
        Box::pin(async move {
            Err(DecreeRefusal::execution_failed(
                "steer is not exercised in this test",
            ))
        })
    }
}

/// A [`Stasis`] double: records which uids were parked.
#[derive(Default)]
struct RecordingStasis {
    parked: std::sync::Mutex<Vec<u32>>,
}

impl Stasis for RecordingStasis {
    fn park<'a>(&'a self, uid: u32) -> BoxFuture<'a, Result<(), DecreeRefusal>> {
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(uid);
        Box::pin(async move { Ok(()) })
    }
}

/// A charter with one subagent slot (sanctioned allocation path) and an open
/// commission.
fn charter_with_slot(tree: &HamletTree, identity: &SafeName, commission: u64) -> u32 {
    let mut charter = Charter::load(&tree.charter()).unwrap();
    let uid = charter
        .allocate_slot(identity.clone(), Some(commission))
        .unwrap();
    charter
        .commissions
        .insert(commission, CommissionRecord::draft(name("commission")));
    charter.save_atomic(&tree.charter()).unwrap();
    uid
}

#[tokio::test]
async fn reassign_task_routes_through_the_session_router() {
    let v = Village::new(Config::default()).await;
    let uid = charter_with_slot(&v.tree, &name("scribe"), 1);

    let router = Arc::new(RecordingRouter::default());
    let executor = Arc::new(
        DecreeExecutor::new(v.tree.clone(), v.config.clone())
            .with_router(Arc::clone(&router) as Arc<dyn SessionRouter>),
    );

    let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
    let decree = Decree::ReassignTask {
        decree_id: placeholder,
        decree_version: DECREE_VERSION,
        commission: 1,
        entry: name("task-7"),
        uid,
    }
    .stamp(1)
    .unwrap();
    write_decree(&v.tree, &decree);

    let report = executor.scan().await.unwrap();
    assert_eq!(report.executed(), 1);
    assert!(matches!(
        read_ack(&v.tree, decree.id()).result,
        hamlet_core::decree::AckResult::Done
    ));

    let recorded = router.reassigned.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(recorded.len(), 1);
    let (r_uid, r_commission, r_entry, r_text) = &recorded[0];
    assert_eq!(*r_uid, uid);
    assert_eq!(*r_commission, 1);
    assert_eq!(r_entry.as_str(), "task-7");
    assert!(r_text.contains("commission 1"), "{r_text}");
    assert!(r_text.contains("INDENTURE.md"), "{r_text}");
}

#[tokio::test]
async fn hibernate_agent_flips_the_charter_stasis() {
    let v = Village::new(Config::default()).await;
    let uid = charter_with_slot(&v.tree, &name("scribe"), 1);
    // Hibernation needs a parcel lease as well (SPEC §10).
    let mut charter = Charter::load(&v.tree.charter()).unwrap();
    charter.parcels.insert(
        uid,
        ParcelLease {
            identity: name("scribe"),
            stasis: false,
        },
    );
    charter.save_atomic(&v.tree.charter()).unwrap();

    let stasis = Arc::new(RecordingStasis::default());
    let executor = Arc::new(
        DecreeExecutor::new(v.tree.clone(), v.config.clone())
            .with_stasis(Arc::clone(&stasis) as Arc<dyn Stasis>),
    );

    let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
    let decree = Decree::HibernateAgent {
        decree_id: placeholder,
        decree_version: DECREE_VERSION,
        uid,
    }
    .stamp(1)
    .unwrap();
    write_decree(&v.tree, &decree);

    let report = executor.scan().await.unwrap();
    assert_eq!(report.executed(), 1);

    // The seam parked the session AND the charter lease is now in stasis —
    // the slot reads "asleep" to every other subsystem.
    assert_eq!(
        *stasis.parked.lock().unwrap_or_else(|e| e.into_inner()),
        vec![uid]
    );
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert!(charter.parcels[&uid].stasis);
}

/// The `deferred` path: a promotion decree with the Sprint 06 stub seam is
/// acknowledged `deferred`, and its status stays `queued` so the pass that
/// ships the real seam executes it.
#[tokio::test]
async fn promotion_defer_keeps_the_decree_open() {
    let v = Village::new(Config::default()).await;

    // A freight file the decree can point at (an agent-created sandbox).
    let sandbox = v.tree.courier_freight_sandbox(&name("scribe"));
    fs::create_dir_all(&sandbox).unwrap();
    fs::write(sandbox.join("report.md"), "# report\n").unwrap();

    let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
    let decree = Decree::ApprovePromotion {
        decree_id: placeholder,
        decree_version: DECREE_VERSION,
        artifact_name: name("report"),
        freight_path: TreeRelPath::parse("courier/freight/scribe/report.md").unwrap(),
    }
    .stamp(1)
    .unwrap();
    write_decree(&v.tree, &decree);

    // The executor RAN the decree and handled the deferred refusal inside:
    // the outcome is `executed`, the ack is `rejected: deferred`, and the
    // status stays queued (the pass that ships the real seam executes it).
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.executed(), 1);
    let ack = read_ack(&v.tree, decree.id());
    assert_eq!(
        ack.result,
        hamlet_core::decree::AckResult::Rejected {
            reason: RejectReason::Deferred
        }
    );

    // The status stays queued (not failed): the decree is still outstanding.
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert_eq!(charter.decrees.get(&1), Some(&DecreeStatus::Queued));
}

/// `DecreeError` → stable ack token, end to end: a file whose body fails
/// validation (not just one that fails to parse) is quarantined with the
/// mapped reason.
#[tokio::test]
async fn unsupported_version_file_is_quarantined_with_the_mapped_reason() {
    let v = Village::new(Config::default()).await;
    // A stamped decree re-rendered with a foreign version (parse fails at
    // `check_version` — the first check in order).
    let decree = escalation_decree(1, "version drift", vec![evidence_ref()]);
    let mut value: serde_json::Value = serde_json::from_str(&decree.render().unwrap()).unwrap();
    value["decree_version"] = serde_json::json!(2);
    let text = serde_json::to_string(&value).unwrap();
    assert!(matches!(
        Decree::parse(&text),
        Err(DecreeError::UnsupportedVersion(2))
    ));

    fs::write(
        v.tree
            .manor_decrees()
            .join(decree.id().decree_file_name().unwrap().as_str()),
        text,
    )
    .unwrap();
    let _ = v.executor.scan().await.unwrap();
    let report = v.executor.scan().await.unwrap();
    assert_eq!(report.quarantined(), 1);
    assert!(matches!(
        report.outcomes[0],
        DecreeOutcome::Quarantined {
            reason: RejectReason::UnsupportedVersion,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// QA05-02 — the ack write escalates to the `podman unshare` seam
// ---------------------------------------------------------------------------

/// **QA05-02 — regression: a daemon that cannot write into the manor home
/// writes it from inside its own userns instead of stalling the Mayor loop.**
///
/// The finding: `manor/home/` is 0700 and owned by subuid 1000 (ADR-0011
/// cl. 3), so under the spec'd dedicated service user the daemon's direct
/// ack write is refused by the kernel — and the old code only *traced* it, so
/// the decree executed, the ack never landed, and the whole loop stalled
/// silently after one turn. Step 03b made the failure loud; this test pins
/// the other half: `PermissionDenied` now escalates through
/// `Runtime::unshare_write_atomic` (ADR-0011 cl. 4) with the exact ack bytes
/// and the Mayor UID as the new owner.
///
/// The refusal is produced for real — a mode-0500 decree directory refuses
/// creation even to its own owner — so this is the deployment shape, not a
/// simulated error.
#[tokio::test]
async fn qa05_02_an_unwritable_manor_home_uses_the_unshare_write_seam() {
    let v = Village::new(Config::default()).await;
    let runtime = Arc::new(FakeRuntime::new());
    let executor = Arc::new(
        DecreeExecutor::new(v.tree.clone(), v.config.clone())
            .with_runtime(Arc::clone(&runtime) as Arc<dyn Runtime>),
    );

    let decree = escalation_decree(1, "written where the daemon cannot", vec![evidence_ref()]);
    write_decree(&v.tree, &decree);

    // 0700 → 0500: the owner may read and traverse but not create. Exactly
    // what a subuid-owned manor home looks like to the daemon host user.
    let dir = v.tree.manor_decrees();
    let original_mode = {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o7777;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        mode
    };

    let report = executor.scan().await;
    // Restore the mode before asserting, so a failure leaves a usable tree.
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(original_mode)).unwrap();
    }

    let report = report.expect("the pass completes: the ack is written through the seam");
    assert_eq!(report.executed(), 1, "the decree itself is perfectly valid");

    let writes = runtime.unshare_writes();
    assert_eq!(writes.len(), 1, "exactly one write went through the seam");
    let (path, bytes, owner) = &writes[0];
    assert_eq!(
        path,
        &v.tree.manor_decree_ack(decree.id()),
        "the seam writes the ack the Mayor is waiting for"
    );
    assert_eq!(&bytes[..], decree_ack_bytes(&decree).as_bytes());
    assert_eq!(*owner, (MAYOR_UID, MAYOR_UID), "handed to the Mayor slot");
    assert!(
        v.tree.manor_decree_ack(decree.id()).is_file(),
        "and the ack really landed in the tree"
    );

    // Without a runtime installed the same refusal is a HARD error, never a
    // trace: the Mayor's loop must not stall quietly (the finding's stated
    // acceptable alternative, still true).
    let decree2 = escalation_decree(2, "no seam installed", vec![evidence_ref()]);
    write_decree(&v.tree, &decree2);
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    }
    let bare = DecreeExecutor::new(v.tree.clone(), v.config.clone());
    let error = bare.scan().await;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(original_mode)).unwrap();
    }
    let error = error.expect_err("with no seam the ack failure must surface");
    assert!(
        error.to_string().contains(&dir.display().to_string()),
        "the error names the path it could not write: {error}"
    );
}

fn decree_ack_bytes(decree: &Decree) -> String {
    DecreeAck::done(decree.id()).render().unwrap()
}
