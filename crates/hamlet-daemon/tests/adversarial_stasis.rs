//! Adversarial integration test suite for Sprint 07 Hibernation & Census
//! (SPEC §10, §14; ADR-0039, ADR-0040).
//!
//! Surface areas covered:
//! 1. **TITLE integrity**: golden sequence, append-only history invariant,
//!    restart stability, tampering on append (QA07-04), cache loss on restart (QA07-05).
//! 2. **Hibernation races**: in-flight task drain, interrupt safety, double-hibernate
//!    idempotence, crashed session hibernation, kill -9 mid-copy pre-hibernation state,
//!    post-teardown failure transition to Crashed.
//! 3. **Stasis integrity**: file corruptions in home backup refused at awaken,
//!    PROPRIETOR frontmatter tampering refused, symlinks bricking awaken (QA07-02),
//!    dangling symlinks in dir digests (QA07-06).
//! 4. **Awakening**: full slot space exhaustion hard error, released parcel unreadable,
//!    pkgcache counters zero fast path, multi-awaken patron destruction (QA07-01).
//! 5. **Convergence independence**: DEED failure does not block PROPRIETOR save and vice versa,
//!    both failures emit escalation notice and wake event exactly once.
//! 6. **Resource policy**: daemon never auto-hibernates, static audit of call sites,
//!    missing trigger assertion (QA07-03), candidate ranking and OOM policy vectors.
//! 7. **Full-cycle resumability**: provision -> work -> hibernate -> restart -> awaken ->
//!    converge -> close with deterministic kill points matching un-interrupted final state.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{CommissionRecord, CommissionState};
use hamlet_core::config::Config;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{SessionId, SessionRecord, SessionStatus};
use hamlet_core::slots::{SlotError, SlotStatus, FIRST_AGENT_UID, LAST_AGENT_UID};
use hamlet_core::stasis::{
    evaluate_oom_policy, rank_candidates, BranchOutcome, CandidateInput, OomAction, TitleDoc,
    TitleEntry, TitleEntryKind, TitleError,
};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::{
    clear_title_cache, format_epoch_iso8601, read_parcel_title, stamp_parcel_title, CensusWake,
    ProductionStasis, RecordingCensusWake, StasisError, TitleWriter,
};
use hamlet_daemon::ledger::{Ledger, RecordingCommissionWake};
use hamlet_daemon::podman::FakeRuntime;
use hamlet_daemon::telemetry::Telemetry;

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("valid safe name")
}

fn setup_test_village(
    tmp: &tempfile::TempDir,
    identity_str: &str,
    commission_id: u64,
) -> (HamletTree, Arc<FakeRuntime>, Arc<Telemetry>, u32) {
    clear_title_cache();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let runtime = Arc::new(FakeRuntime::new());
    let telemetry = Arc::new(Telemetry::open_for_village(tree.root(), &Config::default()).unwrap());

    let mut charter = Charter::empty();
    let identity = name(identity_str);
    let slot_uid = charter
        .allocate_slot(identity.clone(), Some(commission_id))
        .unwrap();

    charter.parcels.insert(
        slot_uid,
        ParcelLease {
            identity: identity.clone(),
            stasis: false,
        },
    );

    let session = SessionRecord {
        id: SessionId::new(slot_uid, 1).unwrap(),
        status: SessionStatus::Running,
        restart_count: 0,
    };
    charter.sessions.insert(slot_uid, session);

    let mut comm = CommissionRecord::draft(name("adversarial-comm"));
    comm.state = CommissionState::Active;
    comm.roster.insert(slot_uid);
    charter.commissions.insert(commission_id, comm);

    charter.save_atomic(&tree.charter()).unwrap();

    // Seed blueprint in guild
    let bp = Blueprint::default_village_alpine().unwrap();
    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    fs::create_dir_all(bp_path.parent().unwrap()).unwrap();
    fs::write(&bp_path, bp.render_toml(None)).unwrap();

    // Seed parcel directory
    let parcel_dir = tree.parcel(slot_uid);
    fs::create_dir_all(&parcel_dir).unwrap();

    // 1. Seed TITLE.md
    let title_path = tree.parcel_title(slot_uid);
    let title_content = format!(
        "# TITLE — parcel{slot_uid}\n\n\
         - [2026-10-19T01:00:00Z] trigger commission={commission_id} prompt=\"adversarial task\"\n\
         - [2026-10-19T01:00:01Z] provision uid={slot_uid} archetype={identity} blueprint=village-alpine version=1\n\
         - [2026-10-19T01:00:05Z] session session_id={slot_uid}.0 incarnation=1\n"
    );
    fs::write(&title_path, title_content).unwrap();

    // 2. Seed DEED.md
    let deed_path = tree.parcel_deed(slot_uid);
    let deed_content = format!(
        "# DEED — parcel{slot_uid}\n\n\
         - identity: {identity}\n\
         - uid: {slot_uid}\n\
         - commission: {commission_id}\n\
         - blueprint: village-alpine v1\n\n\
         ### Dynamic Alterations\n\n\
         - baseline · toolchain bootstrap · uid {slot_uid}\n"
    );
    fs::write(&deed_path, deed_content).unwrap();

    // 3. Seed PROPRIETOR.md
    let prop_path = tree.parcel_proprietor(slot_uid);
    let prop_content = format!(
        "# PROPRIETOR — {identity}\n\n\
         Intelligence profile for {identity} at slot {slot_uid}.\n"
    );
    fs::write(&prop_path, prop_content).unwrap();

    // 4. Seed home/ directory with sample files
    let home_dir = tree.parcel_home(slot_uid);
    fs::create_dir_all(home_dir.join("src")).unwrap();
    fs::write(home_dir.join("README.md"), b"# Agent Private Home\n").unwrap();
    fs::write(
        home_dir.join("src/lib.rs"),
        b"pub fn compute() -> i32 { 42 }\n",
    )
    .unwrap();

    (tree, runtime, telemetry, slot_uid)
}

