//! DEED dynamic-alterations → blueprint evolution integration tests
//! (Sprint 06 Step 05; SPEC §5, §9.4, §10; ADR-0038, CONVENTIONS §9).
//!
//! Verifies:
//! - golden DEED blocks → blueprint version diffs + provenance headers;
//! - rejection matrix: unknown kinds, shell metachars, path separators, invalid versions,
//!   non-LF line endings, duplicate tools, multiple sections → mail report + sled trace;
//! - idempotence: re-promoting already-merged alterations is a no-op (content-hash dedupe);
//! - atomic writes & symlink refusals (ADR-0006, ADR-0007);
//! - notify watcher: `Reaction::DeedMutation` updates incremental parsed view;
//! - ledger triggers: entry-completion (default) vs commission wind-down.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use hamlet_core::blueprint::{Blueprint, DEFAULT_BLUEPRINT_TOML};
use hamlet_core::charter::Charter;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
use hamlet_core::watch::Reaction;
use hamlet_daemon::ledger::{Ledger, RecordingCommissionWake};
use hamlet_daemon::promote::{spawn_deed_plane, DeedPromoter, PromotionOutcome, PromotionTrigger};
use hamlet_daemon::telemetry::Telemetry;
use hamlet_daemon::watch::{Dispatched, Dispatcher};

fn name(s: &str) -> hamlet_core::sanitize::SafeName {
    sanitize_name(s).unwrap()
}

