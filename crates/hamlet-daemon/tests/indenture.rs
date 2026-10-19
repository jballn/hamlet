//! Commission decomposition (`INDENTURE.md`) and entry tracking integration
//! tests (Sprint 06 Step 03; SPEC §9.2–§9.3, ADR-0036, CONVENTIONS §9).
//!
//! All tests run against real tempdir `.hamlet/` trees with `FakeRuntime` and
//! `fakeagent` subprocesses (no podman required):
//!
//! - end-to-end decomposition lifecycle: `create_commission` → `CommissionCreated`
//!   Mayor wake → `fakeagent` Mayor emits `ProvisionAgent` decrees +
//!   `INDENTURE.md` → `DecreeExecutor` provisions 2 fake agents (`ada`, `grace`),
//!   validates `INDENTURE.md`, transitions `Draft → Active`, appends indenture
//!   contracts to `PROPRIETOR.md`, delivers daemon-authored indenture mail to
//!   each agent (`Pending → Delivered`), routes agent progress/completion mail
//!   (`Delivered → InProgress → Done`), transitions `Active → WindingDown`, and
//!   wakes the Mayor with `WakeReason::AllEntriesDone`;
//! - invalid-`INDENTURE.md` self-healing retry loop: malformed/unrostered
//!   `INDENTURE.md` is quarantined to `.indenture.rejected`, acked `rejected`
//!   with line-numbered errors, and wakes the Mayor with
//!   `WakeReason::IndentureRejected`; subsequent valid `INDENTURE.md` heals and
//!   activates the commission;
//! - bounded retry escalation: exhausting `DEFAULT_MAX_INDENTURE_RETRIES` (3)
//!   transitions the commission `Draft → Failed` (`EscalationFatal`), writes an
//!   `escalated` ack, and files an `EscalationNotice` in
//!   `keep/census/manor/escalations/`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::commission::{
    CommissionState, IndentureAck, IndentureAckStatus, IndentureEntryStatus,
};
use hamlet_core::config::Config;
use hamlet_core::decree::{
    AckResult, Decree, DecreeAck, DecreeId, EscalationNotice, DECREE_VERSION,
};
use hamlet_core::packet::{EventKind, WakeReason};
use hamlet_core::pins::ToolchainPins;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::bootstrap::script_pinned_digests;
use hamlet_daemon::census::Provisioner;
use hamlet_daemon::decrees::{AgentProvisioner, CensusAgentProvisioner, DecreeExecutor};
use hamlet_daemon::ledger::{
    CommissionWakeBridge, IndentureOutcome, Ledger, RecordingCommissionWake,
    DEFAULT_MAX_INDENTURE_RETRIES,
};
use hamlet_daemon::mailroute::{MailActor, MailRouter};
use hamlet_daemon::mayor::{ManorProvisioner, MayorEngine};
use hamlet_daemon::podman::{FakeRuntime, Runtime};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::Telemetry;

const TEST_MANIFEST_SHA256: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn name(raw: &str) -> SafeName {
    sanitize_name(raw).expect("test name must sanitize")
}

fn scripted_runtime(uids: &[u32]) -> Arc<FakeRuntime> {
    let runtime = Arc::new(FakeRuntime::new());
    let pins = ToolchainPins::default_hamlet().unwrap();
    for uid in uids {
        script_pinned_digests(&runtime, *uid, &pins, TEST_MANIFEST_SHA256);
    }
    runtime
}

fn provision_decree(seq: u64, identity: &str, blueprint: &str, commission: u64) -> Decree {
    let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
    Decree::ProvisionAgent {
        decree_id: placeholder,
        decree_version: DECREE_VERSION,
        blueprint_ref: name(blueprint),
        commission: Some(commission),
        register_ref: name(identity),
    }
    .stamp(seq)
    .unwrap()
}

