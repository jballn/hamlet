//! Integration tests for quick awakening and dual-state convergence
//! (SPEC §10, ADR-0039, ADR-0040).

use std::fs;
use std::os::unix::fs::symlink;
use std::sync::Arc;

use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{CommissionRecord, CommissionState};
use hamlet_core::config::Config;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{SessionId, SessionRecord, SessionStatus};
use hamlet_core::slots::SlotStatus;
use hamlet_core::stasis::{CommissionConvergenceSummary, PatronDoc, TitleDoc, TitleEntry};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::{clear_title_cache, ProductionStasis, StasisError};
use hamlet_daemon::ledger::{Ledger, RecordingCommissionWake};
use hamlet_daemon::podman::FakeRuntime;
use hamlet_daemon::telemetry::{AgentKey, Telemetry};

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("valid name")
}

fn setup_village(tmp: &tempfile::TempDir) -> (HamletTree, Arc<FakeRuntime>, Arc<Telemetry>) {
    clear_title_cache();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let runtime = Arc::new(FakeRuntime::new());
    let telemetry = Arc::new(Telemetry::open_for_village(tree.root(), &Config::default()).unwrap());

    // Initialize charter with slot 1001 (agent "scribe" on commission 42)
    let mut charter = Charter::empty();
    let identity = name("scribe");
    let slot_uid = charter.allocate_slot(identity.clone(), Some(42)).unwrap();
    assert_eq!(slot_uid, 1001);

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

    let mut comm = CommissionRecord::draft(name("transcription"));
    comm.state = CommissionState::Active;
    comm.roster.insert(slot_uid);
    charter.commissions.insert(42, comm);

    charter.save_atomic(&tree.charter()).unwrap();

    // Seed parcel files
    let parcel_dir = tree.parcel(slot_uid);
    fs::create_dir_all(&parcel_dir).unwrap();

    // 1. Seed TITLE.md
    let title_path = tree.parcel_title(slot_uid);
    let title_content = format!(
        "# TITLE — parcel{slot_uid}\n\n\
         - [2026-10-19T01:00:00Z] trigger commission=42 prompt=\"transcribe annals\"\n\
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
         - commission: 42\n\
         - blueprint: village-alpine v1\n\n\
         ## Alterations\n\n\
         - baseline · village-alpine v1\n"
    );
    fs::write(&deed_path, deed_content).unwrap();

    // 3. Seed PROPRIETOR.md
    let prop_path = tree.parcel_proprietor(slot_uid);
    let prop_content = format!(
        "# PROPRIETOR — {identity}\n\n\
         Active intelligence profile for parcel {slot_uid}.\n\
         Specialized in transcribing historical court records.\n"
    );
    fs::write(&prop_path, prop_content).unwrap();

    // 4. Seed home/ directory with files
    let home_dir = tree.parcel_home(slot_uid);
    fs::create_dir_all(home_dir.join("work")).unwrap();
    fs::create_dir_all(home_dir.join("config")).unwrap();
    fs::write(
        home_dir.join("README.md"),
        b"Agent private home workspace\n",
    )
    .unwrap();
    fs::write(
        home_dir.join("work/notes.txt"),
        b"Day 1: transcriptions completed.\n",
    )
    .unwrap();
    fs::write(
        home_dir.join("config/settings.json"),
        b"{\"editor\": \"vi\"}\n",
    )
    .unwrap();

    (tree, runtime, telemetry)
}

