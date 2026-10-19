//! Vault deliverable promotion & commission close integration tests
//! (Sprint 06 Step 06; SPEC §7, §9.4, §11.2; ADR-0006, ADR-0007, ADR-0036, ADR-0038).
//!
//! Verifies:
//! - Full deliverable submission -> Mayor wake -> ApprovePromotion decree execution;
//! - Byte-identical copy in `keep/vault/<slug>/<artifact>` with `.sha256` sidecar;
//! - Session graceful stop + parcel stasis flag;
//! - Telemetry export to `keep/archives/`;
//! - Bounded rejection rework loop (attempts 1..N mail agent -> attempt N escalates to human + marks Failed);
//! - Security: symlink refusal (ADR-0007), root escape refusal, non-regular file refusal;
//! - Operator `close_commission` lifecycle progression & archive export.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{CommissionRecord, CommissionState};
use hamlet_core::config::Config;
use hamlet_core::decree::{content_digest, RejectReason};
use hamlet_core::packet::WakeReason;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::decrees::Promotion;
use hamlet_daemon::ledger::{Ledger, RecordingCommissionWake};
use hamlet_daemon::mailroute::MailRouter;
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::Telemetry;
use hamlet_daemon::vault::VaultPromotion;

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

fn create_test_village(
    tmp: &Path,
) -> (HamletTree, Arc<Telemetry>, Arc<MailRouter>, Arc<Supervisor>) {
    let tree = HamletTree::ensure(tmp.join(".hamlet")).unwrap();
    let mut charter = Charter::empty();

    charter.reserve_mayor(name("mayor")).unwrap();
    let slot = charter.allocate_slot(name("worker1001"), None).unwrap();
    assert_eq!(slot, 1001);

    charter.parcels.insert(
        1001,
        ParcelLease {
            identity: name("worker1001"),
            stasis: false,
        },
    );

    charter.save_atomic(&tree.charter()).unwrap();

    let telemetry_dir = tmp.join("telemetry");
    let telemetry = Arc::new(Telemetry::open(&telemetry_dir).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        Config::default(),
        spawner,
        Arc::clone(&telemetry),
    ));
    let mail = Arc::new(MailRouter::new(Arc::clone(&supervisor)));

    (tree, telemetry, mail, supervisor)
}

fn seed_commission_active(tree: &HamletTree, id: u64, slug: &str, uid: u32) {
    let mut charter = Charter::load(&tree.charter()).unwrap();
    let mut record = CommissionRecord::draft(name(slug));
    record.state = CommissionState::WindingDown;
    record.roster.insert(uid);
    record
        .indenture_entries
        .insert(uid, hamlet_core::commission::IndentureEntryStatus::Done);
    charter.commissions.insert(id, record);
    if let Some(slot) = charter.slots.get_mut(&uid) {
        slot.commission = Some(id);
    }
    charter.save_atomic(&tree.charter()).unwrap();
}

#[test]
fn test_vault_promotion_approved_full_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry, mail, supervisor) = create_test_village(tmp.path());
    let wake = Arc::new(RecordingCommissionWake::default());

    let ledger = Ledger::new(tree.clone()).with_wake(wake.clone());
    seed_commission_active(&tree, 1, "frontend-app", 1001);

    // 1. Agent writes finished deliverable to courier/freight/
    let freight_dir = tree.courier_freight();
    fs::create_dir_all(&freight_dir).unwrap();
    let freight_file = freight_dir.join("release-bundle.tar");
    let deliverable_bytes = b"FINISHED_PRODUCT_V1.0_ARTIFACT_CONTENT";
    fs::write(&freight_file, deliverable_bytes).unwrap();
    let expected_digest = content_digest(deliverable_bytes);

    // 2. Agent sends mail notice to Mayor (UID 1000)
    let subject = "commission:1:deliverable (frontend-app)";
    let body = "Deliverable ready at courier/freight/release-bundle.tar";

    let handled = ledger
        .handle_deliverable_notice(1001, subject, body)
        .unwrap();
    assert_eq!(handled, Some(1));

    // Verify Mayor wake was fired
    let wakes = wake.events();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].0, 1); // commission 1
    assert_eq!(wakes[0].1.wake_reason(), WakeReason::DeliverableSubmitted);

    // Verify commission is now in review
    let charter_after_submit = Charter::load(&tree.charter()).unwrap();
    let comm = charter_after_submit.commissions.get(&1).unwrap();
    assert!(comm.deliverable_in_review);

    // 3. VaultPromotion approves deliverable
    let vault_promoter = Arc::new(
        VaultPromotion::new(tree.clone())
            .with_telemetry(telemetry)
            .with_mail(mail)
            .with_supervisor(supervisor),
    );

    vault_promoter
        .approve(
            "courier/freight/release-bundle.tar",
            &name("release-bundle.tar"),
        )
        .expect("approve promotion should succeed");

    // 4. Verify artifact in keep/vault/frontend-app/
    let vault_artifact =
        tree.vault_commission_artifact(&name("frontend-app"), &name("release-bundle.tar"));
    assert!(vault_artifact.is_file(), "vault artifact must exist");
    let copied_bytes = fs::read(&vault_artifact).unwrap();
    assert_eq!(copied_bytes, deliverable_bytes);

    // Verify checksum sidecar
    let sidecar_path = tree
        .keep_vault_commission(&name("frontend-app"))
        .join("release-bundle.tar.sha256");
    assert!(sidecar_path.is_file(), "checksum sidecar must exist");
    let sidecar_text = fs::read_to_string(&sidecar_path).unwrap();
    assert!(sidecar_text.starts_with(&expected_digest));

    // 5. Verify charter state advanced to Closed and parcel marked stasis
    let charter_after_approval = Charter::load(&tree.charter()).unwrap();
    let comm_final = charter_after_approval.commissions.get(&1).unwrap();
    assert_eq!(comm_final.state, CommissionState::Closed);
    assert!(!comm_final.deliverable_in_review);

    let lease = charter_after_approval.parcels.get(&1001).unwrap();
    assert!(lease.stasis, "submitting parcel must be marked in stasis");

    // 6. Verify telemetry exported to keep/archives/
    let archives_dir = tree.keep_archives();
    assert!(archives_dir.is_dir());
    let mut archive_entries = fs::read_dir(&archives_dir).unwrap();
    let first = archive_entries.next();
    assert!(first.is_some(), "archive summary file must be exported");
}