fn seed_village(tmp: &Path) -> (HamletTree, Arc<Telemetry>) {
    let tree = HamletTree::ensure(tmp.join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    let _ = charter.allocate_slot(name("worker1001"), None).unwrap();
    let _ = charter.allocate_slot(name("worker1002"), None).unwrap();
    charter.save_atomic(&tree.charter()).unwrap();

    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    fs::create_dir_all(bp_path.parent().unwrap()).unwrap();
    fs::write(&bp_path, DEFAULT_BLUEPRINT_TOML).unwrap();

    let telemetry_dir = tmp.join("telemetry");
    let telemetry = Arc::new(Telemetry::open(&telemetry_dir).unwrap());
    (tree, telemetry)
}

fn write_parcel_deed(tree: &HamletTree, uid: u32, commission: u64, alterations: &str) {
    let parcel_dir = tree.parcel(uid);
    fs::create_dir_all(&parcel_dir).unwrap();

    let content = format!(
        "# DEED — environment record · parcel{uid}\n\n\
- identity: worker{uid}\n\
- uid slot: {uid}\n\
- commission: {commission}\n\
- blueprint: village-alpine v1 · base image docker.io/library/alpine:3.24\n\n\
Tool installs and environment mutations are recorded here.\n\n\
### Dynamic Alterations\n\
- baseline · toolchain bootstrap · uid {uid} · image localhost/hamlet/village-alpine:v1\n\
  - manifest: /home/{uid}/.hamlet/manifest.json · sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n\
  - tool: pixi/pixi 0.34.* → 0.34.0 · pixi@0.34.0 sha256 0123\n\
  - image layer (apk): uutils-coreutils, git, acl, ca-certificates, curl\n\
  - env keys: PIXI_CACHE_DIR, LANG (values never recorded — SPEC §11.4)\n\n\
{alterations}\n"
    );

    fs::write(tree.parcel_deed(uid), content).unwrap();
}

#[test]
fn golden_deed_blocks_evolve_blueprint_with_version_diffs_and_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry) = seed_village(tmp.path());

    let promoter = Arc::new(DeedPromoter::new(tree.clone()).with_telemetry(telemetry));

    // 1. Initial blueprint is v1
    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    let bp_v1_text = fs::read_to_string(&bp_path).unwrap();
    let bp_v1 = Blueprint::from_toml(&bp_v1_text).unwrap();
    assert_eq!(bp_v1.meta().version(), 1);
    let initial_tools_count = bp_v1.tools().len();

    // 2. Parcel 1001 DEED carries valid alterations (new tools)
    let alterations = "\
- pixi uv = 0.4.2
- cargo ripgrep
- apk build-base
";
    write_parcel_deed(&tree, 1001, 1, alterations);

    // 3. Promote parcel 1001
    let outcome = promoter.promote_parcel(1001).unwrap();
    assert_eq!(
        outcome,
        PromotionOutcome::Promoted {
            blueprint: name("village-alpine"),
            old_version: 1,
            new_version: 2,
            alterations_count: 3,
        }
    );

    // 4. Verify version snapshots and active blueprint on disk
    let v1_snap_path = tree.guild_blueprint_version(&name("village-alpine"), 1);
    let v2_snap_path = tree.guild_blueprint_version(&name("village-alpine"), 2);
    assert!(v1_snap_path.exists(), "v1 snapshot must exist");
    assert!(v2_snap_path.exists(), "v2 snapshot must exist");

    let bp_v2_text = fs::read_to_string(&bp_path).unwrap();
    let bp_v2 = Blueprint::from_toml(&bp_v2_text).unwrap();
    assert_eq!(bp_v2.meta().version(), 2);
    assert_eq!(bp_v2.tools().len(), initial_tools_count + 3);

    // Check individual tools
    let uv = bp_v2.tools().iter().find(|t| t.name() == "uv").unwrap();
    assert_eq!(uv.version_spec(), Some("0.4.2"));
    let rg = bp_v2
        .tools()
        .iter()
        .find(|t| t.name() == "ripgrep")
        .unwrap();
    assert_eq!(rg.version_spec(), None);
    let bb = bp_v2
        .tools()
        .iter()
        .find(|t| t.name() == "build-base")
        .unwrap();
    assert_eq!(bb.version_spec(), None);

    // Check provenance header
    assert!(
        bp_v2_text.contains("# Provenance: promoted from parcel1001 on commission 1"),
        "provenance header must record lineage: {bp_v2_text}"
    );

    // 5. Evolution from v2 → v3: parcel 1002 updates tool version
    let alterations_v3 = "\
- pixi uv = 0.5.0
";
    write_parcel_deed(&tree, 1002, 1, alterations_v3);

    let outcome_v3 = promoter.promote_parcel(1002).unwrap();
    assert_eq!(
        outcome_v3,
        PromotionOutcome::Promoted {
            blueprint: name("village-alpine"),
            old_version: 2,
            new_version: 3,
            alterations_count: 1,
        }
    );

    let v3_snap_path = tree.guild_blueprint_version(&name("village-alpine"), 3);
    assert!(v3_snap_path.exists(), "v3 snapshot must exist");

    let bp_v3_text = fs::read_to_string(&bp_path).unwrap();
    let bp_v3 = Blueprint::from_toml(&bp_v3_text).unwrap();
    assert_eq!(bp_v3.meta().version(), 3);
    let uv_v3 = bp_v3.tools().iter().find(|t| t.name() == "uv").unwrap();
    assert_eq!(uv_v3.version_spec(), Some("0.5.0"));
}

#[test]
fn rejection_matrix_unknown_tool_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry) = seed_village(tmp.path());
    let promoter = Arc::new(DeedPromoter::new(tree.clone()).with_telemetry(telemetry));

    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    let before_text = fs::read_to_string(&bp_path).unwrap();

    // Unknown tool kind `npm`
    write_parcel_deed(&tree, 1001, 1, "- npm express = 4.18.2\n");

    let outcome = promoter.promote_parcel(1001).unwrap();
    match outcome {
        PromotionOutcome::Rejected { uid, reason } => {
            assert_eq!(uid, 1001);
            assert!(
                reason.contains("unknown tool kind \"npm\""),
                "reason must mention unknown kind: {reason}"
            );
        }
        other => panic!("expected rejection, got {other:?}"),
    }

    // Blueprint must be unchanged
    let after_text = fs::read_to_string(&bp_path).unwrap();
    assert_eq!(
        before_text, after_text,
        "blueprint must not be modified on rejection"
    );

    // Rejection report mail must be delivered to parcel1001
    let mail_dir = tree.courier_mail_recipient(&name("1001"));
    let mails: Vec<_> = fs::read_dir(&mail_dir)
        .unwrap()
        .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert_eq!(mails.len(), 1, "rejection mail must be delivered");
    assert!(mails[0].contains("Subject: commission:1:deed:rejected"));
    assert!(mails[0].contains("unknown tool kind"));
}