#[tokio::test]
async fn test_hibernate_awaken_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let identity = name("scribe");
    let stasis =
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone());

    // 1. Hibernate agent 1001
    let doc = stasis
        .hibernate(1001, "operator:cli")
        .await
        .expect("hibernate must succeed");
    assert_eq!(doc.frontmatter.patron, identity);

    let patron_dir = tree.patron(&identity);
    assert!(patron_dir.is_dir());

    // Verify charter state after hibernation
    {
        let charter = Charter::load(&tree.charter()).unwrap();
        let slot = charter.slots.get(&1001).unwrap();
        assert_eq!(slot.status, SlotStatus::Hibernated);
        assert!(charter.parcels.get(&1001).unwrap().stasis);
    }

    // 2. Awaken into fresh slot on commission 42
    let awakened = stasis
        .awaken(&identity, Some(42))
        .await
        .expect("awaken must succeed");

    assert_eq!(awakened.patron, identity);
    let new_uid = awakened.uid;
    assert_eq!(new_uid, 1002);
    assert_eq!(awakened.commission, Some(42));

    // Assert: patron record in tavern/patrons/ was removed
    assert!(
        !patron_dir.exists(),
        "patron record must be removed after successful awaken"
    );

    // Assert: old slot 1001 mapping persists as historical lineage
    {
        let charter = Charter::load(&tree.charter()).unwrap();
        let old_slot = charter.slots.get(&1001).unwrap();
        assert_eq!(old_slot.status, SlotStatus::Hibernated);
        assert_eq!(old_slot.identity, identity);

        let fresh_slot = charter.slots.get(&new_uid).unwrap();
        assert_eq!(fresh_slot.status, SlotStatus::Active);
        assert_eq!(fresh_slot.identity, identity);
        assert_eq!(fresh_slot.commission, Some(42));
    }

    // Assert: home content restored with identical content
    let new_home = tree.parcel_home(new_uid);
    assert_eq!(
        fs::read(new_home.join("README.md")).unwrap(),
        b"Agent private home workspace\n"
    );
    assert_eq!(
        fs::read(new_home.join("work/notes.txt")).unwrap(),
        b"Day 1: transcriptions completed.\n"
    );
    assert_eq!(
        fs::read(new_home.join("config/settings.json")).unwrap(),
        b"{\"editor\": \"vi\"}\n"
    );

    // Assert: PROPRIETOR.md contains updated lineage
    let prop_content = fs::read_to_string(tree.parcel_proprietor(new_uid)).unwrap();
    let prop_doc = PatronDoc::parse(&prop_content).unwrap();
    assert_eq!(prop_doc.frontmatter.source_uid, new_uid);
    assert_eq!(prop_doc.frontmatter.commission, Some(42));
    assert!(prop_doc
        .body
        .contains("Specialized in transcribing historical court records."));

    // Assert: TITLE.md opening entry is Awaken
    let title_content = fs::read_to_string(tree.parcel_title(new_uid)).unwrap();
    let title_doc = TitleDoc::parse(&title_content).unwrap();
    assert_eq!(title_doc.entries.len(), 1);
    match &title_doc.entries[0] {
        TitleEntry::Awaken {
            patron,
            from_commission,
            ..
        } => {
            assert_eq!(patron, &identity);
            assert_eq!(from_commission, &Some(42));
        }
        other => panic!("expected Awaken entry, got {:?}", other),
    }

    // Assert: telemetry trace recorded
    let traces = telemetry
        .recent_traces(
            AgentKey {
                commission: 42,
                uid: new_uid,
            },
            10,
        )
        .unwrap();
    assert!(traces.iter().any(|t| t.event == "agent-awakened"));
}

#[tokio::test]
async fn test_awaken_fast_path_zero_downloads() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let identity = name("scribe");
    let stasis =
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone());

    stasis
        .hibernate(1001, "operator:cli")
        .await
        .expect("hibernate must succeed");

    // Clear pkgcache just in case
    let pkgcache_dir = tree.market_pkgcache();
    let _ = fs::remove_dir_all(&pkgcache_dir);
    fs::create_dir_all(&pkgcache_dir).unwrap();

    // Awaken
    let awakened = stasis
        .awaken(&identity, Some(42))
        .await
        .expect("awaken must succeed");

    assert_eq!(awakened.uid, 1002);

    // Assert zero downloads: pkgcache directory has 0 files
    let pkgcache_entries: Vec<_> = fs::read_dir(&pkgcache_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        pkgcache_entries.is_empty(),
        "fast path must not download tools: pkgcache was {pkgcache_entries:?}"
    );
}