// ---------------------------------------------------------------------------
// 1. Full end-to-end decomposition + completion lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_decomposition_provisioning_delivery_and_completion_lifecycle() {
    let mut config = Config::default();
    config.sessions.ready_timeout_ms = 10_000;

    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    Charter::empty().save_atomic(&tree.charter()).unwrap();

    let runtime = scripted_runtime(&[1001, 1002]);
    ManorProvisioner::new(
        tree.clone(),
        config.clone(),
        Arc::clone(&runtime) as Arc<dyn Runtime>,
    )
    .provision(&Blueprint::default_village_alpine().unwrap())
    .await
    .expect("manor provisioning must succeed");

    // Seed guild blueprint and register briefs for `ada` and `grace`.
    let blueprint_src =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../defaults/village-alpine.toml");
    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    fs::create_dir_all(bp_path.parent().unwrap()).unwrap();
    fs::copy(&blueprint_src, &bp_path).unwrap();

    fs::write(
        tree.register_brief(&name("ada")),
        "# ada\n\nRole: analytical engine architect.\n",
    )
    .unwrap();
    fs::write(
        tree.register_brief(&name("grace")),
        "# grace\n\nRole: compiler and linker engineer.\n",
    )
    .unwrap();

    // Precompute the two ProvisionAgent decrees and the valid INDENTURE.md that
    // the fake Mayor will emit on its CommissionCreated wake turn.
    let d1 = provision_decree(1, "ada", "village-alpine", 1);
    let d2 = provision_decree(2, "grace", "village-alpine", 1);
    let d1_path = tree
        .manor_decrees()
        .join(d1.id().decree_file_name().unwrap().as_str());
    let d2_path = tree
        .manor_decrees()
        .join(d2.id().decree_file_name().unwrap().as_str());
    let indenture_path = tree.commission_indenture(1);
    let valid_indenture = "\
# INDENTURE

## ada
- goal: Design the watchtower foundation schema
- scope: src/foundation.rs and tests/foundation.rs
- done: cargo test foundation passes and bundle pushed to agent/1001