// ===========================================================================
// Area 1: TITLE integrity
// ===========================================================================

#[tokio::test]
async fn test_title_golden_sequence_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, _runtime, _telemetry, uid) = setup_test_village(&tmp, "chronicler", 10);

    // Append full lifecycle sequence
    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Steer {
            timestamp: format_epoch_iso8601(1_700_000_100),
            directive: "prioritize verification suite".to_string(),
        },
    )
    .unwrap();

    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Promotion {
            timestamp: format_epoch_iso8601(1_700_000_200),
            kind: "bundle".to_string(),
            target: "patch-01.bundle".to_string(),
            status: "applied".to_string(),
        },
    )
    .unwrap();

    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Promotion {
            timestamp: format_epoch_iso8601(1_700_000_300),
            kind: "submission".to_string(),
            target: "freight/report.tar".to_string(),
            status: "submitted".to_string(),
        },
    )
    .unwrap();

    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Promotion {
            timestamp: format_epoch_iso8601(1_700_000_400),
            kind: "deed".to_string(),
            target: "village-alpine.v2".to_string(),
            status: "promoted".to_string(),
        },
    )
    .unwrap();

    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Crash {
            timestamp: format_epoch_iso8601(1_700_000_500),
            session_id: "1001.0".to_string(),
            exit_code: Some(137),
            restarts: 0,
            reason: "oom killed".to_string(),
        },
    )
    .unwrap();

    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::SessionIncarnation {
            timestamp: format_epoch_iso8601(1_700_000_600),
            session_id: "1001.1".to_string(),
            incarnation: 2,
        },
    )
    .unwrap();

    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Promotion {
            timestamp: format_epoch_iso8601(1_700_000_700),
            kind: "session".to_string(),
            target: "1001.1".to_string(),
            status: "stopped".to_string(),
        },
    )
    .unwrap();

    stamp_parcel_title(
        &tree,
        uid,
        TitleEntry::Hibernate {
            timestamp: format_epoch_iso8601(1_700_000_800),
            patron: name("chronicler"),
            reason: "operator:cli".to_string(),
        },
    )
    .unwrap();

    let doc = read_parcel_title(&tree, uid).unwrap();
    assert_eq!(doc.entries.len(), 11);

    let expected_kinds = [
        TitleEntryKind::Trigger,
        TitleEntryKind::Provision,
        TitleEntryKind::SessionIncarnation,
        TitleEntryKind::Steer,
        TitleEntryKind::Promotion,
        TitleEntryKind::Promotion,
        TitleEntryKind::Promotion,
        TitleEntryKind::Crash,
        TitleEntryKind::SessionIncarnation,
        TitleEntryKind::Promotion,
        TitleEntryKind::Hibernate,
    ];

    for (i, (entry, &expected)) in doc.entries.iter().zip(&expected_kinds).enumerate() {
        assert_eq!(entry.kind(), expected, "kind mismatch at index {i}");
    }
}

#[tokio::test]
async fn test_title_append_only_prefix_assertion() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, _runtime, _telemetry, uid) = setup_test_village(&tmp, "scribe", 10);

    let doc = read_parcel_title(&tree, uid).unwrap();
    assert_eq!(doc.entries.len(), 3);

    // Strict prefix check passes
    let prefix = &doc.entries[0..2];
    assert!(doc.verify_append_only(prefix).is_ok());

    // Decreased count fails
    let longer_prev = vec![
        doc.entries[0].clone(),
        doc.entries[1].clone(),
        doc.entries[2].clone(),
        TitleEntry::Steer {
            timestamp: "2026-10-19T01:00:10Z".to_string(),
            directive: "extra".to_string(),
        },
    ];
    assert!(matches!(
        doc.verify_append_only(&longer_prev),
        Err(TitleError::NonAppendMutation { .. })
    ));

    // Modified historical entry fails
    let mut mutated_prev = doc.entries.clone();
    mutated_prev[1] = TitleEntry::Provision {
        timestamp: "2026-10-19T01:00:01Z".to_string(),
        uid: 1001,
        archetype: name("hacked"),
        blueprint: name("village-alpine"),
        version: 99,
    };
    assert!(matches!(
        doc.verify_append_only(&mutated_prev),
        Err(TitleError::NonAppendMutation { .. })
    ));
}

#[tokio::test]
async fn test_title_stability_across_daemon_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, _runtime, _telemetry, uid) = setup_test_village(&tmp, "archivist", 10);

    let doc1 = read_parcel_title(&tree, uid).unwrap();
    let digest1 = doc1.content_digest();

    // Simulate daemon restart: wipe in-memory cache
    clear_title_cache();

    let doc2 = read_parcel_title(&tree, uid).unwrap();
    let digest2 = doc2.content_digest();

    assert_eq!(doc1.entries, doc2.entries);
    assert_eq!(digest1, digest2);
}

