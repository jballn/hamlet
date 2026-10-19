//! Guild ledger integration tests (Sprint 06 Step 02; SPEC §9.1, ADR-0036,
//! ADR-0037, CONVENTIONS §9).
//!
//! All tests run against real tempdir `.hamlet/` trees and real `git`
//! subprocesses with **no podman** required:
//!
//! - full commission creation flow (`commission1`, `commission2`, bare git
//!   repo, `COMMISSION.md`, `Draft` charter registration);
//! - `CommissionCreated` Mayor wake fired (`RecordingCommissionWake` spy AND
//!   end-to-end `CommissionWakeBridge` → `MayorEngine` → `fakeagent`);
//! - slug sanitization and brief/seed adversarial rejections (zero side
//!   effects on refusal);
//! - `--seed <dir>` initial commit on `main` authored by `hamlet-daemon` with
//!   `TITLE`-style lineage;
//! - roster management API (`assign_uid`, `release_uid`, `roster`) feeding
//!   Sprint 04 `FreightGuard::policy` and Sprint 05 `DecreeExecutor`;
//! - multi-tenancy: two commissions, disjoint rosters, independent states,
//!   independent bare git repositories, distinct `MountSet::for_agent` bind
//!   sources;
//! - kill/restart resumability from `charter.json` + tree alone.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::commission::{CommissionEvent, CommissionState};
use hamlet_core::config::Config;
use hamlet_core::decree::{AckResult, Decree, DecreeAck, DecreeId, RejectReason, DECREE_VERSION};
use hamlet_core::mount::{MountMode, MountSet};
use hamlet_core::packet::{EventKind, WakeReason, MAX_ARTIFACT_BYTES};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::MAYOR_UID;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::decrees::{DecreeExecutor, DecreeRefusal, SessionRouter};
use hamlet_daemon::freight::FreightGuard;
use hamlet_daemon::ledger::{
    render_commission_list, seed_commit_message, CommissionWakeBridge, Ledger, LedgerError,
    RecordingCommissionWake, DEFAULT_BRANCH, DEFAULT_HEAD_REF, SEED_AUTHOR_EMAIL, SEED_AUTHOR_NAME,
};
use hamlet_daemon::mayor::{ManorProvisioner, MayorEngine};
use hamlet_daemon::podman::{BoxFuture, FakeRuntime, Runtime};
use hamlet_daemon::supervise::HostProcessSpawner;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn name(raw: &str) -> SafeName {
    sanitize_name(raw).expect("test name must sanitize")
}

struct TestVillage {
    tree: HamletTree,
    _tmp: tempfile::TempDir,
}

impl TestVillage {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        let mut charter = Charter::empty();
        charter.reserve_mayor(name("manor")).unwrap();
        charter.save_atomic(&tree.charter()).unwrap();
        Self { tree, _tmp: tmp }
    }

    fn allocate_unassigned_slot(&self, identity: &str) -> u32 {
        let mut charter = Charter::load(&self.tree.charter()).unwrap();
        let uid = charter.allocate_slot(name(identity), None).unwrap();
        charter.save_atomic(&self.tree.charter()).unwrap();
        uid
    }
}