## grace
- goal: Implement the beacon signal protocol
- scope: src/beacon.rs and tests/beacon.rs
- done: cargo test beacon passes and bundle pushed to agent/1002
";

    let mayor_log = tmp.path().join("fakeagent-mayor.log");
    let mayor_script = tmp.path().join("fakeagent-mayor.json");
    fs::write(
        &mayor_script,
        serde_json::json!({
            "log": mayor_log.to_string_lossy(),
            "emit_files": [
                { "path": d1_path.to_string_lossy(), "content": d1.render().unwrap() },
                { "path": d2_path.to_string_lossy(), "content": d2.render().unwrap() },
                { "path": indenture_path.to_string_lossy(), "content": valid_indenture },
            ],
            "usage": { "input": 12, "output": 8, "totalTokens": 20 },
        })
        .to_string(),
    )
    .unwrap();

    let mayor_engine = Arc::new(
        MayorEngine::new(
            tree.clone(),
            config.clone(),
            Arc::new(HostProcessSpawner::new(env!("CARGO_BIN_EXE_fakeagent"))),
        )
        .with_env(BTreeMap::from([(
            "FAKEAGENT_SCRIPT".to_owned(),
            mayor_script.to_string_lossy().into_owned(),
        )])),
    );

    // Worker spawner logs worker prompts so we can verify the brief addendum.
    let worker_log = tmp.path().join("fakeagent-workers.log");
    let worker_script = tmp.path().join("fakeagent-workers.json");
    fs::write(
        &worker_script,
        serde_json::json!({
            "log": worker_log.to_string_lossy(),
        })
        .to_string(),
    )
    .unwrap();

    struct ScriptedSpawner {
        inner: HostProcessSpawner,
        script_path: String,
    }
    impl SessionSpawner for ScriptedSpawner {
        fn spawn_session<'a>(
            &'a self,
            spec: &'a hamlet_daemon::podman::SessionExecSpec,
        ) -> hamlet_daemon::podman::BoxFuture<
            'a,
            Result<Box<dyn hamlet_daemon::podman::ExecHandle>, hamlet_daemon::podman::PodmanError>,
        > {
            let mut spec = spec.clone();
            spec.env
                .insert("FAKEAGENT_SCRIPT".to_owned(), self.script_path.clone());
            Box::pin(async move { self.inner.spawn_session(&spec).await })
        }
    }

    let telemetry = Arc::new(Telemetry::open(tree.root().join(".telemetry-indenture")).unwrap());
    let worker_spawner: Arc<dyn SessionSpawner> = Arc::new(ScriptedSpawner {
        inner: HostProcessSpawner::new(env!("CARGO_BIN_EXE_fakeagent")),
        script_path: worker_script.to_string_lossy().into_owned(),
    });
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config.clone(),
        worker_spawner,
        Arc::clone(&telemetry),
    ));
    let provisioner = Arc::new(Provisioner::new(
        tree.clone(),
        config.clone(),
        Arc::clone(&runtime) as Arc<dyn Runtime>,
    ));
    let agent_provisioner: Arc<dyn AgentProvisioner> = Arc::new(CensusAgentProvisioner::new(
        provisioner,
        Arc::clone(&supervisor),
        hamlet_core::config::Secrets::default(),
    ));

    let mail_router = Arc::new(MailRouter::new(Arc::clone(&supervisor)));

    let wake_bridge = Arc::new(CommissionWakeBridge::new(Arc::clone(&mayor_engine)));
    let ledger = Arc::new(
        Ledger::new(tree.clone())
            .with_wake(wake_bridge)
            .with_mail_router(Arc::clone(&mail_router)),
    );
    mail_router.set_ledger(Arc::clone(&ledger));

    let executor = DecreeExecutor::new(tree.clone(), config.clone())
        .with_provisioner(agent_provisioner)
        .with_ledger(Arc::clone(&ledger));

    // Step 1: Create commission 1 -> wakes Mayor with CommissionCreated ->
    // fakeagent Mayor emits ProvisionAgent(ada), ProvisionAgent(grace), and INDENTURE.md.
    let brief = "# Commission 1: Watchtower\n\nBuild foundation and beacon.\n";
    let created = ledger
        .create_commission("watchtower", brief, None)
        .expect("commission creation must succeed");
    assert_eq!(created.id, 1);

    let deadline = Instant::now() + Duration::from_secs(10);
    while mayor_engine.sleep_state().turns < 1 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Mayor turn 1 (CommissionCreated)"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        d1_path.is_file() && d2_path.is_file() && indenture_path.is_file(),
        "fakeagent Mayor must have emitted both ProvisionAgent decrees and INDENTURE.md"
    );

    // Clear emit_files on the Mayor script before later wakes so turn 2 doesn't
    // re-write anything.
    fs::write(
        &mayor_script,
        serde_json::json!({
            "log": mayor_log.to_string_lossy(),
            "usage": { "input": 10, "output": 5, "totalTokens": 15 },
        })
        .to_string(),
    )
    .unwrap();

    // Step 2: Run DecreeExecutor::scan() -> executes both ProvisionAgent decrees
    // AND reconciles INDENTURE.md at the end of the pass.
    let report = executor.scan().await.expect("decree scan must succeed");
    assert_eq!(report.executed(), 2);

    let ack1 =
        DecreeAck::parse(&fs::read_to_string(tree.manor_decree_ack(d1.id())).unwrap()).unwrap();
    let ack2 =
        DecreeAck::parse(&fs::read_to_string(tree.manor_decree_ack(d2.id())).unwrap()).unwrap();
    assert_eq!(ack1.result, AckResult::Done);
    assert_eq!(ack2.result, AckResult::Done);

    // Verify INDENTURE.md.ack is `done`.
    let ind_ack =
        IndentureAck::parse(&fs::read_to_string(tree.commission_indenture_ack(1)).unwrap())
            .expect("INDENTURE.md.ack must parse");
    assert_eq!(ind_ack.commission, 1);
    assert_eq!(ind_ack.status, IndentureAckStatus::Done);
    assert_eq!(ind_ack.attempt, 0);

    // Verify charter state: commission 1 is Active, rostered {1001, 1002}, both
    // indenture entries Delivered.
    let charter = Charter::load(&tree.charter()).unwrap();
    let rec = &charter.commissions[&1];
    assert_eq!(rec.state(), CommissionState::Active);
    assert_eq!(rec.roster, BTreeSet::from([1001, 1002]));
    assert_eq!(
        rec.indenture_entries,
        BTreeMap::from([
            (1001, IndentureEntryStatus::Delivered),
            (1002, IndentureEntryStatus::Delivered),
        ])
    );

    // Verify PROPRIETOR.md on both parcels received the indenture contract section.
    let prop_1001 = fs::read_to_string(tree.parcel_proprietor(1001)).unwrap();
    let prop_1002 = fs::read_to_string(tree.parcel_proprietor(1002)).unwrap();
    assert!(prop_1001.contains("## Commission 1 Indenture"));
    assert!(prop_1001.contains("- goal: Design the watchtower foundation schema"));
    assert!(prop_1001.contains("Subject: commission:1:done"));
    assert!(prop_1002.contains("## Commission 1 Indenture"));
    assert!(prop_1002.contains("- goal: Implement the beacon signal protocol"));

    // Wait for both supervised worker sessions to finish readiness and receive
    // both their initial session brief AND their delivered indenture mail on stdin.
    let deadline = Instant::now() + Duration::from_secs(10);
    let worker_prompts = loop {
        let current = fs::read_to_string(&worker_log).unwrap_or_default();
        if current.contains("--- COMMISSION 1 PROTOCOL (parcel1001 · ada) ---")
            && current.contains("--- COMMISSION 1 PROTOCOL (parcel1002 · grace) ---")
            && current.contains("You are assigned to commission1 (watchtower) under `## ada`.")
            && current.contains("You are assigned to commission1 (watchtower) under `## grace`.")
        {
            break current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for worker sessions to receive initial brief and indenture mail:\n{current}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(worker_prompts.contains("Subject: commission:1:done"));

    // Verify daemon-authored indenture mails were routed and acked into both
    // workers' `.read/` mail spools.
    let read_1001: Vec<String> = fs::read_dir(tree.courier_mail_read(&name("1001")))
        .unwrap()
        .flatten()
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .collect();
    let read_1002: Vec<String> = fs::read_dir(tree.courier_mail_read(&name("1002")))
        .unwrap()
        .flatten()
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .collect();
    assert_eq!(read_1001.len(), 1);
    assert!(read_1001[0].contains("Subject: commission:1:indenture (watchtower)"));
    assert!(read_1001[0].contains("Design the watchtower foundation schema"));
    assert!(read_1001[0].contains("/var/git"));
    assert!(read_1001[0].contains("/var/freight/"));
    assert_eq!(read_1002.len(), 1);
    assert!(read_1002[0].contains("Subject: commission:1:indenture (watchtower)"));
    assert!(read_1002[0].contains("Implement the beacon signal protocol"));

    // Idempotence check: re-running reconcile_indenture(s) is a no-op.
    assert_eq!(
        ledger.reconcile_indenture(1).unwrap(),
        IndentureOutcome::Unchanged
    );
    assert!(ledger.reconcile_indentures().unwrap().is_empty());

    // Step 3: Worker 1001 (ada) sends `in_progress` notice to UID 1000.
    mail_router
        .send(
            MailActor::Agent(1001),
            MAYOR_UID,
            "commission:1:in_progress",
            "Started work on src/foundation.rs.\n",
            None,
        )
        .unwrap();
    mail_router.recover_spools().unwrap();

    let detail = ledger.show_commission("watchtower").unwrap();
    assert_eq!(detail.state, CommissionState::Active);
    assert!(
        detail
            .render()
            .contains("entries:    1001 (ada): in_progress, 1002 (grace): delivered"),
        "show_commission must render updated entry statuses:\n{}",
        detail.render()
    );

    // Step 4: Worker 1001 (ada) sends `done` notice to UID 1000.
    mail_router
        .send(
            MailActor::Agent(1001),
            MAYOR_UID,
            "commission:1:done",
            "Foundation schema complete and pushed to agent/1001.\n",
            None,
        )
        .unwrap();
    mail_router.recover_spools().unwrap();

    let detail = ledger.show_commission("watchtower").unwrap();
    assert_eq!(detail.state, CommissionState::Active);
    assert!(
        detail
            .render()
            .contains("entries:    1001 (ada): done, 1002 (grace): delivered"),
        "show_commission must show 1001 done and 1002 delivered:\n{}",
        detail.render()
    );
    assert_eq!(
        mayor_engine.sleep_state().turns,
        1,
        "Mayor must not wake with AllEntriesDone while grace (1002) is still delivered"
    );

    // Step 5: Worker 1002 (grace) sends `done` notice to UID 1000 ->
    // all entries Done -> commission 1 transitions Active -> WindingDown ->
    // Mayor woken with WakeReason::AllEntriesDone!
    mail_router
        .send(
            MailActor::Agent(1002),
            MAYOR_UID,
            "commission:1:done",
            "Beacon protocol complete and pushed to agent/1002.\n",
            None,
        )
        .unwrap();
    mail_router.recover_spools().unwrap();

    let detail = ledger.show_commission("watchtower").unwrap();
    assert_eq!(detail.state, CommissionState::WindingDown);
    assert!(
        detail
            .render()
            .contains("entries:    1001 (ada): done, 1002 (grace): done"),
        "show_commission must show both entries done:\n{}",
        detail.render()
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while mayor_engine.sleep_state().turns < 2 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Mayor turn 2 (AllEntriesDone)"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let mayor_prompts = fs::read_to_string(&mayor_log).unwrap();
    assert!(
        mayor_prompts.contains("wake_reason: all_entries_done"),
        "Mayor turn 2 must carry wake_reason: all_entries_done:\n{mayor_prompts}"
    );
    assert!(
        mayor_prompts.contains("kind: all_entries_done"),
        "Mayor turn 2 must carry kind: all_entries_done:\n{mayor_prompts}"
    );
    assert!(
        mayor_prompts.contains("## ada")
            && mayor_prompts.contains("## grace")
            && mayor_prompts.contains("winding_down"),
        "AllEntriesDone wake packet must carry INDENTURE.md body and winding_down state:\n{mayor_prompts}"
    );

    supervisor.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2. Invalid INDENTURE.md self-healing retry loop
// ---------------------------------------------------------------------------

#[test]
fn invalid_indenture_triggers_self_healing_retry_loop_then_succeeds_on_correction() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter.reserve_mayor(name("manor")).unwrap();
    charter.save_atomic(&tree.charter()).unwrap();

    let spy = Arc::new(RecordingCommissionWake::new());
    let telemetry = Arc::new(Telemetry::open(tree.root().join(".telemetry-retry")).unwrap());
    let spawner: Arc<dyn SessionSpawner> =
        Arc::new(HostProcessSpawner::new(env!("CARGO_BIN_EXE_fakeagent")));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        Config::default(),
        spawner,
        Arc::clone(&telemetry),
    ));
    let mail_router = Arc::new(MailRouter::new(supervisor));
    let ledger = Arc::new(
        Ledger::new(tree.clone())
            .with_wake(spy.clone())
            .with_mail_router(Arc::clone(&mail_router)),
    );
    mail_router.set_ledger(Arc::clone(&ledger));

    ledger
        .create_commission(
            "aqueduct",
            "# Commission 1: Aqueduct\n\nRepair arches.\n",
            None,
        )
        .unwrap();

    // Allocate and roster ada (1001) on commission 1.
    let mut charter = Charter::load(&tree.charter()).unwrap();
    let uid_ada = charter.allocate_slot(name("ada"), Some(1)).unwrap();
    charter.assign_uid(1, uid_ada).unwrap();
    charter.save_atomic(&tree.charter()).unwrap();
    assert_eq!(uid_ada, 1001);

    // Attempt 1: Malformed INDENTURE.md (missing `- done:` bullet).
    let malformed = "\
# INDENTURE

## ada
- goal: Survey the western arches
- scope: src/survey.rs
";
    fs::write(tree.commission_indenture(1), malformed).unwrap();

    let outcome1 = ledger.reconcile_indenture(1).unwrap();
    assert!(
        matches!(outcome1, IndentureOutcome::Rejected { attempt: 1, .. }),
        "expected Rejected(attempt=1), got {outcome1:?}"
    );

    // Malformed INDENTURE.md was quarantined to `.indenture.rejected` and acked.
    assert!(!tree.commission_indenture(1).exists());
    assert_eq!(
        fs::read_to_string(tree.commission(1).join(".indenture.rejected")).unwrap(),
        malformed
    );
    let ack1 = IndentureAck::parse(&fs::read_to_string(tree.commission_indenture_ack(1)).unwrap())
        .unwrap();
    assert_eq!(ack1.status, IndentureAckStatus::Rejected);
    assert_eq!(ack1.attempt, 1);
    assert!(
        ack1.error.as_deref().unwrap_or_default().contains("line"),
        "rejection error must include line-numbered parse failure: {ack1:?}"
    );

    // Commission stays in Draft and Mayor was woken with IndentureRejected.
    assert_eq!(
        ledger.show_commission("aqueduct").unwrap().state,
        CommissionState::Draft
    );
    let events = spy.events();
    assert_eq!(events.len(), 2); // 0 = CommissionCreated, 1 = IndentureRejected(attempt 1)
    assert_eq!(
        events[1].1.kind(),
        &EventKind::IndentureRejected {
            commission: 1,
            attempt: 1
        }
    );
    assert_eq!(events[1].1.wake_reason(), WakeReason::IndentureRejected);
    assert!(events[1].1.body().contains("INDENTURE.md rejected"));

    // Attempt 2: Mayor writes corrected INDENTURE.md for rostered `ada`.
    let corrected = "\
# INDENTURE

## ada
- goal: Survey the western arches
- scope: src/survey.rs
- done: cargo test survey passes
";
    fs::write(tree.commission_indenture(1), corrected).unwrap();

    let outcome2 = ledger.reconcile_indenture(1).unwrap();
    assert_eq!(
        outcome2,
        IndentureOutcome::Posted {
            commission: 1,
            delivered_uids: vec![1001]
        }
    );

    let ack2 = IndentureAck::parse(&fs::read_to_string(tree.commission_indenture_ack(1)).unwrap())
        .unwrap();
    assert_eq!(ack2.status, IndentureAckStatus::Done);
    assert_eq!(ack2.attempt, 0);

    let detail = ledger.show_commission("aqueduct").unwrap();
    assert_eq!(detail.state, CommissionState::Active);
    assert_eq!(detail.indenture_entries.len(), 1);
    assert_eq!(detail.indenture_entries[0].uid, 1001);
    assert_eq!(
        detail.indenture_entries[0].status,
        IndentureEntryStatus::Delivered
    );
}