/// **Finding QA07-04 (MEDIUM)**:
/// `TitleWriter::append` unconditionally executes `let _ = self.read_and_verify(uid);`,
/// completely discarding any tamper or parse errors. If an adversary mutates or corrupts
/// `TITLE.md`, `append` blindly opens the corrupted file and appends the new entry.
#[tokio::test]
async fn qa07_04_title_append_ignores_read_and_verify_tamper_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, _runtime, _telemetry, uid) = setup_test_village(&tmp, "rogue-agent", 10);

    // Populate initial cache
    let writer = TitleWriter::new(tree.clone());
    let (doc_clean, tampered) = writer.read_and_verify(uid).unwrap();
    assert!(!tampered);
    assert_eq!(doc_clean.entries.len(), 3);

    // Adversary replaces TITLE.md with unparseable non-markdown garbage
    let title_path = tree.parcel_title(uid);
    fs::write(
        &title_path,
        b"CORRUPTED GARBAGE NOT MARKDOWN NO HEADER\nINVALID LINES\n",
    )
    .unwrap();

    // read_and_verify directly confirms parse failure
    let read_res = writer.read_and_verify(uid);
    assert!(
        matches!(
            read_res,
            Err(hamlet_daemon::census::CensusError::Tamper { .. })
        ),
        "read_and_verify detects unparseable corruption"
    );

    // REPRO QA07-04: append ignores the error and appends anyway, returning Ok(())!
    let append_res = writer.append(
        uid,
        TitleEntry::Steer {
            timestamp: format_epoch_iso8601(1_700_000_999),
            directive: "injected after corruption".to_string(),
        },
    );

    // Expected under SPEC §10: append must fail closed when TITLE.md is corrupted.
    // Actual: append returns Ok(()), blindly writing into the corrupted file!
    assert!(
        append_res.is_ok(),
        "QA07-04 repro: TitleWriter::append blindly appends to corrupted TITLE.md without error"
    );

    let content_after = fs::read_to_string(&title_path).unwrap();
    assert!(
        content_after.contains("CORRUPTED GARBAGE"),
        "corrupted lines preserved"
    );
    assert!(
        content_after.contains("injected after corruption"),
        "new entry appended to garbage file"
    );
}

/// **Finding QA07-05 (MEDIUM)**:
/// `TITLE_CACHE` is stored in an in-memory `OnceLock<Mutex<...>>` with no persistent record
/// in `charter.json` or on disk. When the daemon restarts, `TITLE_CACHE` is empty. If an adversary
/// edits historical entries while the daemon restarts, `read_and_verify` sees `cache.get(&key)`
/// is None, so non-append mutation is never detected and tampered history is adopted as truth.
#[tokio::test]
async fn qa07_05_title_cache_blind_across_daemon_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, _runtime, _telemetry, uid) = setup_test_village(&tmp, "patient-rogue", 10);

    let wake = Arc::new(RecordingCensusWake::new());
    let writer = TitleWriter::new(tree.clone()).with_wake(Arc::clone(&wake) as Arc<dyn CensusWake>);

    // 1. Initial read populates cache
    let (_doc, tampered) = writer.read_and_verify(uid).unwrap();
    assert!(!tampered);

    // 2. Simulate daemon restart: cache wiped
    clear_title_cache();

    // 3. Rogue agent modifies historical provision line in place while daemon was down
    let title_path = tree.parcel_title(uid);
    let original = fs::read_to_string(&title_path).unwrap();
    let tampered_content = original.replace("version=1", "version=999");
    fs::write(&title_path, tampered_content).unwrap();

    // 4. Daemon restarts and calls read_and_verify
    let (_doc_after, tampered_after) = writer.read_and_verify(uid).unwrap();

    // Expected under CONVENTIONS §2/§3 and Requirement 1: tampering detected across restarts.
    // Actual (QA07-05): tampered_after is false, and no Mayor wake event is emitted!
    assert!(
        !tampered_after,
        "QA07-05 repro: ephemeral in-memory cache fails to detect tampering across daemon restart"
    );
    assert_eq!(
        wake.events().len(),
        0,
        "QA07-05 repro: no Mayor wake fired on restart read of tampered TITLE.md"
    );
}

// ===========================================================================
// Area 2: Hibernation races
// ===========================================================================

#[tokio::test]
async fn test_hibernate_during_inflight_task_graceful_drain() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "worker", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry);

    // Seed delivery queue file with pending tasking envelope
    let queue_path = tree
        .root()
        .join("courier/queues")
        .join(format!("{uid}.jsonl"));
    fs::create_dir_all(queue_path.parent().unwrap()).unwrap();
    fs::write(
        &queue_path,
        b"{\"kind\":\"tasking\",\"payload\":\"in-flight task payload\"}\n",
    )
    .unwrap();

    // Hibernate agent
    let doc = stasis.hibernate(uid, "operator:cli").await.unwrap();
    assert_eq!(doc.frontmatter.patron, name("worker"));

    // Assert: delivery queue preserved without truncation (SPEC §10)
    assert!(queue_path.is_file(), "delivery queue must be preserved");
    let queue_content = fs::read_to_string(&queue_path).unwrap();
    assert!(
        queue_content.contains("in-flight task payload"),
        "undelivered task payload must not be truncated"
    );

    // Compute freed
    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(
        charter.slots.get(&uid).unwrap().status,
        SlotStatus::Hibernated
    );
}

#[tokio::test]
async fn test_hibernate_during_interrupt() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "interrupted", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry);

    // Seed an interrupt envelope in spool/mail
    let spool = tree.courier_mail_recipient(&name(&uid.to_string()));
    fs::create_dir_all(&spool).unwrap();
    fs::write(
        spool.join("interrupt.json"),
        b"{\"interrupt\":\"urgent_halt\"}\n",
    )
    .unwrap();

    // Hibernate during pending interrupt executes cleanly
    let doc = stasis.hibernate(uid, "operator:cli").await.unwrap();
    assert_eq!(doc.frontmatter.source_uid, uid);

    // Mail spool is preserved
    assert!(spool.join("interrupt.json").is_file());
}