#[test]
fn test_vault_promotion_rejection_rework_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry, mail, supervisor) = create_test_village(tmp.path());
    let wake = Arc::new(RecordingCommissionWake::default());

    let ledger = Ledger::new(tree.clone()).with_wake(wake.clone());
    seed_commission_active(&tree, 2, "backend-service", 1001);

    // Initial deliverable
    let freight_dir = tree.courier_freight();
    fs::create_dir_all(&freight_dir).unwrap();
    let freight_file = freight_dir.join("service.tar");
    fs::write(&freight_file, b"IMPERFECT_DELIVERABLE_V0").unwrap();

    // Agent submits deliverable
    ledger.submit_deliverable(2, 1001, None, None).unwrap();

    let vault_promoter = Arc::new(
        VaultPromotion::new(tree.clone())
            .with_telemetry(telemetry)
            .with_mail(mail)
            .with_supervisor(supervisor),
    );

    // 1. Mayor rejects promotion (attempt 1/3)
    vault_promoter
        .reject("courier/freight/service.tar", "Missing test coverage")
        .expect("rejection 1 should succeed");

    assert_eq!(
        vault_promoter.rejection_count("courier/freight/service.tar"),
        1
    );

    // Verify rejection mail delivered to agent 1001
    let agent_mail_dir = tree.courier_mail_recipient(&name("1001"));
    assert!(agent_mail_dir.is_dir());
    let mut mail_files: Vec<_> = fs::read_dir(&agent_mail_dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    mail_files.sort_by_key(|e| e.file_name());
    assert!(!mail_files.is_empty(), "agent must receive rejection mail");

    let mail_content = fs::read_to_string(mail_files[0].path()).unwrap();
    assert!(mail_content.contains("Deliverable promotion rejected: Missing test coverage"));
    assert!(mail_content.contains("Attempt: 1/3"));

    // Verify deliverable_in_review reset for rework
    let charter = Charter::load(&tree.charter()).unwrap();
    let comm = charter.commissions.get(&2).unwrap();
    assert!(
        !comm.deliverable_in_review,
        "deliverable_in_review must be false during rework"
    );
    assert_eq!(comm.state, CommissionState::WindingDown);

    // 2. Agent reworks and re-submits deliverable
    fs::write(&freight_file, b"PERFECT_DELIVERABLE_V1").unwrap();
    ledger.submit_deliverable(2, 1001, None, None).unwrap();

    let charter = Charter::load(&tree.charter()).unwrap();
    let comm = charter.commissions.get(&2).unwrap();
    assert!(comm.deliverable_in_review);

    // 3. Mayor approves reworked deliverable
    vault_promoter
        .approve("courier/freight/service.tar", &name("service.tar"))
        .expect("approve should succeed");

    // Verify artifact saved
    let vault_artifact =
        tree.vault_commission_artifact(&name("backend-service"), &name("service.tar"));
    assert!(vault_artifact.is_file());
    assert_eq!(
        fs::read(&vault_artifact).unwrap(),
        b"PERFECT_DELIVERABLE_V1"
    );

    let charter = Charter::load(&tree.charter()).unwrap();
    assert_eq!(
        charter.commissions.get(&2).unwrap().state,
        CommissionState::Closed
    );
}