// ---------------------------------------------------------------------------
// 3. Bounded retry exhaustion -> escalation notice + Failed state
// ---------------------------------------------------------------------------

#[test]
fn bounded_indenture_retries_exhaust_into_fatal_escalation_and_failed_state() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter.reserve_mayor(name("manor")).unwrap();
    charter.save_atomic(&tree.charter()).unwrap();

    let spy = Arc::new(RecordingCommissionWake::new());
    let ledger = Ledger::new(tree.clone()).with_wake(spy.clone());
    assert_eq!(DEFAULT_MAX_INDENTURE_RETRIES, 3);

    ledger
        .create_commission(
            "harbor",
            "# Commission 1: Harbor\n\nDredge the channel.\n",
            None,
        )
        .unwrap();

    // Three consecutive bad INDENTURE.md submissions:
    // 1. bad header
    // 2. references unprovisioned agent `ghost`
    // 3. missing `- scope:` line
    let bad_payloads = [
        "# WRONG_HEADER\n",
        "# INDENTURE\n\n## ghost\n- goal: g\n- scope: s\n- done: d\n",
        "# INDENTURE\n\n## ghost\n- goal: g\n- done: d\n",
    ];

    for (idx, payload) in bad_payloads.iter().enumerate() {
        let expected_attempt = (idx + 1) as u32;
        fs::write(tree.commission_indenture(1), payload).unwrap();
        let outcome = ledger.reconcile_indenture(1).unwrap();

        if expected_attempt < DEFAULT_MAX_INDENTURE_RETRIES {
            assert!(
                matches!(
                    outcome,
                    IndentureOutcome::Rejected {
                        attempt,
                        ..
                    } if attempt == expected_attempt
                ),
                "attempt {expected_attempt} must be Rejected, got {outcome:?}"
            );
            assert_eq!(
                ledger.show_commission("harbor").unwrap().state,
                CommissionState::Draft
            );
        } else {
            assert!(
                matches!(
                    outcome,
                    IndentureOutcome::Escalated {
                        attempt,
                        ..
                    } if attempt == DEFAULT_MAX_INDENTURE_RETRIES
                ),
                "final attempt {expected_attempt} must Escalate, got {outcome:?}"
            );
        }
    }

    // After attempt 3:
    // 1. INDENTURE.md.ack is `escalated` with attempt == 3.
    let ack = IndentureAck::parse(&fs::read_to_string(tree.commission_indenture_ack(1)).unwrap())
        .unwrap();
    assert_eq!(ack.status, IndentureAckStatus::Escalated);
    assert_eq!(ack.attempt, DEFAULT_MAX_INDENTURE_RETRIES);

    // 2. Commission 1 transitioned Draft -> Failed via EscalationFatal.
    let detail = ledger.show_commission("harbor").unwrap();
    assert_eq!(detail.state, CommissionState::Failed);

    // 3. An EscalationNotice was written into keep/census/manor/escalations/.
    let esc_dir = tree.manor_escalations();
    let esc_files: Vec<_> = fs::read_dir(&esc_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(
        esc_files.len(),
        1,
        "exactly one escalation notice must be filed in manor/escalations/"
    );
    let notice = EscalationNotice::parse(&fs::read_to_string(&esc_files[0]).unwrap())
        .expect("escalation notice must parse cleanly");
    assert!(
        notice
            .summary
            .contains("commission1 INDENTURE.md failed validation after 3 attempts"),
        "unexpected escalation summary: {}",
        notice.summary
    );
    assert_eq!(notice.evidence_refs.len(), 2);

    // 4. Wakes fired: 1 CommissionCreated + 2 IndentureRejected (attempt 3 escalated instead of re-waking).
    let events = spy.events();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].1.wake_reason(), WakeReason::CommissionCreated);
    assert_eq!(
        events[1].1.kind(),
        &EventKind::IndentureRejected {
            commission: 1,
            attempt: 1
        }
    );
    assert_eq!(
        events[2].1.kind(),
        &EventKind::IndentureRejected {
            commission: 1,
            attempt: 2
        }
    );
}