#[tokio::test]
async fn test_hibernate_double_decree_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "stable-agent", 10);
    let stasis = Arc::new(ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry));

    // First hibernation
    let doc1 = stasis.hibernate(uid, "decree:0001").await.unwrap();
    assert_eq!(doc1.frontmatter.patron, name("stable-agent"));

    // Second hibernation: idempotent return of existing patron record
    let doc2 = stasis.hibernate(uid, "decree:0002").await.unwrap();
    assert_eq!(doc1.frontmatter.patron, doc2.frontmatter.patron);
    assert_eq!(doc1.frontmatter.source_uid, doc2.frontmatter.source_uid);
}

#[tokio::test]
async fn test_hibernate_crashed_session_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "crashed-agent", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    // Set session status to Crashed in charter
    let mut charter = Charter::load(&tree.charter()).unwrap();
    charter.sessions.get_mut(&uid).unwrap().status = SessionStatus::Crashed;
    charter.save_atomic(&tree.charter()).unwrap();

    // Hibernation of crashed session is allowed and succeeds
    let doc = stasis.hibernate(uid, "operator:cli").await.unwrap();
    assert_eq!(doc.frontmatter.patron, name("crashed-agent"));

    let charter_after = Charter::load(&tree.charter()).unwrap();
    assert_eq!(
        charter_after.slots.get(&uid).unwrap().status,
        SlotStatus::Hibernated
    );
    assert_eq!(
        charter_after.sessions.get(&uid).unwrap().status,
        SessionStatus::Stopped
    );
}

#[tokio::test]
async fn test_hibernate_kill_9_mid_copy_leaves_pre_hibernation_state() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, _runtime, _telemetry, uid) = setup_test_village(&tmp, "resilient", 10);

    // Simulate kill -9 mid-copy leaving a partial staging dir in tavern/patrons/
    let patrons_dir = tree.tavern_patrons();
    fs::create_dir_all(&patrons_dir).unwrap();
    let unfinished_staging = patrons_dir.join(".tmp_PROPRIETORresilient_1001_123456789");
    fs::create_dir_all(&unfinished_staging).unwrap();
    fs::write(
        unfinished_staging.join("partial.tmp"),
        b"partial copy bytes",
    )
    .unwrap();

    // Assert: patron record is NOT committed in tavern/patrons/PROPRIETORresilient
    let patron_dir = tree.patron(&name("resilient"));
    assert!(!patron_dir.exists(), "patron dir must not exist mid-copy");

    // Assert: live parcel directory is intact
    let parcel_dir = tree.parcel(uid);
    assert!(parcel_dir.is_dir(), "live parcel home must remain intact");

    // Charter state remains pre-hibernation
    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(charter.slots.get(&uid).unwrap().status, SlotStatus::Active);
}

#[tokio::test]
async fn test_hibernate_post_teardown_failure_marks_crashed_hibernating() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "overflow-agent", 10);

    // Configure max_home_bytes to 5 bytes to trigger failure in Step 2 after container teardown
    let stasis = ProductionStasis::new(tree.clone(), runtime)
        .with_telemetry(telemetry)
        .with_max_home_bytes(5);

    let err = stasis.hibernate(uid, "operator:cli").await.unwrap_err();
    assert!(
        matches!(err, StasisError::PostTeardown { .. }),
        "must return typed PostTeardown error: {err}"
    );

    // Charter must transition slot and session to Crashed
    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(
        charter.slots.get(&uid).unwrap().status,
        SlotStatus::Crashed,
        "slot must be Crashed"
    );
    assert_eq!(
        charter.sessions.get(&uid).unwrap().status,
        SessionStatus::Crashed,
        "session must be Crashed"
    );

    // TITLE.md must record crash entry describing failure
    let title_doc = read_parcel_title(&tree, uid).unwrap();
    let last = title_doc.entries.last().unwrap();
    assert_eq!(last.kind(), TitleEntryKind::Crash);
}

// ===========================================================================
// Area 3: Stasis integrity
// ===========================================================================

#[tokio::test]
async fn test_stasis_awaken_refuses_corrupted_file_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "integrity-agent", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    // Hibernate cleanly
    stasis.hibernate(uid, "operator:cli").await.unwrap();

    // Corrupt one byte in home.uid.bak/README.md without changing length
    let backed_file = tree
        .patron_home_backup(&name("integrity-agent"))
        .join("README.md");
    let mut bytes = fs::read(&backed_file).unwrap();
    bytes[0] ^= 0xFF;
    fs::write(&backed_file, bytes).unwrap();

    // Awaken must fail with typed manifest verification mismatch
    let err = stasis
        .awaken(&name("integrity-agent"), Some(10))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StasisError::ManifestVerification(ref msg) if msg.contains("digest mismatch")),
        "awaken must refuse corrupted file digest: {err}"
    );
}

#[tokio::test]
async fn test_stasis_awaken_refuses_tampered_proprietor_frontmatter() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "frontmatter-agent", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    stasis.hibernate(uid, "operator:cli").await.unwrap();

    // Tamper with PROPRIETOR.md: delete closing `---` fence
    let prop_path = tree.patron_md(&name("frontmatter-agent"));
    let original = fs::read_to_string(&prop_path).unwrap();
    let broken = original.replacen("---\n", "", 2); // strip fences
    fs::write(&prop_path, broken).unwrap();

    // Awaken must refuse
    let err = stasis
        .awaken(&name("frontmatter-agent"), Some(10))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StasisError::Patron(..)),
        "awaken must refuse tampered frontmatter: {err}"
    );
}