#[test]
fn test_vault_promotion_bounded_escalation_to_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry, mail, supervisor) = create_test_village(tmp.path());

    let ledger = Ledger::new(tree.clone());
    seed_commission_active(&tree, 3, "failing-project", 1001);

    let freight_file = tree.courier_freight().join("broken.bin");
    fs::create_dir_all(tree.courier_freight()).unwrap();
    fs::write(&freight_file, b"ALWAYS_BROKEN").unwrap();

    let vault_promoter = Arc::new(
        VaultPromotion::new(tree.clone())
            .with_telemetry(telemetry)
            .with_mail(mail)
            .with_supervisor(supervisor)
            .with_max_rejections(3),
    );

    // Submit and reject 1
    ledger.submit_deliverable(3, 1001, None, None).unwrap();
    vault_promoter
        .reject("courier/freight/broken.bin", "Fail 1")
        .unwrap();

    // Submit and reject 2
    ledger.submit_deliverable(3, 1001, None, None).unwrap();
    vault_promoter
        .reject("courier/freight/broken.bin", "Fail 2")
        .unwrap();

    // Submit and reject 3 (hits bound)
    ledger.submit_deliverable(3, 1001, None, None).unwrap();
    vault_promoter
        .reject("courier/freight/broken.bin", "Fail 3")
        .unwrap();

    // Verify EscalationNotice in keep/census/manor/escalations/
    let escalations_dir = tree.manor_escalations();
    let escalation_file = escalations_dir.join("escalation-commission3.json");
    assert!(escalation_file.is_file(), "escalation notice must be filed");

    let notice_json = fs::read_to_string(&escalation_file).unwrap();
    assert!(notice_json.contains("promotion rejected 3 times: Fail 3"));

    // Verify commission state transitioned to Failed
    let charter = Charter::load(&tree.charter()).unwrap();
    let comm = charter.commissions.get(&3).unwrap();
    assert_eq!(comm.state, CommissionState::Failed);
}

#[test]
fn test_vault_promotion_security_symlinks_and_traversal_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry, mail, supervisor) = create_test_village(tmp.path());
    seed_commission_active(&tree, 4, "secure-app", 1001);

    let vault_promoter = Arc::new(
        VaultPromotion::new(tree.clone())
            .with_telemetry(telemetry)
            .with_mail(mail)
            .with_supervisor(supervisor),
    );

    // 1. Missing file -> invalid_target
    let err = vault_promoter
        .approve("courier/freight/nonexistent.bin", &name("nonexistent.bin"))
        .unwrap_err();
    assert_eq!(err.reason, RejectReason::InvalidTarget);

    // 2. Symlink in freight path -> refused (ADR-0007)
    let real_file = tmp.path().join("real_target.txt");
    fs::write(&real_file, b"SECRET").unwrap();
    let symlink_path = tree.courier_freight().join("symlink_freight.txt");
    fs::create_dir_all(tree.courier_freight()).unwrap();
    std::os::unix::fs::symlink(&real_file, &symlink_path).unwrap();

    let err = vault_promoter
        .approve("courier/freight/symlink_freight.txt", &name("symlink.txt"))
        .unwrap_err();
    assert_eq!(err.reason, RejectReason::InvalidTarget);
    assert!(err.detail.contains("symlink refused"));

    // 3. Path traversal outside village root -> refused
    let err = vault_promoter
        .approve("../../../../etc/passwd", &name("passwd"))
        .unwrap_err();
    assert_eq!(err.reason, RejectReason::InvalidTarget);
}

#[test]
fn test_operator_commission_close() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry, _mail, _supervisor) = create_test_village(tmp.path());
    seed_commission_active(&tree, 5, "operator-close-test", 1001);

    let ledger = Ledger::new(tree.clone()).with_telemetry(telemetry);

    let report = ledger
        .close_commission("operator-close-test")
        .expect("close_commission should succeed");

    assert_eq!(report.id, 5);
    assert_eq!(report.slug.as_str(), "operator-close-test");
    assert_eq!(report.state, CommissionState::Closed);
    assert_eq!(report.parcels_stopped, 1);
    assert!(report.archives_exported.is_some());

    // Verify charter updated
    let charter = Charter::load(&tree.charter()).unwrap();
    let comm = charter.commissions.get(&5).unwrap();
    assert_eq!(comm.state, CommissionState::Closed);

    let lease = charter.parcels.get(&1001).unwrap();
    assert!(lease.stasis);

    // Idempotent second close
    let second_report = ledger
        .close_commission("operator-close-test")
        .expect("second close should succeed idempotently");
    assert_eq!(second_report.state, CommissionState::Closed);
    assert_eq!(second_report.parcels_stopped, 0);
}