fn git_stdout(git_dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .args(args)
        .output()
        .expect("git must run");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

// ---------------------------------------------------------------------------
// 1. Full creation flow + Mayor wake reaction spy
// ---------------------------------------------------------------------------

#[test]
fn create_commission_materializes_ledger_bare_git_brief_charter_and_fires_wake() {
    let v = TestVillage::new();
    let spy = Arc::new(RecordingCommissionWake::new());
    let ledger = Ledger::new(v.tree.clone()).with_wake(spy.clone());

    let brief_1 = "# Commission 1: Build the Watchtower\n\nConstruct the northern watchtower.\n";
    let c1 = ledger
        .create_commission("watchtower", brief_1, None)
        .expect("first commission creation must succeed");

    assert_eq!(c1.id, 1);
    assert_eq!(c1.slug.as_str(), "watchtower");
    assert_eq!(c1.state, CommissionState::Draft);
    assert_eq!(c1.dir, v.tree.commission(1));
    assert_eq!(c1.git_dir, v.tree.commission_git(1));
    assert_eq!(c1.commission_md, v.tree.commission_md(1));
    assert_eq!(c1.seeded_files, None);
    assert_eq!(c1.seeded_commit, None);

    // Bare git repository invariants (SPEC §9.1, ADR-0011, ADR-0037 cl. 4).
    assert_eq!(
        git_stdout(&c1.git_dir, &["rev-parse", "--is-bare-repository"]).trim(),
        "true"
    );
    assert_eq!(
        git_stdout(&c1.git_dir, &["symbolic-ref", "HEAD"]).trim(),
        DEFAULT_HEAD_REF
    );
    assert_eq!(
        git_stdout(&c1.git_dir, &["config", "--get", "core.hooksPath"]).trim(),
        "/dev/null"
    );
    let git_mode = fs::metadata(&c1.git_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(git_mode, 0o755, "bare git dir must be 0755 (ADR-0011)");

    // COMMISSION.md written verbatim at mode 0644.
    assert_eq!(fs::read_to_string(&c1.commission_md).unwrap(), brief_1);
    let md_mode = fs::metadata(&c1.commission_md)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(md_mode, 0o644);

    // Charter state: sequential 1-based id in Draft (ADR-0036 cl. 1–2).
    let charter = Charter::load(&v.tree.charter()).unwrap();
    let rec1 = &charter.commissions[&1];
    assert_eq!(rec1.slug.as_str(), "watchtower");
    assert_eq!(rec1.state(), CommissionState::Draft);
    assert!(rec1.roster.is_empty());

    // Sequential second commission gets commission2.
    let brief_2 = "# Commission 2: Repair the Aqueduct\n\nSeal the western arches.\n";
    let c2 = ledger
        .create_commission("aqueduct", brief_2, None)
        .expect("second commission creation must succeed");
    assert_eq!(c2.id, 2);
    assert_eq!(c2.slug.as_str(), "aqueduct");

    // Reaction spy recorded both CommissionCreated wakes in order.
    let events = spy.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, 1);
    assert_eq!(
        events[0].1.kind(),
        &EventKind::CommissionCreated { commission: 1 }
    );
    assert_eq!(events[0].1.wake_reason(), WakeReason::CommissionCreated);
    assert_eq!(events[0].1.body(), brief_1);

    assert_eq!(events[1].0, 2);
    assert_eq!(
        events[1].1.kind(),
        &EventKind::CommissionCreated { commission: 2 }
    );
    assert_eq!(events[1].1.body(), brief_2);
}

#[tokio::test]
async fn commission_wake_bridge_drives_mayor_single_turn_evaluation() {
    let mut config = Config::default();
    config.sessions.ready_timeout_ms = 10_000;

    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    Charter::empty().save_atomic(&tree.charter()).unwrap();

    let runtime = Arc::new(FakeRuntime::new());
    ManorProvisioner::new(
        tree.clone(),
        config.clone(),
        Arc::clone(&runtime) as Arc<dyn Runtime>,
    )
    .provision(&Blueprint::default_village_alpine().unwrap())
    .await
    .expect("manor provisioning must succeed");

    let script_log = tmp.path().join("fakeagent-mayor.log");
    let script = tmp.path().join("fakeagent-mayor.json");
    fs::write(
        &script,
        serde_json::json!({
            "log": script_log.to_string_lossy(),
            "usage": { "input": 5, "output": 3, "totalTokens": 8 },
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

    let bridge = Arc::new(CommissionWakeBridge::new(Arc::clone(&engine)));
    let ledger = Ledger::new(tree.clone()).with_wake(bridge);

    let brief = "# Commission: Lighthouse\n\nErect a beacon on the headland.\n";
    let created = ledger
        .create_commission("lighthouse", brief, None)
        .expect("commission creation must succeed");
    assert_eq!(created.id, 1);

    // Wait for the spawned CommissionWakeBridge task to finish the single turn.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.sleep_state().turns < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for Mayor turn on CommissionCreated wake"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let sleep = engine.sleep_state();
    assert_eq!(sleep.turns, 1);
    assert!(!sleep.awake);
    assert_eq!(sleep.pending, 0);

    let raw_log = fs::read_to_string(&script_log).unwrap();
    assert!(
        raw_log.contains("wake_reason: commission_created"),
        "Mayor prompt must carry wake_reason: commission_created: {raw_log}"
    );
    assert!(
        raw_log.contains("Erect a beacon on the headland."),
        "Mayor prompt must embed the COMMISSION.md brief: {raw_log}"
    );
}

// ---------------------------------------------------------------------------
// 2. Slug sanitization & adversarial brief/duplicate rejections
// ---------------------------------------------------------------------------

#[test]
fn slug_sanitization_and_brief_rejections_leave_tree_and_charter_untouched() {
    let v = TestVillage::new();
    let spy = Arc::new(RecordingCommissionWake::new());
    let ledger = Ledger::new(v.tree.clone()).with_wake(spy.clone());
    let charter_before = fs::read_to_string(v.tree.charter()).unwrap();

    let hostile_slugs = [
        "",
        ".",
        "..",
        "../escape",
        "nested/slug",
        "nested\\slug",
        "-leading-hyphen",
        "has space",
        "bad$char",
        "nul\0byte",
        &"a".repeat(65),
    ];

    for bad_slug in hostile_slugs {
        let err = ledger
            .create_commission(bad_slug, "# valid brief\n", None)
            .expect_err(&format!("hostile slug {bad_slug:?} must be refused"));
        assert!(
            matches!(err, LedgerError::Sanitize(_)),
            "expected Sanitize error for {bad_slug:?}, got {err:?}"
        );
    }

    // Empty / whitespace brief rejected.
    assert!(matches!(
        ledger.create_commission("valid-slug", "   \n\t  ", None),
        Err(LedgerError::InvalidBrief { .. })
    ));

    // CRLF brief rejected (ADR-0019 LF-only discipline).
    assert!(matches!(
        ledger.create_commission("valid-slug", "# Title\r\nBody\r\n", None),
        Err(LedgerError::InvalidBrief { .. })
    ));

    // Oversize brief (> MAX_ARTIFACT_BYTES) rejected before any disk write.
    let huge = "x".repeat(MAX_ARTIFACT_BYTES + 1);
    assert!(matches!(
        ledger.create_commission("valid-slug", &huge, None),
        Err(LedgerError::InvalidBrief { .. })
    ));

    // Tree and charter are byte-identical; no commission1 directory exists; no wake fired.
    assert!(!v.tree.commission(1).exists());
    assert_eq!(
        fs::read_to_string(v.tree.charter()).unwrap(),
        charter_before
    );
    assert!(spy.is_empty());

    // Duplicate slug is rejected after the first commission succeeds.
    ledger
        .create_commission("unique-slug", "# Brief 1\n", None)
        .unwrap();
    let dup_err = ledger
        .create_commission("unique-slug", "# Brief 2\n", None)
        .expect_err("duplicate slug must be refused");
    assert!(matches!(dup_err, LedgerError::Charter(_)));
    assert!(!v.tree.commission(2).exists());
    assert_eq!(spy.len(), 1);
}

// ---------------------------------------------------------------------------
// 3. `--seed <dir>` initial commit on `main` + TITLE-style lineage
// ---------------------------------------------------------------------------

#[test]
fn seed_directory_imports_files_as_initial_commit_on_main_with_title_lineage() {
    let v = TestVillage::new();
    let seed_tmp = tempfile::tempdir().unwrap();
    let seed_dir = seed_tmp.path();

    // Populate seed directory with regular files, nested subdirectory, executable
    // script, and an ignored `.git` directory.
    fs::write(seed_dir.join("README.md"), "# Seeded Project\n").unwrap();
    fs::create_dir_all(seed_dir.join("src")).unwrap();
    fs::write(
        seed_dir.join("src/lib.rs"),
        "pub fn answer() -> u32 {\n    42\n}\n",
    )
    .unwrap();
    fs::create_dir_all(seed_dir.join("scripts")).unwrap();
    let script_path = seed_dir.join("scripts/run.sh");
    fs::write(&script_path, "#!/bin/sh\necho ok\n").unwrap();
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

    // A pre-existing `.git/` folder inside the seed source must be ignored cleanly.
    fs::create_dir_all(seed_dir.join(".git")).unwrap();
    fs::write(seed_dir.join(".git/HEAD"), "ref: refs/heads/master\n").unwrap();

    let spy = Arc::new(RecordingCommissionWake::new());
    let ledger = Ledger::new(v.tree.clone()).with_wake(spy.clone());

    let created = ledger
        .create_commission(
            "seeded-work",
            "# Seeded Commission\n\nExtend the seeded crate.\n",
            Some(seed_dir),
        )
        .expect("seeded commission creation must succeed");

    assert_eq!(created.id, 1);
    assert_eq!(created.seeded_files, Some(3));
    let commit_oid = created
        .seeded_commit
        .as_deref()
        .expect("seeded commit oid must be recorded");
    assert_eq!(commit_oid.len(), 40);

    // No staging artifacts left behind in commission1/.
    assert!(!created.dir.join(".seed-worktree").exists());
    assert!(!created.dir.join(".seed-index").exists());

    // Verify commit on refs/heads/main is root commit (no parents), authored by
    // hamlet-daemon, with TITLE-style lineage message.
    let head_oid = git_stdout(&created.git_dir, &["rev-parse", DEFAULT_HEAD_REF]);
    assert_eq!(head_oid.trim(), commit_oid);

    let parents = git_stdout(&created.git_dir, &["log", "-1", "--format=%P", commit_oid]);
    assert_eq!(parents.trim(), "", "seed commit must be the root commit");

    let author = git_stdout(
        &created.git_dir,
        &["log", "-1", "--format=%an <%ae>", commit_oid],
    );
    assert_eq!(
        author.trim(),
        format!("{SEED_AUTHOR_NAME} <{SEED_AUTHOR_EMAIL}>")
    );

    let message = git_stdout(&created.git_dir, &["log", "-1", "--format=%B", commit_oid]);
    assert!(
        message.contains("seed: commission1 (seeded-work)"),
        "commit subject must identify commission and slug: {message}"
    );
    assert!(
        message.contains("# TITLE — chain of title · commission1"),
        "commit body must carry TITLE-style lineage header: {message}"
    );
    assert!(
        message.contains(
            "1. trigger: `commission_new(slug=\"seeded-work\", commission=1, seeded_files=3)`"
        ),
        "commit body must record trigger and file count: {message}"
    );

    // Pure builder produces the exact format for a known timestamp.
    assert_eq!(
        seed_commit_message(1, &name("seeded-work"), 3, 1_700_000_000),
        format!(
            "seed: commission1 (seeded-work)\n\
             \n\
             # TITLE — chain of title · commission1\n\
             \n\
             1. trigger: `commission_new(slug=\"seeded-work\", commission=1, seeded_files=3)` · unix 1700000000 · initial commit on `{DEFAULT_BRANCH}` authored by {SEED_AUTHOR_NAME} (SPEC §9.1)\n"
        )
    );

    // Verify tree entries and modes in the bare repository.
    let ls_tree = git_stdout(&created.git_dir, &["ls-tree", "-r", DEFAULT_HEAD_REF]);
    assert!(ls_tree.contains("100644 blob") && ls_tree.contains("\tREADME.md"));
    assert!(ls_tree.contains("100644 blob") && ls_tree.contains("\tsrc/lib.rs"));
    assert!(ls_tree.contains("100755 blob") && ls_tree.contains("\tscripts/run.sh"));
    assert!(!ls_tree.contains(".git"));

    let readme = git_stdout(
        &created.git_dir,
        &["show", &format!("{DEFAULT_HEAD_REF}:README.md")],
    );
    assert_eq!(readme, "# Seeded Project\n");
}

#[test]
fn seed_directory_refuses_symlinks_and_missing_dirs_without_side_effects() {
    let v = TestVillage::new();
    let ledger = Ledger::new(v.tree.clone());

    // Missing seed directory refused.
    let missing = v.tree.root().join("does-not-exist");
    assert!(matches!(
        ledger.create_commission("alpha", "# Brief\n", Some(&missing)),
        Err(LedgerError::SeedDirInvalid { .. })
    ));
    assert!(!v.tree.commission(1).exists());

    // Symlinkinside seed directory refused (ADR-0007).
    let seed_tmp = tempfile::tempdir().unwrap();
    fs::write(seed_tmp.path().join("ok.txt"), "ok\n").unwrap();
    symlink("/etc/passwd", seed_tmp.path().join("escape.txt")).unwrap();

    assert!(matches!(
        ledger.create_commission("alpha", "# Brief\n", Some(seed_tmp.path())),
        Err(LedgerError::SymlinkRefused { .. })
    ));
    assert!(!v.tree.commission(1).exists());
    let charter = Charter::load(&v.tree.charter()).unwrap();
    assert!(charter.commissions.is_empty());
}

// ---------------------------------------------------------------------------
// 4. Roster management API -> Sprint 04 Freight ACL & Sprint 05 Decrees
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SpySessionRouter {
    reassigned: std::sync::Mutex<Vec<(u32, u64, String)>>,
}

impl SessionRouter for SpySessionRouter {
    fn reassign(
        &self,
        uid: u32,
        commission: u64,
        entry: &SafeName,
        _text: &str,
    ) -> Result<(), DecreeRefusal> {
        self.reassigned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((uid, commission, entry.to_string()));
        Ok(())
    }

    fn steer<'a>(
        &'a self,
        _uid: u32,
        _directive: &'a str,
    ) -> BoxFuture<'a, Result<(), DecreeRefusal>> {
        Box::pin(async move { Ok(()) })
    }
}

#[tokio::test]
async fn roster_management_feeds_freight_acl_policy_and_decree_validation() {
    let v = TestVillage::new();
    let runtime = Arc::new(FakeRuntime::new());
    ManorProvisioner::new(
        v.tree.clone(),
        Config::default(),
        Arc::clone(&runtime) as Arc<dyn Runtime>,
    )
    .provision(&Blueprint::default_village_alpine().unwrap())
    .await
    .unwrap();

    let ledger = Ledger::new(v.tree.clone());
    ledger
        .create_commission("alpha", "# Alpha commission\n", None)
        .unwrap();
    ledger
        .create_commission("beta", "# Beta commission\n", None)
        .unwrap();

    let uid_scribe = v.allocate_unassigned_slot("scribe");
    let uid_mason = v.allocate_unassigned_slot("mason");
    assert_eq!(uid_scribe, 1001);
    assert_eq!(uid_mason, 1002);

    let telemetry = Arc::new(
        hamlet_daemon::telemetry::Telemetry::open(v.tree.root().join(".telemetry-test")).unwrap(),
    );
    let spawner: Arc<dyn hamlet_daemon::supervise::SessionSpawner> =
        Arc::new(HostProcessSpawner::new(env!("CARGO_BIN_EXE_fakeagent")));
    let supervisor = Arc::new(hamlet_daemon::supervise::Supervisor::new(
        v.tree.clone(),
        Config::default(),
        spawner,
        telemetry,
    ));
    let freight = FreightGuard::new(
        supervisor,
        Arc::clone(&runtime) as Arc<dyn Runtime>,
        Arc::new(hamlet_daemon::freight::RecordingWake::new()),
    );

    // Before rostering: only MAYOR_UID (1000) is a valid freight grantee.
    let p0 = freight.policy().unwrap();
    assert_eq!(p0.grantees, BTreeSet::from([MAYOR_UID]));

    // Assign 1001 to commission 1 (alpha).
    ledger.assign_uid(1, uid_scribe).unwrap();
    assert_eq!(ledger.roster(1).unwrap(), BTreeSet::from([uid_scribe]));

    // FreightGuard::policy now includes 1001; commission 2's scoped policy does not.
    let p1 = freight.policy().unwrap();
    assert_eq!(p1.grantees, BTreeSet::from([MAYOR_UID, uid_scribe]));
    assert_eq!(
        freight.policy_for_commission(1).unwrap().grantees,
        BTreeSet::from([MAYOR_UID, uid_scribe])
    );
    assert_eq!(
        freight.policy_for_commission(2).unwrap().grantees,
        BTreeSet::from([MAYOR_UID])
    );

    // Cannot assign 1001 to commission 2 while bound to commission 1.
    assert!(ledger.assign_uid(2, uid_scribe).is_err());

    // Sprint 05 DecreeExecutor: ReassignTask on commission 1 succeeds for rostered
    // 1001, and is refused with InvalidTarget for unrostered 1002.
    let router = Arc::new(SpySessionRouter::default());
    let executor = DecreeExecutor::new(v.tree.clone(), Config::default())
        .with_router(Arc::clone(&router) as Arc<dyn SessionRouter>);

    let placeholder = DecreeId::new(1, "0000000000000000").unwrap();
    let d1 = Decree::ReassignTask {
        decree_id: placeholder.clone(),
        decree_version: DECREE_VERSION,
        commission: 1,
        entry: name("entry-a"),
        uid: uid_scribe,
    }
    .stamp(1)
    .unwrap();
    fs::write(
        v.tree
            .manor_decrees()
            .join(d1.id().decree_file_name().unwrap().as_str()),
        d1.render().unwrap(),
    )
    .unwrap();
    executor.scan().await.unwrap();
    let ack1 =
        DecreeAck::parse(&fs::read_to_string(v.tree.manor_decree_ack(d1.id())).unwrap()).unwrap();
    assert_eq!(ack1.result, AckResult::Done);

    // Unrostered 1002 on commission 1 is rejected with InvalidTarget.
    let d2 = Decree::ReassignTask {
        decree_id: placeholder.clone(),
        decree_version: DECREE_VERSION,
        commission: 1,
        entry: name("entry-b"),
        uid: uid_mason,
    }
    .stamp(2)
    .unwrap();
    fs::write(
        v.tree
            .manor_decrees()
            .join(d2.id().decree_file_name().unwrap().as_str()),
        d2.render().unwrap(),
    )
    .unwrap();
    executor.scan().await.unwrap();
    let ack2 =
        DecreeAck::parse(&fs::read_to_string(v.tree.manor_decree_ack(d2.id())).unwrap()).unwrap();
    assert_eq!(
        ack2.result,
        AckResult::Rejected {
            reason: RejectReason::InvalidTarget
        }
    );

    // Release 1001 from commission 1 -> removed from freight grantees and
    // refused by subsequent ReassignTask decrees on commission 1.
    ledger.release_uid(1, uid_scribe).unwrap();
    assert!(ledger.roster(1).unwrap().is_empty());
    assert_eq!(
        freight.policy().unwrap().grantees,
        BTreeSet::from([MAYOR_UID])
    );

    let d3 = Decree::ReassignTask {
        decree_id: placeholder,
        decree_version: DECREE_VERSION,
        commission: 1,
        entry: name("entry-c"),
        uid: uid_scribe,
    }
    .stamp(3)
    .unwrap();
    fs::write(
        v.tree
            .manor_decrees()
            .join(d3.id().decree_file_name().unwrap().as_str()),
        d3.render().unwrap(),
    )
    .unwrap();
    executor.scan().await.unwrap();
    let ack3 =
        DecreeAck::parse(&fs::read_to_string(v.tree.manor_decree_ack(d3.id())).unwrap()).unwrap();
    assert_eq!(
        ack3.result,
        AckResult::Rejected {
            reason: RejectReason::InvalidTarget
        }
    );
}

// ---------------------------------------------------------------------------
// 5. Multi-tenancy: two commissions, disjoint rosters, independent states,
//    independent git dirs, and distinct per-commission mount sets
// ---------------------------------------------------------------------------

#[test]
fn multi_tenancy_disjoint_rosters_independent_states_git_dirs_and_mount_sets() {
    let v = TestVillage::new();
    let ledger = Ledger::new(v.tree.clone());

    let seed1 = tempfile::tempdir().unwrap();
    fs::write(seed1.path().join("alpha.txt"), "commission 1 payload\n").unwrap();
    let seed2 = tempfile::tempdir().unwrap();
    fs::write(seed2.path().join("beta.txt"), "commission 2 payload\n").unwrap();

    let c1 = ledger
        .create_commission("alpha", "# Alpha Brief\n", Some(seed1.path()))
        .unwrap();
    let c2 = ledger
        .create_commission("beta", "# Beta Brief\n", Some(seed2.path()))
        .unwrap();
    assert_eq!(c1.id, 1);
    assert_eq!(c2.id, 2);
    assert_ne!(c1.git_dir, c2.git_dir);

    let u1 = v.allocate_unassigned_slot("agent-a1");
    let u2 = v.allocate_unassigned_slot("agent-a2");
    let u3 = v.allocate_unassigned_slot("agent-b1");

    // Disjoint rosters: {1001, 1002} on commission 1, {1003} on commission 2.
    ledger.assign_uid(1, u1).unwrap();
    ledger.assign_uid(1, u2).unwrap();
    ledger.assign_uid(2, u3).unwrap();

    let r1 = ledger.roster(1).unwrap();
    let r2 = ledger.roster(2).unwrap();
    assert_eq!(r1, BTreeSet::from([u1, u2]));
    assert_eq!(r2, BTreeSet::from([u3]));
    assert!(r1.is_disjoint(&r2));

    // Independent states: advance commission 1 to Active (IndenturePosted) while
    // commission 2 remains in Draft; then close commission 1 without affecting
    // commission 2.
    ledger
        .apply_event(
            1,
            &CommissionEvent::IndenturePosted {
                entries: BTreeSet::from([u1, u2]),
            },
        )
        .unwrap();
    assert_eq!(
        ledger.show_commission("alpha").unwrap().state,
        CommissionState::Active
    );
    assert_eq!(
        ledger.show_commission("beta").unwrap().state,
        CommissionState::Draft
    );

    ledger
        .apply_event(1, &CommissionEvent::OperatorClose)
        .unwrap();
    assert_eq!(
        ledger.show_commission("alpha").unwrap().state,
        CommissionState::Closed
    );
    assert_eq!(
        ledger.show_commission("beta").unwrap().state,
        CommissionState::Draft
    );

    // Closed commission 1 refuses roster mutations while Draft commission 2 remains mutable.
    assert!(ledger.release_uid(1, u1).is_err());

    // Independent bare git repositories: commission 1 has alpha.txt only;
    // commission 2 has beta.txt only.
    let tree1 = git_stdout(&c1.git_dir, &["ls-tree", "--name-only", DEFAULT_HEAD_REF]);
    let tree2 = git_stdout(&c2.git_dir, &["ls-tree", "--name-only", DEFAULT_HEAD_REF]);
    assert_eq!(tree1.trim(), "alpha.txt");
    assert_eq!(tree2.trim(), "beta.txt");

    // MountSet::for_agent differs per commission: `/var/git/` resolves to
    // `guild/ledger/commission1/git` for u1 on commission 1 and
    // `guild/ledger/commission2/git` for u3 on commission 2.
    let mounts_c1 = MountSet::for_agent(u1, 1).unwrap();
    let mounts_c2 = MountSet::for_agent(u3, 2).unwrap();

    let git_mount_c1 = mounts_c1
        .specs()
        .iter()
        .find(|m| m.container_path() == "/var/git/")
        .expect("agent mount set must include /var/git/");
    let git_mount_c2 = mounts_c2
        .specs()
        .iter()
        .find(|m| m.container_path() == "/var/git/")
        .expect("agent mount set must include /var/git/");

    assert_eq!(git_mount_c1.mode(), MountMode::Ro);
    assert_eq!(git_mount_c2.mode(), MountMode::Ro);
    assert_eq!(git_mount_c1.host_path(&v.tree), v.tree.commission_git(1));
    assert_eq!(git_mount_c2.host_path(&v.tree), v.tree.commission_git(2));
    assert_ne!(
        git_mount_c1.host_path(&v.tree),
        git_mount_c2.host_path(&v.tree)
    );
}

// ---------------------------------------------------------------------------
// 6. Kill/restart resumability & list/show tree-only derivation
// ---------------------------------------------------------------------------

#[test]
fn restart_resumes_from_charter_and_tree_alone() {
    let v = TestVillage::new();
    {
        let ledger = Ledger::new(v.tree.clone());
        ledger
            .create_commission("first", "# First brief\n", None)
            .unwrap();
        let uid = v.allocate_unassigned_slot("scribe");
        ledger.assign_uid(1, uid).unwrap();
    }

    // Fresh Ledger instance (simulating daemon restart) reconstructs everything
    // from charter.json + tree and allocates commission2 next.
    let ledger2 = Ledger::new(v.tree.clone());
    let list = ledger2.list_commissions().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, 1);
    assert_eq!(list[0].slug.as_str(), "first");
    assert_eq!(list[0].roster, BTreeSet::from([1001]));
    assert!(list[0].has_brief);
    assert!(!list[0].has_indenture);
    assert!(list[0].has_bare_git);

    let c2 = ledger2
        .create_commission("second", "# Second brief\n", None)
        .unwrap();
    assert_eq!(c2.id, 2);

    let table = render_commission_list(&ledger2.list_commissions().unwrap());
    assert!(table.contains("1   first   draft  1001    yes    no         bare"));
    assert!(table.contains("2   second  draft  -       yes    no         bare"));
}