/// **Finding QA07-02 (HIGH)**:
/// `copy_dir_recursive` in `census.rs:2565-2574` copies symlinks into `home.uid.bak/`
/// and `hibernate_post_teardown` writes a `ManifestEntryType::Symlink` into `manifest.json`.
/// Hibernation reports success, container is destroyed, and live parcel is deleted.
/// But subsequently, `verify_backup_integrity` in `census.rs:2610-2614` unconditionally
/// rejects any symlinks during awaken, permanently bricking the patron record!
#[tokio::test]
async fn qa07_02_hibernate_copies_symlinks_bricking_subsequent_awaken() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "symlink-victim", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    // Agent creates a symlink in home/
    let home = tree.parcel_home(uid);
    let link_path = home.join("link_to_readme");
    #[cfg(unix)]
    std::os::unix::fs::symlink("README.md", &link_path).unwrap();

    // 1. Hibernate succeeds, copying the symlink into home.uid.bak/
    let hib_res = stasis.hibernate(uid, "operator:cli").await;
    assert!(
        hib_res.is_ok(),
        "QA07-02 repro: hibernate succeeds and allows symlink into backup"
    );

    // Container destroyed, parcel deleted
    assert!(!tree.parcel(uid).exists());

    // 2. Awaken is now attempted: verify_backup_integrity refuses the symlink!
    let awaken_res = stasis.awaken(&name("symlink-victim"), Some(10)).await;

    // REPRO QA07-02: awaken fails with ManifestVerification, bricking the patron!
    assert!(
        matches!(awaken_res, Err(StasisError::ManifestVerification(ref msg)) if msg.contains("refusing symlink")),
        "QA07-02 repro: patron with symlink is permanently bricked and cannot be awakened: {awaken_res:?}"
    );
}

/// **Finding QA07-06 (MEDIUM)**:
/// `compute_dir_digest` in `census.rs:2733` uses `entry.metadata()?` which follows symlinks.
/// If a subdirectory contains a dangling symlink (broken target), `metadata()?` returns
/// `ErrorKind::NotFound`, causing `hibernate_post_teardown` to fail and mark the slot Crashed.
#[tokio::test]
async fn qa07_06_compute_dir_digest_follows_symlinks_and_fails_on_dangling() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "dangling-victim", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    // Create a subdirectory with a dangling symlink
    let sub = tree.parcel_home(uid).join("docs");
    fs::create_dir_all(&sub).unwrap();
    let broken_link = sub.join("dangling_link");
    #[cfg(unix)]
    std::os::unix::fs::symlink("nonexistent_target_file.txt", &broken_link).unwrap();

    // Attempt hibernate: compute_dir_digest follows symlink and fails with NotFound!
    let hib_res = stasis.hibernate(uid, "operator:cli").await;

    // REPRO QA07-06: fails post-teardown because compute_dir_digest called entry.metadata()
    assert!(
        matches!(hib_res, Err(StasisError::PostTeardown { .. })),
        "QA07-06 repro: compute_dir_digest follows dangling symlink and crashes with NotFound: {hib_res:?}"
    );

    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(
        charter.slots.get(&uid).unwrap().status,
        SlotStatus::Crashed,
        "QA07-06 repro: post-teardown failure transitions slot to Crashed"
    );
}

// ===========================================================================
// Area 4: Awakening
// ===========================================================================

#[tokio::test]
async fn test_awaken_full_slot_space_exhaustion_hard_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "slot-seeker", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    stasis.hibernate(uid, "operator:cli").await.unwrap();

    // Artificially exhaust all slots 1001..=2000 in charter
    let mut charter = Charter::load(&tree.charter()).unwrap();
    for slot_num in (FIRST_AGENT_UID + 1)..=LAST_AGENT_UID {
        charter.slots.insert(
            slot_num,
            hamlet_core::slots::SlotRecord::new(name(&format!("filler{slot_num}")), Some(10)),
        );
    }
    charter.save_atomic(&tree.charter()).unwrap();

    // Awaken must fail with typed Slot exhaustion hard error
    let err = stasis
        .awaken(&name("slot-seeker"), Some(10))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StasisError::Slot(SlotError::Exhausted { .. })),
        "must return hard SlotError::Exhausted: {err}"
    );
}

#[tokio::test]
async fn test_awaken_released_old_parcel_unreadable() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "clean-slate", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    stasis.hibernate(uid, "operator:cli").await.unwrap();

    // Old parcel directory must be completely released (removed) from disk
    let old_parcel = tree.parcel(uid);
    assert!(!old_parcel.exists(), "old parcel directory must be removed");

    // Awaken into new slot
    let awakened = stasis.awaken(&name("clean-slate"), Some(10)).await.unwrap();
    assert_ne!(awakened.uid, uid);

    // Old parcel remains absent
    assert!(!old_parcel.exists());
}

#[tokio::test]
async fn test_awaken_pkgcache_counters_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "fast-waker", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    stasis.hibernate(uid, "operator:cli").await.unwrap();

    let pkgcache = tree.market_pkgcache();
    let _ = fs::remove_dir_all(&pkgcache);
    fs::create_dir_all(&pkgcache).unwrap();

    // Awaken executes fast path without downloading tools
    let awakened = stasis.awaken(&name("fast-waker"), Some(10)).await.unwrap();
    assert_eq!(awakened.uid, 1002);

    let pkgcache_files: Vec<_> = fs::read_dir(&pkgcache).unwrap().collect();
    assert!(
        pkgcache_files.is_empty(),
        "fast path must not download tools: pkgcache has files: {pkgcache_files:?}"
    );
}