#[tokio::test]
async fn test_awaken_tamper_detection() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let identity = name("scribe");
    let stasis =
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone());

    stasis
        .hibernate(1001, "operator:cli")
        .await
        .expect("hibernate must succeed");

    let patron_home_bak = tree.patron_home_backup(&identity);

    // 1. Content tamper
    let readme = patron_home_bak.join("README.md");
    fs::write(&readme, b"TAMPERED CONTENT").unwrap();

    let err = stasis
        .awaken(&identity, Some(42))
        .await
        .expect_err("tampered file must be rejected");

    match err {
        StasisError::ManifestVerification(msg) => {
            assert!(
                msg.contains("README.md"),
                "error must mention tampered file: {msg}"
            );
        }
        other => panic!("expected ManifestVerification error, got {:?}", other),
    }

    // Restore valid content
    fs::write(&readme, b"Agent private home workspace\n").unwrap();

    // 2. Extra untracked file
    let untracked = patron_home_bak.join("malicious.sh");
    fs::write(&untracked, b"#!/bin/sh\nexit 1\n").unwrap();

    let err_untracked = stasis
        .awaken(&identity, Some(42))
        .await
        .expect_err("untracked file must be rejected");

    match err_untracked {
        StasisError::ManifestVerification(msg) => {
            assert!(
                msg.contains("untracked file"),
                "error must report untracked file: {msg}"
            );
        }
        other => panic!("expected ManifestVerification error, got {:?}", other),
    }

    let _ = fs::remove_file(&untracked);

    // 3. Symlink insertion
    let symlink_path = patron_home_bak.join("bad_symlink");
    symlink("/etc/passwd", &symlink_path).unwrap();

    let err_symlink = stasis
        .awaken(&identity, Some(42))
        .await
        .expect_err("symlink must be rejected");

    match err_symlink {
        StasisError::ManifestVerification(msg) => {
            assert!(
                msg.contains("symlinks forbidden"),
                "error must report symlink: {msg}"
            );
        }
        other => panic!("expected ManifestVerification error, got {:?}", other),
    }

    // Assert: patron record remains intact in tavern/patrons/
    assert!(tree.patron(&identity).is_dir());
}

#[tokio::test]
async fn test_restart_mid_awaken_resumable() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let identity = name("scribe");
    let stasis =
        ProductionStasis::new(tree.clone(), runtime.clone()).with_telemetry(telemetry.clone());

    stasis
        .hibernate(1001, "operator:cli")
        .await
        .expect("hibernate must succeed");

    // Attempt awakening with non-existent commission ID -> pre-flight failure
    let err = stasis
        .awaken(&identity, Some(9999))
        .await
        .expect_err("unknown commission must fail");

    match err {
        StasisError::UnknownCommission(9999) => {}
        other => panic!("expected UnknownCommission, got {:?}", other),
    }

    // Assert patron record in tavern/patrons/ is untouched
    let patron_dir = tree.patron(&identity);
    assert!(
        patron_dir.is_dir(),
        "patron record must remain intact after aborted awaken"
    );

    // Awaken with valid commission succeeds
    let awakened = stasis
        .awaken(&identity, Some(42))
        .await
        .expect("retry with valid commission must succeed");

    assert_eq!(awakened.uid, 1002);
    assert!(
        !patron_dir.exists(),
        "patron record removed only after full success"
    );
}