#[test]
fn rejection_matrix_injections_and_metacharacters() {
    let cases = [
        ("- pixi uv;rm -rf /\n", "invalid tool name"),
        ("- pixi uv$(whoami)\n", "invalid tool name"),
        ("- pixi ../evil\n", "invalid tool name"),
        ("- apk usr/bin/sh\n", "invalid tool name"),
        ("- pixi uv = 1.0;malicious\n", "invalid version spec"),
        (
            "arbitrary text without bullet\n",
            "not an alteration bullet",
        ),
    ];

    for (alterations, expected_keyword) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let (tree, telemetry) = seed_village(tmp.path());
        let promoter = Arc::new(DeedPromoter::new(tree.clone()).with_telemetry(telemetry));
        let bp_path = tree.guild_blueprint(&name("village-alpine"));
        let before_text = fs::read_to_string(&bp_path).unwrap();

        write_parcel_deed(&tree, 1001, 1, alterations);

        let outcome = promoter.promote_parcel(1001).unwrap();
        match outcome {
            PromotionOutcome::Rejected { uid, reason } => {
                assert_eq!(uid, 1001);
                assert!(
                    reason.contains(expected_keyword),
                    "reason must contain '{expected_keyword}', got '{reason}'"
                );
            }
            other => panic!("expected rejection for {alterations:?}, got {other:?}"),
        }

        let after_text = fs::read_to_string(&bp_path).unwrap();
        assert_eq!(
            before_text, after_text,
            "blueprint must stay untouched on rejection"
        );
    }
}

#[test]
fn rejection_matrix_duplicates_and_multiple_sections() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry) = seed_village(tmp.path());
    let promoter = Arc::new(DeedPromoter::new(tree.clone()).with_telemetry(telemetry));

    // Duplicate tool in one section
    write_parcel_deed(&tree, 1001, 1, "- pixi uv\n- pixi uv = 0.5\n");
    let outcome = promoter.promote_parcel(1001).unwrap();
    assert!(matches!(outcome, PromotionOutcome::Rejected { .. }));

    // Multiple sections
    let deed_two_sections = "\
# DEED — environment record · parcel1002
- identity: worker1002
- uid slot: 1002
- commission: 1
- blueprint: village-alpine v1 · base image docker.io/library/alpine:3.24

### Dynamic Alterations
- pixi uv

### Dynamic Alterations
- apk curl
";
    fs::create_dir_all(tree.parcel(1002)).unwrap();
    fs::write(tree.parcel_deed(1002), deed_two_sections).unwrap();

    let outcome_two = promoter.promote_parcel(1002).unwrap();
    match outcome_two {
        PromotionOutcome::Rejected { reason, .. } => {
            assert!(reason.contains("second '### Dynamic Alterations' section"));
        }
        other => panic!("expected duplicate section rejection, got {other:?}"),
    }
}