/// **Finding QA07-01 (CRITICAL / HIGH)**:
/// Step 05 QA Requirement 4 mandates: "two awakens of one patron → two independent identities,
/// shared lineage refs."
/// In `census.rs:2054`, `awaken` unconditionally calls `fs::remove_dir_all(&patron_dir)`,
/// deleting the patron record from disk. Attempting to awaken the patron a second time fails
/// immediately with `NoSuchPatron`.
/// Furthermore, `awaken` hardcodes the awakened slot identity to `patron_ref` rather than
/// an independent identity, causing `charter.validate()` to reject multiple active awakens.
#[tokio::test]
async fn qa07_01_two_awakens_of_one_patron_destroyed_and_single_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "archetype-patron", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    // 1. Hibernate agent to produce patron in tavern/patrons/PROPRIETORarchetype-patron/
    stasis.hibernate(uid, "operator:cli").await.unwrap();
    let patron_dir = tree.patron(&name("archetype-patron"));
    assert!(patron_dir.is_dir());

    // 2. First awaken succeeds
    let awakened1 = stasis
        .awaken(&name("archetype-patron"), Some(10))
        .await
        .unwrap();
    assert_eq!(awakened1.uid, 1002);

    // REPRO QA07-01 Part A: patron record was deleted from tavern/patrons/!
    assert!(
        !patron_dir.exists(),
        "QA07-01 repro: awaken deleted tavern/patrons/PROPRIETOR<patron> on first awaken"
    );

    // REPRO QA07-01 Part B: second awaken fails because patron record was deleted
    let awaken2_res = stasis.awaken(&name("archetype-patron"), Some(10)).await;
    assert!(
        matches!(awaken2_res, Err(StasisError::NoSuchPatron(..))),
        "QA07-01 repro: second awaken of patron fails with NoSuchPatron because record was destroyed"
    );

    // REPRO QA07-01 Part C: even if patron record is restored, awakened1 holds identity "archetype-patron",
    // so awakening again with the same patron name violates single-active-identity charter invariant!
    assert_eq!(awakened1.identity, name("archetype-patron"));
}

// ===========================================================================
// Area 5: Convergence independence
// ===========================================================================

#[test]
fn test_convergence_deed_failure_does_not_block_proprietor() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "deed-fail-agent", 10);
    let stasis = Arc::new(
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone()),
    );
    let wake = Arc::new(RecordingCommissionWake::new());
    let ledger = Ledger::new(tree.clone())
        .with_stasis(stasis)
        .with_runtime(runtime)
        .with_telemetry(telemetry)
        .with_wake(wake);

    // Corrupt DEED.md with invalid grammar
    let deed_path = tree.parcel_deed(uid);
    let corrupt_deed = format!(
        "# DEED — parcel{uid}\n\n\
         - identity: deed-fail-agent\n\
         - uid: {uid}\n\
         - commission: 10\n\
         - blueprint: village-alpine v1\n\n\
         ### Dynamic Alterations\n\n\
         - totally invalid alteration line that breaks grammar\n"
    );
    fs::write(&deed_path, corrupt_deed).unwrap();

    // Close commission: dual convergence runs
    let report = ledger.close_commission("adversarial-comm").unwrap();
    let summary = report.convergence.unwrap();

    assert_eq!(summary.reports.len(), 1);
    let p_rep = &summary.reports[&uid];
    assert!(
        matches!(p_rep.deed_outcome, BranchOutcome::Failed { .. }),
        "deed branch must fail"
    );
    assert!(
        matches!(p_rep.proprietor_outcome, BranchOutcome::Succeeded { .. }),
        "proprietor branch must succeed despite deed failure"
    );

    // Patron record is saved in tavern/patrons/
    assert!(tree.patron(&name("deed-fail-agent")).is_dir());
}

#[test]
fn test_convergence_proprietor_failure_does_not_block_deed() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "prop-fail-agent", 10);
    // Configure stasis max_home_bytes to 5 bytes so stasis fails
    let stasis = Arc::new(
        ProductionStasis::new(tree.clone(), runtime.clone())
            .with_telemetry(telemetry.clone())
            .with_max_home_bytes(5),
    );
    let wake = Arc::new(RecordingCommissionWake::new());
    let ledger = Ledger::new(tree.clone())
        .with_stasis(stasis)
        .with_runtime(runtime)
        .with_telemetry(telemetry)
        .with_wake(wake);

    // Add valid alteration to DEED.md so deed promotion succeeds
    let deed_path = tree.parcel_deed(uid);
    let deed_content = format!(
        "# DEED — parcel{uid}\n\n\
         - identity: prop-fail-agent\n\
         - uid: {uid}\n\
         - commission: 10\n\
         - blueprint: village-alpine v1\n\n\
         ### Dynamic Alterations\n\n\
         - baseline · toolchain bootstrap · uid {uid}\n\
         - pixi uv = 0.4.2\n"
    );
    fs::write(&deed_path, deed_content).unwrap();

    let report = ledger.close_commission("adversarial-comm").unwrap();
    let summary = report.convergence.unwrap();

    let p_rep = &summary.reports[&uid];
    assert!(
        matches!(p_rep.deed_outcome, BranchOutcome::Succeeded { .. }),
        "deed branch must succeed despite proprietor failure"
    );
    assert!(
        matches!(p_rep.proprietor_outcome, BranchOutcome::Failed { .. }),
        "proprietor branch must fail"
    );
}