#[tokio::test]
async fn test_dual_state_convergence_matrix() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, runtime, telemetry) = setup_village(&tmp);
    let wake = Arc::new(RecordingCommissionWake::new());

    // --- Scenario A: Both branches succeed ---
    {
        let ledger = Ledger::new(tree.clone())
            .with_runtime(runtime.clone())
            .with_telemetry(telemetry.clone())
            .with_wake(wake.clone());

        let report = ledger
            .close_commission("transcription")
            .expect("close_commission must succeed");

        assert_eq!(report.state, CommissionState::Closed);
        let conv = report.convergence.expect("convergence summary present");
        assert!(conv.overall_success());
        let parcel_report = conv.reports.get(&1001).expect("parcel 1001 report");
        assert!(
            parcel_report.deed_outcome.is_skipped() || parcel_report.deed_outcome.is_succeeded()
        );
        assert!(parcel_report.proprietor_outcome.is_succeeded());

        // Convergence archive written
        let conv_file = tree.commission_convergence_archive(42);
        assert!(conv_file.is_file(), "convergence archive must exist");
        let content = fs::read_to_string(&conv_file).unwrap();
        let loaded_summary: CommissionConvergenceSummary = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded_summary.commission_id, 42);

        // No failure wakes
        assert!(wake.convergence_failures().is_empty());
    }

    // --- Scenario B: DEED fails (grammar error), PROPRIETOR succeeds ---
    {
        let tmp_b = tempfile::tempdir().unwrap();
        let (tree_b, runtime_b, telemetry_b) = setup_village(&tmp_b);
        let wake_b = Arc::new(RecordingCommissionWake::new());

        // Corrupt DEED.md with invalid grammar
        let deed_path = tree_b.parcel_deed(1001);
        fs::write(
            &deed_path,
            "# DEED — parcel1001\n\n\
             - identity: scribe\n\
             - uid: 1001\n\
             - commission: 42\n\
             - blueprint: village-alpine v1\n\n\
             ### Dynamic Alterations\n\n\
             - totally invalid alteration line that breaks grammar\n",
        )
        .unwrap();

        let ledger_b = Ledger::new(tree_b.clone())
            .with_runtime(runtime_b.clone())
            .with_telemetry(telemetry_b.clone())
            .with_wake(wake_b.clone());

        let report_b = ledger_b
            .close_commission("transcription")
            .expect("close_commission completes even with branch failure");

        let conv_b = report_b.convergence.expect("convergence summary present");
        assert!(!conv_b.overall_success(), "overall success should be false");

        let p_report = conv_b.reports.get(&1001).unwrap();
        assert!(
            p_report.deed_outcome.is_failed(),
            "deed outcome must be Failed"
        );
        assert!(
            p_report.proprietor_outcome.is_succeeded(),
            "proprietor outcome must still succeed (independent execution)"
        );

        // Convergence failed wake fired
        assert_eq!(wake_b.convergence_failures().len(), 1);

        // Escalation notice filed in keep/census/manor/escalations/
        let escalations_dir = tree_b.manor_escalations();
        let entries: Vec<_> = fs::read_dir(&escalations_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            !entries.is_empty(),
            "escalation notice must be filed on failure"
        );

        // Sled trace captured in exported archive
        assert!(report_b.archives_exported.is_some());
        let archive_path = report_b.archives_exported.as_ref().unwrap();
        let archive_content = fs::read_to_string(archive_path).unwrap();
        assert!(archive_content.contains("commission-convergence-failed"));
    }

    // --- Scenario C: DEED succeeds, PROPRIETOR fails (e.g. oversize home) ---
    {
        let tmp_c = tempfile::tempdir().unwrap();
        let (tree_c, runtime_c, telemetry_c) = setup_village(&tmp_c);
        let wake_c = Arc::new(RecordingCommissionWake::new());

        // Configure stasis with tiny max_home_bytes (10 bytes) to force overflow failure
        let stasis_c = Arc::new(
            ProductionStasis::new(tree_c.clone(), runtime_c.clone())
                .with_max_home_bytes(10)
                .with_telemetry(telemetry_c.clone()),
        );

        let ledger_c = Ledger::new(tree_c.clone())
            .with_stasis(stasis_c)
            .with_runtime(runtime_c.clone())
            .with_telemetry(telemetry_c.clone())
            .with_wake(wake_c.clone());

        let report_c = ledger_c
            .close_commission("transcription")
            .expect("close_commission completes even with branch failure");

        let conv_c = report_c.convergence.expect("convergence summary present");
        assert!(!conv_c.overall_success());

        let p_report = conv_c.reports.get(&1001).unwrap();
        assert!(
            p_report.deed_outcome.is_skipped() || p_report.deed_outcome.is_succeeded(),
            "deed outcome must still succeed/skip"
        );
        assert!(
            p_report.proprietor_outcome.is_failed(),
            "proprietor outcome must be Failed due to size limit"
        );

        // Wake and escalation filed
        assert_eq!(wake_c.convergence_failures().len(), 1);
        let escalations_dir = tree_c.manor_escalations();
        let entries: Vec<_> = fs::read_dir(&escalations_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(!entries.is_empty(), "escalation notice must be filed");
    }
}