#[test]
fn idempotence_and_content_hash_dedupe() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry) = seed_village(tmp.path());
    let promoter = Arc::new(DeedPromoter::new(tree.clone()).with_telemetry(telemetry));

    let alterations = "- pixi uv = 0.4.2\n";
    write_parcel_deed(&tree, 1001, 1, alterations);

    // First promotion: bumps v1 → v2
    let first = promoter.promote_parcel(1001).unwrap();
    assert_eq!(
        first,
        PromotionOutcome::Promoted {
            blueprint: name("village-alpine"),
            old_version: 1,
            new_version: 2,
            alterations_count: 1,
        }
    );

    // Second promotion of identical alterations: no-op, dedupe via content hash and toolset
    let second = promoter.promote_parcel(1001).unwrap();
    assert_eq!(
        second,
        PromotionOutcome::AlreadyMerged {
            blueprint: name("village-alpine"),
            version: 2,
        }
    );

    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    let bp_text = fs::read_to_string(&bp_path).unwrap();
    let bp = Blueprint::from_toml(&bp_text).unwrap();
    assert_eq!(
        bp.meta().version(),
        2,
        "version must not bump on re-promotion"
    );

    // Clean parcel with no dynamic alterations: NoAlterations outcome, no bump
    write_parcel_deed(&tree, 1002, 1, "");
    let clean = promoter.promote_parcel(1002).unwrap();
    assert_eq!(clean, PromotionOutcome::NoAlterations { uid: 1002 });
    assert_eq!(bp.meta().version(), 2);
}

#[test]
fn symlink_refusal_at_deed_and_blueprint_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry) = seed_village(tmp.path());
    let promoter = Arc::new(DeedPromoter::new(tree.clone()).with_telemetry(telemetry));

    // Plant a symlink as DEED.md pointing to /dev/null
    let parcel_dir = tree.parcel(1001);
    fs::create_dir_all(&parcel_dir).unwrap();
    std::os::unix::fs::symlink("/dev/null", tree.parcel_deed(1001)).unwrap();

    let err = promoter.promote_parcel(1001).unwrap_err();
    assert!(
        matches!(
            err,
            hamlet_daemon::promote::PromoteError::SymlinkRefused { .. }
        ),
        "symlink DEED.md must be refused: {err}"
    );

    // Plant a symlink as blueprint
    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    fs::remove_file(&bp_path).unwrap();
    std::os::unix::fs::symlink("/dev/null", &bp_path).unwrap();

    write_parcel_deed(&tree, 1002, 1, "- pixi uv\n");
    let err_bp = promoter.promote_parcel(1002).unwrap_err();
    assert!(
        matches!(
            err_bp,
            hamlet_daemon::promote::PromoteError::SymlinkRefused { .. }
        ),
        "symlink blueprint must be refused: {err_bp}"
    );
}

#[tokio::test]
async fn notify_watcher_updates_incremental_parsed_view() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry) = seed_village(tmp.path());
    let promoter = Arc::new(DeedPromoter::new(tree.clone()).with_telemetry(telemetry));

    let dispatcher = Dispatcher::new();
    let _plane = spawn_deed_plane(promoter.clone(), &dispatcher);

    // Initial state: no cached view
    assert!(promoter.cached_view(1001).is_none());

    // Write DEED.md for parcel 1001
    write_parcel_deed(&tree, 1001, 1, "- pixi uv = 0.4.2\n");

    // Dispatch Reaction::DeedMutation { uid: 1001 }
    dispatcher
        .dispatch(Dispatched {
            seq: 1,
            reaction: Reaction::DeedMutation { uid: 1001 },
        })
        .await;

    // Yield to let the deed plane task process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let view = promoter.cached_view(1001).expect("view must be cached");
    assert_eq!(view.uid, 1001);
    assert_eq!(view.blueprint_ref, Some(name("village-alpine")));
    assert_eq!(view.commission, Some(1));
    let alts = view.alterations.unwrap();
    assert_eq!(alts.len(), 1);
    assert_eq!(alts[0].name(), "uv");
    assert!(!view.checksum.is_empty());
}