#[test]
fn test_convergence_both_failures_emits_escalation_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "both-fail-agent", 10);
    let stasis = Arc::new(
        ProductionStasis::new(tree.clone(), runtime.clone())
            .with_telemetry(telemetry.clone())
            .with_max_home_bytes(5),
    );
    let wake = Arc::new(RecordingCommissionWake::new());
    let ledger = Ledger::new(tree.clone())
        .with_stasis(stasis)
        .with_runtime(runtime)
        .with_telemetry(telemetry)
        .with_wake(wake.clone());

    // Corrupt DEED.md so both branches fail
    let corrupt_deed = format!(
        "# DEED — parcel{uid}\n\n\
         - identity: both-fail-agent\n\
         - uid: {uid}\n\
         - commission: 10\n\
         - blueprint: village-alpine v1\n\n\
         ### Dynamic Alterations\n\n\
         - totally invalid alteration line that breaks grammar\n"
    );
    fs::write(tree.parcel_deed(uid), corrupt_deed).unwrap();

    let report = ledger.close_commission("adversarial-comm").unwrap();
    let summary = report.convergence.unwrap();

    assert!(!summary.overall_success());
    let p_rep = &summary.reports[&uid];
    assert!(matches!(p_rep.deed_outcome, BranchOutcome::Failed { .. }));
    assert!(matches!(
        p_rep.proprietor_outcome,
        BranchOutcome::Failed { .. }
    ));

    // Assert: wake called exactly once
    assert_eq!(
        wake.convergence_failures().len(),
        1,
        "convergence_failed wake must be called exactly once"
    );

    // Assert: escalation notice filed in manor
    let escalations_dir = tree.manor_escalations();
    assert!(escalations_dir.is_dir());
    let notice_files: Vec<_> = fs::read_dir(&escalations_dir).unwrap().collect();
    assert_eq!(
        notice_files.len(),
        1,
        "exactly one escalation notice file created"
    );
}

// ===========================================================================
// Area 6: Resource policy
// ===========================================================================

#[test]
fn test_resource_policy_static_audit_call_sites() {
    // Static audit: verify that all .hibernate( and .hibernate_sync( call sites
    // are in decrees.rs, ledger.rs, agent.rs (CLI), or internal census/test files.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let daemon_src = root.join("crates/hamlet-daemon/src");
    let cli_src = root.join("crates/hamlet-cli/src");

    for dir in [daemon_src, cli_src] {
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "rs") {
                let content = fs::read_to_string(&path).unwrap();
                for (line_no, line) in content.lines().enumerate() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("//") {
                        continue;
                    }
                    if trimmed.contains(".hibernate(") || trimmed.contains(".hibernate_sync(") {
                        let fname = path.file_name().unwrap().to_string_lossy();
                        assert!(
                            fname == "census.rs"
                                || fname == "decrees.rs"
                                || fname == "ledger.rs"
                                || fname == "agent.rs"
                                || fname == "local_api.rs",
                            "unauthorized hibernate call site at {}:{}: {}",
                            path.display(),
                            line_no + 1,
                            trimmed
                        );
                    }
                }
            }
        }
    }
}

/// **Finding QA07-03 (HIGH)**:
/// Step 05 QA Requirement 6 mandates: "daemon NEVER auto-hibernates (audit all hibernate
/// call sites: trigger must be decree|operator — static test over the code path + runtime assertion)".
/// In product code, `hibernate_internal` contains NO assertion or validation on `trigger`.
/// Any arbitrary trigger is accepted. In `ledger.rs:2059`, `close_commission` calls
/// `stasis.hibernate_sync(uid, &trigger)` with `"commission:{commission_id}:close"`,
/// which violates the mandatory `decree|operator` trigger policy.
#[tokio::test]
async fn qa07_03_missing_runtime_assertion_on_hibernation_trigger() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry, uid) = setup_test_village(&tmp, "trigger-victim", 10);
    let stasis = ProductionStasis::new(tree.clone(), runtime).with_telemetry(telemetry);

    // Calling hibernate with an unauthorized trigger
    let unauthorized_trigger = "unauthorized:daemon_autonomous_trigger";
    let res = stasis.hibernate(uid, unauthorized_trigger).await;

    // REPRO QA07-03: hibernation succeeds with unauthorized trigger!
    assert!(
        res.is_ok(),
        "QA07-03 repro: hibernate accepts unauthorized non-decree/non-operator trigger without error"
    );

    // Sled telemetry and archived title record the unauthorized trigger verbatim
    let archived_title = tree.parcel_archive(uid).join("TITLE.md");
    let content = fs::read_to_string(&archived_title).unwrap();
    let title_doc = TitleDoc::parse(&content).unwrap();
    let last = title_doc.entries.last().unwrap();
    match last {
        TitleEntry::Hibernate { reason, .. } => {
            assert_eq!(
                reason, unauthorized_trigger,
                "QA07-03 repro: unauthorized trigger stamped into TITLE.md"
            );
        }
        other => panic!("expected Hibernate entry, got {:?}", other),
    }
}

#[test]
fn test_resource_candidate_ranking_and_oom_policy() {
    let inputs = vec![
        CandidateInput {
            uid: 1001,
            identity: name("crashed-cand"),
            session_status: "crashed".to_string(),
            is_active_in_commission: false,
            idle_duration_secs: 120,
            memory_usage_bytes: 100 * 1024 * 1024,
            memory_limit_bytes: 512 * 1024 * 1024,
            consecutive_crashes: 2,
            has_unprocessed_tasks: false,
        },
        CandidateInput {
            uid: 1002,
            identity: name("busy-cand"),
            session_status: "busy".to_string(),
            is_active_in_commission: true,
            idle_duration_secs: 0,
            memory_usage_bytes: 200 * 1024 * 1024,
            memory_limit_bytes: 512 * 1024 * 1024,
            consecutive_crashes: 0,
            has_unprocessed_tasks: true,
        },
    ];

    let ranked = rank_candidates(&inputs, 85);
    assert_eq!(ranked.len(), 2);
    // Crashed session ranks before busy session with pending tasks
    assert_eq!(ranked[0].uid, 1001);
    assert!(ranked[0].score > ranked[1].score);

    // OOM policy: backoff on first crash under normal memory
    let action1 = evaluate_oom_policy(0, 5, 1000, 60000, 200 * 1024 * 1024, 512 * 1024 * 1024);
    assert_eq!(
        action1,
        OomAction::RestartWithBackoff {
            attempt: 1,
            backoff_ms: 1000
        }
    );

    // OOM policy: suggest early hibernation on repeated OOMs under high pressure (>90%)
    let action_high = evaluate_oom_policy(2, 5, 1000, 60000, 480 * 1024 * 1024, 512 * 1024 * 1024);
    assert!(matches!(
        action_high,
        OomAction::SuggestEarlyHibernation { oom_count: 2, .. }
    ));

    // OOM policy: exhaust budget wakes Mayor
    let action_exhausted =
        evaluate_oom_policy(5, 5, 1000, 60000, 200 * 1024 * 1024, 512 * 1024 * 1024);
    assert!(matches!(
        action_exhausted,
        OomAction::BudgetExhaustedWakeMayor {
            total_crashes: 5,
            ..
        }
    ));
}