#[tokio::test]
async fn ledger_entry_completion_trigger_evolves_blueprint() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry) = seed_village(tmp.path());

    // Create commission 1
    let ledger = Ledger::new(tree.clone())
        .with_wake(Arc::new(RecordingCommissionWake::new()))
        .with_telemetry(telemetry.clone());

    ledger
        .create_commission("test-comm", "Brief for commission.\n", None)
        .unwrap();

    let promoter = Arc::new(DeedPromoter::new(tree.clone()).with_telemetry(telemetry));
    let ledger = ledger
        .with_promoter(promoter)
        .with_promotion_trigger(PromotionTrigger::EntryCompletion);

    // Assign slot 1001 to commission 1 and post indenture
    ledger.assign_uid(1, 1001).unwrap();
    write_parcel_deed(&tree, 1001, 1, "- pixi uv = 0.4.2\n");

    let indenture_doc = "\
# INDENTURE

## worker1001
- goal: build engine
- scope: core logic
- done: tests green
";
    fs::write(tree.commission_indenture(1), indenture_doc).unwrap();
    ledger.reconcile_indenture(1).unwrap();

    // Now, agent 1001 reports done via entry notice
    let result = ledger
        .handle_entry_notice(1001, "commission:1:done")
        .unwrap();
    assert!(result.is_some());

    // With EntryCompletion trigger, blueprint must now be version 2
    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    let bp_text = fs::read_to_string(&bp_path).unwrap();
    let bp = Blueprint::from_toml(&bp_text).unwrap();
    assert_eq!(
        bp.meta().version(),
        2,
        "entry completion must trigger blueprint evolution"
    );
    assert!(bp.tools().iter().any(|t| t.name() == "uv"));
}

#[tokio::test]
async fn ledger_commission_winddown_trigger_evolves_blueprint_on_all_done() {
    let tmp = tempfile::tempdir().unwrap();
    let (tree, telemetry) = seed_village(tmp.path());

    let ledger = Ledger::new(tree.clone())
        .with_wake(Arc::new(RecordingCommissionWake::new()))
        .with_telemetry(telemetry.clone());

    ledger
        .create_commission("test-winddown", "Brief for commission.\n", None)
        .unwrap();

    let promoter = Arc::new(DeedPromoter::new(tree.clone()).with_telemetry(telemetry));
    let ledger = ledger
        .with_promoter(promoter)
        .with_promotion_trigger(PromotionTrigger::CommissionWindDown);

    ledger.assign_uid(1, 1001).unwrap();
    ledger.assign_uid(1, 1002).unwrap();
    write_parcel_deed(&tree, 1001, 1, "- pixi uv = 0.4.2\n");
    write_parcel_deed(&tree, 1002, 1, "- cargo ripgrep\n");

    let indenture_doc = "\
# INDENTURE

## worker1001
- goal: build engine
- scope: core logic
- done: tests green

## worker1002
- goal: build CLI
- scope: parser
- done: tests green
";
    fs::write(tree.commission_indenture(1), indenture_doc).unwrap();
    ledger.reconcile_indenture(1).unwrap();

    // First agent completes: under CommissionWindDown, blueprint must NOT evolve yet
    ledger
        .handle_entry_notice(1001, "commission:1:done")
        .unwrap();

    let bp_path = tree.guild_blueprint(&name("village-alpine"));
    let bp_text_mid = fs::read_to_string(&bp_path).unwrap();
    let bp_mid = Blueprint::from_toml(&bp_text_mid).unwrap();
    assert_eq!(
        bp_mid.meta().version(),
        1,
        "blueprint must not evolve before all entries done"
    );

    // Second agent completes: commission transitions to WindingDown, triggering promotion
    ledger
        .handle_entry_notice(1002, "commission:1:done")
        .unwrap();

    let bp_text_final = fs::read_to_string(&bp_path).unwrap();
    let bp_final = Blueprint::from_toml(&bp_text_final).unwrap();
    assert_eq!(
        bp_final.meta().version(),
        3,
        "blueprint must evolve for both agents at wind-down (v1 -> v2 -> v3)"
    );
    assert!(bp_final.tools().iter().any(|t| t.name() == "uv"));
    assert!(bp_final.tools().iter().any(|t| t.name() == "ripgrep"));
}