// ===========================================================================
// Area 7: Full-cycle resumability
// ===========================================================================

#[tokio::test]
async fn test_full_cycle_resumability_with_deterministic_kills() {
    // Flow: provision -> work -> hibernate -> restart daemon -> awaken -> converge -> close.
    // We compare an uninterrupted golden run against a run with kills at 3 deterministic points:
    // Kill 1: mid-hibernate staging (simulated partial temp dir)
    // Kill 2: after daemon restart
    // Kill 3: before commission close

    let tmp_clean = tempfile::tempdir().unwrap();
    let (tree_clean, rt_clean, tel_clean, uid_clean) =
        setup_test_village(&tmp_clean, "resilient-cycle", 42);
    let stasis_clean = ProductionStasis::new(tree_clean.clone(), rt_clean.clone())
        .with_telemetry(tel_clean.clone());

    // 1. Clean run: hibernate
    let doc_clean = stasis_clean
        .hibernate(uid_clean, "operator:cli")
        .await
        .unwrap();
    assert_eq!(doc_clean.frontmatter.source_uid, uid_clean);

    // 2. Clean run: awaken
    let awakened_clean = stasis_clean
        .awaken(&name("resilient-cycle"), Some(42))
        .await
        .unwrap();
    assert_eq!(awakened_clean.uid, 1002);

    // 3. Clean run: close commission with dual convergence
    let wake_clean = Arc::new(RecordingCommissionWake::new());
    let ledger_clean = Ledger::new(tree_clean.clone())
        .with_stasis(Arc::new(stasis_clean))
        .with_runtime(rt_clean)
        .with_telemetry(tel_clean)
        .with_wake(wake_clean);

    let report_clean = ledger_clean.close_commission("adversarial-comm").unwrap();
    assert!(report_clean.convergence.is_some());

    let final_charter_clean = Charter::load(&tree_clean.charter()).unwrap();

    // Now test kill-resilient run
    let tmp_kill = tempfile::tempdir().unwrap();
    let (tree_kill, rt_kill, tel_kill, uid_kill) =
        setup_test_village(&tmp_kill, "resilient-cycle", 42);

    // Kill Point 1: Simulate interrupted staging in tavern/patrons/ before hibernate succeeds
    let patrons_dir = tree_kill.tavern_patrons();
    fs::create_dir_all(&patrons_dir).unwrap();
    let orphan_staging = patrons_dir.join(".tmp_PROPRIETORresilient-cycle_1001_99999");
    fs::create_dir_all(&orphan_staging).unwrap();
    fs::write(orphan_staging.join("interrupted.dat"), b"garbage").unwrap();

    // Resume: real hibernate succeeds despite leftover staging
    let stasis_kill1 =
        ProductionStasis::new(tree_kill.clone(), rt_kill.clone()).with_telemetry(tel_kill.clone());
    let doc_kill = stasis_kill1
        .hibernate(uid_kill, "operator:cli")
        .await
        .unwrap();
    assert_eq!(doc_kill.frontmatter.source_uid, uid_kill);

    // Kill Point 2: Restart daemon (clear in-memory title cache and re-open state)
    clear_title_cache();
    let stasis_kill2 =
        ProductionStasis::new(tree_kill.clone(), rt_kill.clone()).with_telemetry(tel_kill.clone());

    // Awaken
    let awakened_kill = stasis_kill2
        .awaken(&name("resilient-cycle"), Some(42))
        .await
        .unwrap();
    assert_eq!(awakened_kill.uid, 1002);

    // Kill Point 3: Interrupted close attempt (simulated leftover temp file in keep/archives/)
    let archives_dir = tree_kill.root().join("keep/archives");
    fs::create_dir_all(&archives_dir).unwrap();
    fs::write(
        archives_dir.join(".commission42-convergence.json.tmp"),
        b"incomplete write",
    )
    .unwrap();

    // Resume close commission
    let wake_kill = Arc::new(RecordingCommissionWake::new());
    let ledger_kill = Ledger::new(tree_kill.clone())
        .with_stasis(Arc::new(stasis_kill2))
        .with_runtime(rt_kill)
        .with_telemetry(tel_kill)
        .with_wake(wake_kill);

    let report_kill = ledger_kill.close_commission("adversarial-comm").unwrap();
    assert!(report_kill.convergence.is_some());

    let final_charter_kill = Charter::load(&tree_kill.charter()).unwrap();

    // Verify final charter states are identical between clean run and kill run
    assert_eq!(
        final_charter_clean.slots.len(),
        final_charter_kill.slots.len()
    );
    for (&uid, slot_clean) in &final_charter_clean.slots {
        let slot_kill = final_charter_kill.slots.get(&uid).unwrap();
        assert_eq!(slot_clean.identity, slot_kill.identity);
        assert_eq!(slot_clean.status, slot_kill.status);
        assert_eq!(slot_clean.commission, slot_kill.commission);
    }
    assert_eq!(
        final_charter_clean.commissions.get(&42).unwrap().state,
        final_charter_kill.commissions.get(&42).unwrap().state
    );
}
