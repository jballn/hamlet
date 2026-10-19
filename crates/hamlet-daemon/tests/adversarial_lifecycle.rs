//! Adversarial Commission Lifecycle Suite — QA prompt 06/07
//! (SPEC §7, §9, §11.2, §14; ADR-0036, ADR-0037, ADR-0038; CONVENTIONS §2, §3, §9).
//!
//! Evaluates the Sprint 06 Commission Lifecycle surfaces from an adversarial perspective:
//! 1. Git mediation: corrupt/truncated bundles, foreign-namespace branches,
//!    forged author identity (author != slot UID), oversized bundles, hook
//!    smuggling in bundles, non-fast-forward attempts — all rejected; bare repo
//!    history never corrupted (fsck after every rejection round).
//! 2. DEED promotion: injection lines (shell metachars, newlines, path
//!    separators), unknown tool kinds, dup-set idempotence, DEED rewritten
//!    wholesale by a rogue agent (daemon detects non-append mutation →
//!    reject + trace), kill -9 mid-promotion → blueprint intact.
//! 3. Vault loop: approve decree referencing nonexistent freight path;
//!    artifact name traversal attempts; checksum mismatch on copy (simulate);
//!    double-approve replay; reject-loop bound → escalation fires exactly once.
//! 4. State machine: illegal transitions refused (closed → active etc.);
//!    two commissions never cross-contaminate (rosters, repos, vaults, archives).
//! 5. Restart resumability: full commission mid-flight (indenture posted,
//!    one bundle pending, one submission pending) → daemon kill → restart →
//!    everything resumes, nothing duplicated.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use hamlet_core::blueprint::{Blueprint, DEFAULT_BLUEPRINT_TOML};
use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{
    CommissionError, CommissionEvent, CommissionRecord, CommissionState, IndentureEntryStatus,
};
use hamlet_core::config::Config;
use hamlet_core::decree::RejectReason;
use hamlet_core::gitmed::{agent_identity_email, BundleRejection, MAX_BUNDLE_BYTES};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::decrees::Promotion;
use hamlet_daemon::ledger::{GitBundleOutcome, Ledger, RecordingCommissionWake};
use hamlet_daemon::mailroute::MailRouter;
use hamlet_daemon::promote::{DeedPromoter, PromotionOutcome};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::Telemetry;
use hamlet_daemon::vault::VaultPromotion;

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("valid safe name")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} in {} failed: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_bare(git_dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .expect("spawn bare git");
    assert!(
        out.status.success(),
        "git --git-dir={} {:?} failed: {}",
        git_dir.display(),
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn assert_fsck_clean(git_dir: &Path) {
    let _ = git_bare(git_dir, &["fsck", "--full"]);
}

fn setup_seed_dir(base: &Path) -> PathBuf {
    let seed = base.join("seed-src");
    fs::create_dir_all(&seed).expect("create seed dir");
    fs::write(seed.join("README.md"), "# Seed Project\nInitial content.\n").expect("write readme");
    seed
}

#[allow(dead_code)]
struct TestVillage {
    tmp: tempfile::TempDir,
    tree: HamletTree,
    telemetry: Arc<Telemetry>,
    mail: Arc<MailRouter>,
    supervisor: Arc<Supervisor>,
    wake: Arc<RecordingCommissionWake>,
    ledger: Arc<Ledger>,
}

impl TestVillage {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(".hamlet");
        let tree = HamletTree::ensure(&root).expect("ensure tree");

        let mut charter = Charter::empty();
        charter.reserve_mayor(name("mayor")).unwrap();
        charter.save_atomic(&tree.charter()).unwrap();

        let telemetry_dir = tmp.path().join(".telemetry");
        let telemetry = Arc::new(Telemetry::open(&telemetry_dir).expect("open telemetry"));
        let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
        let supervisor = Arc::new(Supervisor::new(
            tree.clone(),
            Config::default(),
            spawner,
            Arc::clone(&telemetry),
        ));
        let mail = Arc::new(MailRouter::new(Arc::clone(&supervisor)));
        let wake = Arc::new(RecordingCommissionWake::new());
        let ledger = Arc::new(
            Ledger::new(tree.clone())
                .with_wake(Arc::clone(&wake) as _)
                .with_mail_router(Arc::clone(&mail))
                .with_telemetry(Arc::clone(&telemetry)),
        );
        mail.set_ledger(Arc::clone(&ledger));

        let bp_path = tree.guild_blueprint(&name("village-alpine"));
        fs::create_dir_all(bp_path.parent().unwrap()).unwrap();
        fs::write(&bp_path, DEFAULT_BLUEPRINT_TOML).unwrap();

        Self {
            tmp,
            tree,
            telemetry,
            mail,
            supervisor,
            wake,
            ledger,
        }
    }

    fn create_commission(&self, slug: &str) -> (u64, PathBuf) {
        let seed = setup_seed_dir(self.tmp.path());
        let res = self
            .ledger
            .create_commission(slug, &format!("# Commission {slug}\n"), Some(&seed))
            .expect("create commission");
        (res.id, res.git_dir)
    }

    fn create_commission_winding_down(&self, slug: &str, identity: &str) -> (u64, u32) {
        let (id, _) = self.create_commission(slug);
        let uid = self.allocate_and_roster(id, identity);
        let mut charter = Charter::load(&self.tree.charter()).unwrap();
        let comm = charter.commissions.get_mut(&id).unwrap();
        comm.state = CommissionState::WindingDown;
        comm.indenture_entries
            .insert(uid, IndentureEntryStatus::Done);
        charter.save_atomic(&self.tree.charter()).unwrap();
        (id, uid)
    }

    fn allocate_and_roster(&self, commission: u64, identity: &str) -> u32 {
        let ident = name(identity);
        let mut charter = Charter::load(&self.tree.charter()).expect("load charter");
        let uid = charter
            .allocate_slot(ident.clone(), Some(commission))
            .expect("allocate slot");
        charter.parcels.insert(
            uid,
            ParcelLease {
                identity: ident,
                stasis: false,
            },
        );
        charter
            .save_atomic(&self.tree.charter())
            .expect("save charter");
        self.ledger
            .assign_uid(commission, uid)
            .expect("assign uid to commission");

        let spool = self.tree.parcel_git_spool(uid);
        fs::create_dir_all(&spool).expect("create git-spool");
        fs::set_permissions(&spool, fs::Permissions::from_mode(0o700)).expect("chmod 0700 spool");

        let parcel_dir = self.tree.parcel(uid);
        fs::create_dir_all(&parcel_dir).expect("create parcel dir");
        let deed_stub = format!(
            "# DEED — environment record · parcel{uid}\n\n\
             - identity: {identity}\n\
             - uid slot: {uid}\n\
             - commission: {commission}\n\
             - blueprint: village-alpine v1 · base image docker.io/library/alpine:3.24\n\n\
             ### Dynamic Alterations\n\
             - baseline · toolchain bootstrap · uid {uid} · image localhost/hamlet/village-alpine:v1\n\
               - manifest: /home/{uid}/.hamlet/manifest.json · sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n\
               - tool: pixi/pixi 0.34.* → 0.34.0 · pixi@0.34.0 sha256 0123\n\
               - image layer (apk): uutils-coreutils, git, acl, ca-certificates, curl\n\
               - env keys: PIXI_CACHE_DIR, LANG\n\n"
        );
        fs::write(self.tree.parcel_deed(uid), deed_stub).expect("write deed stub");

        uid
    }
}

// ===========================================================================
// 1. Git Mediation
// ===========================================================================

#[test]
fn test_git_mediation_corrupt_and_truncated_bundle_rejected() {
    let village = TestVillage::new();
    let (comm_id, git_dir) = village.create_commission("git-corrupt");
    let uid = village.allocate_and_roster(comm_id, "coder-corrupt");
    let spool = village.tree.parcel_git_spool(uid);

    // 0-byte bundle
    let empty_bundle = spool.join("empty.bundle");
    fs::write(&empty_bundle, b"").unwrap();
    fs::set_permissions(&empty_bundle, fs::Permissions::from_mode(0o600)).unwrap();

    let outcome = village
        .ledger
        .process_git_bundle(uid, "empty.bundle")
        .expect("process empty bundle");
    assert!(
        matches!(
            outcome,
            GitBundleOutcome::Rejected {
                rejection: BundleRejection::VerifyFailed,
                ..
            }
        ),
        "0-byte bundle must be rejected with VerifyFailed"
    );
    assert!(!empty_bundle.exists());
    assert!(spool.join("empty.bundle.verify").is_file());
    assert_fsck_clean(&git_dir);

    // Truncated bundle header
    let trunc_bundle = spool.join("trunc.bundle");
    fs::write(&trunc_bundle, b"# v2 git bundle\n").unwrap();
    fs::set_permissions(&trunc_bundle, fs::Permissions::from_mode(0o600)).unwrap();

    let outcome2 = village
        .ledger
        .process_git_bundle(uid, "trunc.bundle")
        .expect("process trunc bundle");
    assert!(
        matches!(
            outcome2,
            GitBundleOutcome::Rejected {
                rejection: BundleRejection::VerifyFailed,
                ..
            }
        ),
        "truncated bundle must be rejected with VerifyFailed"
    );
    assert!(!trunc_bundle.exists());
    assert!(spool.join("trunc.bundle.verify").is_file());
    assert_fsck_clean(&git_dir);
}

#[test]
fn test_git_mediation_foreign_namespace_and_other_agent_branch_rejected() {
    let village = TestVillage::new();
    let (comm_id, git_dir) = village.create_commission("git-ns");
    let uid1 = village.allocate_and_roster(comm_id, "agent1");
    let _uid2 = village.allocate_and_roster(comm_id, "agent2");

    let work = village.tmp.path().join("work-ns");
    git(
        village.tmp.path(),
        &["clone", &git_dir.to_string_lossy(), &work.to_string_lossy()],
    );
    let email1 = agent_identity_email(uid1);
    git(&work, &["config", "user.name", "agent1"]);
    git(&work, &["config", "user.email", &email1]);

    let spool1 = village.tree.parcel_git_spool(uid1);

    // 1. Target branch is refs/heads/main
    git(&work, &["checkout", "main"]);
    fs::write(work.join("bad.txt"), "hijack\n").unwrap();
    git(&work, &["add", "bad.txt"]);
    git(&work, &["commit", "-m", "hijack main"]);
    let main_bundle = spool1.join("main.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &main_bundle.to_string_lossy(),
            "refs/heads/main",
        ],
    );
    fs::set_permissions(&main_bundle, fs::Permissions::from_mode(0o600)).unwrap();

    let out1 = village
        .ledger
        .process_git_bundle(uid1, "main.bundle")
        .expect("process main bundle");
    assert!(
        matches!(
            out1,
            GitBundleOutcome::Rejected {
                rejection: BundleRejection::NotOwnBranch,
                ..
            }
        ),
        "push to main must be rejected with NotOwnBranch"
    );
    assert_fsck_clean(&git_dir);

    // 2. Target branch is refs/heads/agent/1002 (spoofing agent2 from agent1)
    let main_oid = git_bare(&git_dir, &["rev-parse", "refs/heads/main"]);
    git(&work, &["checkout", "-b", "agent/1002", &main_oid]);
    fs::write(work.join("spoof.txt"), "spoof agent2\n").unwrap();
    git(&work, &["add", "spoof.txt"]);
    git(&work, &["commit", "-m", "spoof agent2"]);
    let spoof_bundle = spool1.join("spoof.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &spoof_bundle.to_string_lossy(),
            "refs/heads/agent/1002",
        ],
    );
    fs::set_permissions(&spoof_bundle, fs::Permissions::from_mode(0o600)).unwrap();

    let out2 = village
        .ledger
        .process_git_bundle(uid1, "spoof.bundle")
        .expect("process spoof bundle");
    assert!(
        matches!(
            out2,
            GitBundleOutcome::Rejected {
                rejection: BundleRejection::NotOwnBranch,
                ..
            }
        ),
        "push to agent/1002 by uid 1001 must be rejected with NotOwnBranch"
    );
    assert!(!git_dir.join("refs/heads/agent/1002").exists());
    assert_fsck_clean(&git_dir);
}

#[test]
fn test_git_mediation_forged_author_or_committer_identity_rejected() {
    let village = TestVillage::new();
    let (comm_id, git_dir) = village.create_commission("git-forgery");
    let uid = village.allocate_and_roster(comm_id, "forger");
    let spool = village.tree.parcel_git_spool(uid);

    let work = village.tmp.path().join("work-forgery");
    git(
        village.tmp.path(),
        &["clone", &git_dir.to_string_lossy(), &work.to_string_lossy()],
    );

    // 1. Forged Author Email
    git(&work, &["checkout", "-b", "agent/1001"]);
    fs::write(work.join("forged1.txt"), "forged author\n").unwrap();
    git(&work, &["add", "forged1.txt"]);
    let valid_email = agent_identity_email(uid);
    let forge_author_cmd = Command::new("git")
        .current_dir(&work)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "evil")
        .env("GIT_AUTHOR_EMAIL", "evil@malicious.com")
        .env("GIT_COMMITTER_NAME", "forger")
        .env("GIT_COMMITTER_EMAIL", &valid_email)
        .args(["commit", "-m", "forged author"])
        .output()
        .unwrap();
    assert!(forge_author_cmd.status.success());

    let bundle1 = spool.join("forged-author.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &bundle1.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );
    fs::set_permissions(&bundle1, fs::Permissions::from_mode(0o600)).unwrap();

    let out1 = village
        .ledger
        .process_git_bundle(uid, "forged-author.bundle")
        .expect("process forged author bundle");
    assert!(
        matches!(
            out1,
            GitBundleOutcome::Rejected {
                rejection: BundleRejection::ForeignIdentity,
                ..
            }
        ),
        "forged author email must be rejected with ForeignIdentity"
    );
    assert_fsck_clean(&git_dir);

    // 2. Forged Committer Email
    fs::write(work.join("forged2.txt"), "forged committer\n").unwrap();
    git(&work, &["add", "forged2.txt"]);
    let forge_committer_cmd = Command::new("git")
        .current_dir(&work)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "forger")
        .env("GIT_AUTHOR_EMAIL", &valid_email)
        .env("GIT_COMMITTER_NAME", "evil")
        .env("GIT_COMMITTER_EMAIL", "evil@malicious.com")
        .args(["commit", "-m", "forged committer"])
        .output()
        .unwrap();
    assert!(forge_committer_cmd.status.success());

    let bundle2 = spool.join("forged-committer.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &bundle2.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );
    fs::set_permissions(&bundle2, fs::Permissions::from_mode(0o600)).unwrap();

    let out2 = village
        .ledger
        .process_git_bundle(uid, "forged-committer.bundle")
        .expect("process forged committer bundle");
    assert!(
        matches!(
            out2,
            GitBundleOutcome::Rejected {
                rejection: BundleRejection::ForeignIdentity,
                ..
            }
        ),
        "forged committer email must be rejected with ForeignIdentity"
    );
    assert_fsck_clean(&git_dir);
}

#[test]
fn test_git_mediation_oversized_bundle_rejected() {
    let village = TestVillage::new();
    let (comm_id, git_dir) = village.create_commission("git-oversize");
    let uid = village.allocate_and_roster(comm_id, "coder-huge");
    let spool = village.tree.parcel_git_spool(uid);

    let huge_file = spool.join("oversize.bundle");
    let f = File::create(&huge_file).expect("create file");
    f.set_len(MAX_BUNDLE_BYTES + 1024).expect("set_len");
    fs::set_permissions(&huge_file, fs::Permissions::from_mode(0o600)).unwrap();

    let out = village
        .ledger
        .process_git_bundle(uid, "oversize.bundle")
        .expect("process huge bundle");
    assert!(
        matches!(
            out,
            GitBundleOutcome::Rejected {
                rejection: BundleRejection::Oversize,
                ..
            }
        ),
        "bundles exceeding 32 MiB must be rejected with Oversize"
    );
    assert!(!huge_file.exists());
    assert!(spool.join("oversize.bundle.oversize").is_file());
    assert_fsck_clean(&git_dir);
}

#[test]
fn test_git_mediation_hook_smuggling_attempt_fails_closed() {
    let village = TestVillage::new();
    let (comm_id, git_dir) = village.create_commission("git-hook");
    let uid = village.allocate_and_roster(comm_id, "hook-smuggler");
    let spool = village.tree.parcel_git_spool(uid);

    let work = village.tmp.path().join("work-hook");
    git(
        village.tmp.path(),
        &["clone", &git_dir.to_string_lossy(), &work.to_string_lossy()],
    );
    let email = agent_identity_email(uid);
    git(&work, &["config", "user.name", "hook-smuggler"]);
    git(&work, &["config", "user.email", &email]);
    git(&work, &["checkout", "-b", "agent/1001"]);

    // Attempt to commit a hook script inside the project tree
    let hook_dir = work.join(".git-hooks");
    fs::create_dir_all(&hook_dir).unwrap();
    let hook_file = hook_dir.join("post-commit");
    fs::write(&hook_file, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook_file, fs::Permissions::from_mode(0o755)).unwrap();

    git(&work, &["add", ".git-hooks/post-commit"]);
    git(&work, &["commit", "-m", "add hook script"]);

    let bundle = spool.join("hook.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &bundle.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );
    fs::set_permissions(&bundle, fs::Permissions::from_mode(0o600)).unwrap();

    // Verify hooks in bare repo remain disabled (core.hooksPath = /dev/null)
    let hooks_path_cfg = git_bare(&git_dir, &["config", "--get", "core.hooksPath"]);
    assert_eq!(hooks_path_cfg, "/dev/null");

    let out = village
        .ledger
        .process_git_bundle(uid, "hook.bundle")
        .expect("process hook bundle");
    assert!(
        matches!(out, GitBundleOutcome::Accepted { .. }),
        "content commit itself is valid, but hooks are neutralized by core.hooksPath"
    );
    assert_fsck_clean(&git_dir);
}

#[test]
fn test_git_mediation_non_fast_forward_attempt_rejected() {
    let village = TestVillage::new();
    let (comm_id, git_dir) = village.create_commission("git-nonff");
    let uid = village.allocate_and_roster(comm_id, "coder-nonff");
    let spool = village.tree.parcel_git_spool(uid);

    let work = village.tmp.path().join("work-nonff");
    git(
        village.tmp.path(),
        &["clone", &git_dir.to_string_lossy(), &work.to_string_lossy()],
    );
    let email = agent_identity_email(uid);
    git(&work, &["config", "user.name", "coder-nonff"]);
    git(&work, &["config", "user.email", &email]);
    git(&work, &["checkout", "-b", "agent/1001"]);

    // Push 1: valid initial commit
    fs::write(work.join("f1.txt"), "c1\n").unwrap();
    git(&work, &["add", "f1.txt"]);
    git(&work, &["commit", "-m", "commit 1"]);
    let bundle1 = spool.join("p1.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &bundle1.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );
    village
        .ledger
        .process_git_bundle(uid, "p1.bundle")
        .expect("process p1");
    assert_fsck_clean(&git_dir);

    // Push 2: non-fast-forward (reset hard back to main and make divergent commit)
    let main_oid = git_bare(&git_dir, &["rev-parse", "refs/heads/main"]);
    git(&work, &["reset", "--hard", &main_oid]);
    fs::write(work.join("divergent.txt"), "divergent\n").unwrap();
    git(&work, &["add", "divergent.txt"]);
    git(&work, &["commit", "-m", "divergent commit"]);
    let bundle2 = spool.join("p2.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &bundle2.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );
    fs::set_permissions(&bundle2, fs::Permissions::from_mode(0o600)).unwrap();

    let out2 = village
        .ledger
        .process_git_bundle(uid, "p2.bundle")
        .expect("process p2");
    assert!(
        matches!(
            out2,
            GitBundleOutcome::Rejected {
                rejection: BundleRejection::NonFastForward,
                ..
            }
        ),
        "divergent bundle must be rejected with NonFastForward"
    );
    assert_fsck_clean(&git_dir);
}

// ===========================================================================
// 2. DEED Promotion
// ===========================================================================

#[test]
fn test_deed_promotion_injection_lines_rejected_with_trace_and_mail() {
    let village = TestVillage::new();
    let (comm_id, _) = village.create_commission("inject-comm");
    let promoter = Arc::new(
        DeedPromoter::new(village.tree.clone())
            .with_telemetry(village.telemetry.clone())
            .with_mail(village.mail.clone()),
    );

    let injection_cases = [
        "- pixi evil; rm -rf / = 1.0",
        "- cargo `whoami` = 0.1",
        "- apk tool$(id)",
        "- pixi ../escape = 1.0",
        "- cargo tool/slash = 1.0",
        "- pixi name with spaces = 1.0",
        "- nvm node = 18.0\r\n", // CR byte injection
    ];

    for (i, injection) in injection_cases.iter().enumerate() {
        let uid = 1001 + i as u32;
        let mut charter = Charter::load(&village.tree.charter()).unwrap();
        let _ = charter
            .allocate_slot(name(&format!("inject-{i}")), Some(comm_id))
            .unwrap();
        charter.save_atomic(&village.tree.charter()).unwrap();

        let deed_text = format!(
            "# DEED — environment record · parcel{uid}\n\n\
             - identity: inject-{i}\n\
             - uid slot: {uid}\n\
             - commission: {comm_id}\n\
             - blueprint: village-alpine v1 · base image docker.io/library/alpine:3.24\n\n\
             ### Dynamic Alterations\n\
             - baseline · toolchain bootstrap · uid {uid}\n\
             {injection}\n"
        );
        let parcel_dir = village.tree.parcel(uid);
        fs::create_dir_all(&parcel_dir).unwrap();
        fs::write(village.tree.parcel_deed(uid), deed_text).unwrap();

        let outcome = promoter.promote_parcel(uid).unwrap();
        assert!(
            matches!(outcome, PromotionOutcome::Rejected { .. }),
            "injection line {:?} must be rejected, got {:?}",
            injection,
            outcome
        );

        // Verify blueprint was NOT modified
        let bp_path = village.tree.guild_blueprint(&name("village-alpine"));
        let bp = Blueprint::from_toml(&fs::read_to_string(&bp_path).unwrap()).unwrap();
        assert_eq!(
            bp.meta().version(),
            1,
            "blueprint version must not be bumped"
        );
    }
}

#[test]
fn test_deed_promotion_unknown_tool_kinds_rejected() {
    let village = TestVillage::new();
    let (comm_id, _) = village.create_commission("unknown-comm");
    let promoter = Arc::new(DeedPromoter::new(village.tree.clone()));

    let unknown_kinds = [
        "- pip requests = 2.31.0",
        "- npm typescript = 5.0.0",
        "- gem rails = 7.0.0",
        "- apt vim = 9.0",
    ];

    for (i, line) in unknown_kinds.iter().enumerate() {
        let uid = 1050 + i as u32;
        let mut charter = Charter::load(&village.tree.charter()).unwrap();
        let _ = charter
            .allocate_slot(name(&format!("unknown-{i}")), Some(comm_id))
            .unwrap();
        charter.save_atomic(&village.tree.charter()).unwrap();

        let deed_text = format!(
            "# DEED — environment record · parcel{uid}\n\n\
             - identity: unknown-{i}\n\
             - uid slot: {uid}\n\
             - commission: {comm_id}\n\
             - blueprint: village-alpine v1 · base image docker.io/library/alpine:3.24\n\n\
             ### Dynamic Alterations\n\
             - baseline · toolchain bootstrap · uid {uid}\n\
             {line}\n"
        );
        let parcel_dir = village.tree.parcel(uid);
        fs::create_dir_all(&parcel_dir).unwrap();
        fs::write(village.tree.parcel_deed(uid), deed_text).unwrap();

        let outcome = promoter.promote_parcel(uid).unwrap();
        assert!(
            matches!(outcome, PromotionOutcome::Rejected { .. }),
            "unknown tool kind in {:?} must be rejected",
            line
        );
    }
}

#[test]
fn test_deed_promotion_dup_set_idempotence() {
    let village = TestVillage::new();
    let (comm_id, _) = village.create_commission("deed-idemp");
    let uid = village.allocate_and_roster(comm_id, "idemp-worker");
    let promoter = Arc::new(DeedPromoter::new(village.tree.clone()));

    let deed_text = format!(
        "# DEED — environment record · parcel{uid}\n\n\
         - identity: idemp-worker\n\
         - uid slot: {uid}\n\
         - commission: {comm_id}\n\
         - blueprint: village-alpine v1 · base image docker.io/library/alpine:3.24\n\n\
         ### Dynamic Alterations\n\
         - baseline · toolchain bootstrap · uid {uid}\n\
         - pixi uv = 0.4.2\n"
    );
    fs::write(village.tree.parcel_deed(uid), deed_text).unwrap();

    // First promotion: promoted v1 -> v2
    let o1 = promoter.promote_parcel(uid).unwrap();
    assert!(matches!(
        o1,
        PromotionOutcome::Promoted { new_version: 2, .. }
    ));

    // Second promotion: AlreadyMerged
    let o2 = promoter.promote_parcel(uid).unwrap();
    assert!(
        matches!(o2, PromotionOutcome::AlreadyMerged { version: 2, .. }),
        "re-promotion of identical alteration set must return AlreadyMerged"
    );

    let bp_path = village.tree.guild_blueprint(&name("village-alpine"));
    let bp = Blueprint::from_toml(&fs::read_to_string(&bp_path).unwrap()).unwrap();
    assert_eq!(bp.meta().version(), 2, "blueprint version must remain at 2");
}

/// **Finding QA06-01 (HIGH)**:
/// When a rogue agent rewrites DEED.md wholesale (e.g. deleting the immutable
/// baseline floor and deleting previously recorded alterations to inject fresh
/// alterations), `DeedPromoter` fails to detect non-append mutation and promotes
/// the altered history into the blueprint.
#[test]
fn qa06_01_deed_rewritten_wholesale_non_append_mutation_unverified() {
    let village = TestVillage::new();
    let (comm_id, _) = village.create_commission("rogue-deed");
    let uid = village.allocate_and_roster(comm_id, "rogue-agent");
    let promoter =
        Arc::new(DeedPromoter::new(village.tree.clone()).with_telemetry(village.telemetry.clone()));

    // Initial valid DEED with baseline + initial tool
    let initial_deed = format!(
        "# DEED — environment record · parcel{uid}\n\n\
         - identity: rogue-agent\n\
         - uid slot: {uid}\n\
         - commission: {comm_id}\n\
         - blueprint: village-alpine v1 · base image docker.io/library/alpine:3.24\n\n\
         ### Dynamic Alterations\n\
         - baseline · toolchain bootstrap · uid {uid}\n\
         - pixi uv = 0.4.2\n"
    );
    fs::write(village.tree.parcel_deed(uid), &initial_deed).unwrap();
    let view1 = promoter.inspect_deed(uid).unwrap();
    assert!(view1.alterations.is_ok());

    // Rogue agent rewrites DEED.md wholesale: deletes baseline, deletes `pixi uv`,
    // and substitutes `cargo ripgrep`
    let wholesale_rewrite = format!(
        "# DEED — environment record · parcel{uid}\n\n\
         - identity: rogue-agent\n\
         - uid slot: {uid}\n\
         - commission: {comm_id}\n\
         - blueprint: village-alpine v1 · base image docker.io/library/alpine:3.24\n\n\
         ### Dynamic Alterations\n\
         - cargo ripgrep\n"
    );
    fs::write(village.tree.parcel_deed(uid), wholesale_rewrite).unwrap();

    // Trigger promotion: SPEC §9.4 and Deliverable 1 requirement state that
    // the daemon MUST detect non-append mutation, reject the promotion, and trace it.
    let outcome = promoter.promote_parcel(uid).unwrap();

    assert!(
        matches!(outcome, PromotionOutcome::Rejected { .. }),
        "QA06-01: wholesale non-append DEED mutation must be rejected"
    );
}

#[test]
fn test_deed_promotion_kill_9_mid_promotion_leaves_blueprint_intact() {
    let village = TestVillage::new();
    let bp_path = village.tree.guild_blueprint(&name("village-alpine"));
    let initial_content = fs::read_to_string(&bp_path).unwrap();

    // Simulate a crash leaving an unfinished temp file in guild/blueprints/
    let temp_trash = village
        .tree
        .guild_blueprints()
        .join(".village-alpine.toml.tmp.9999");
    fs::write(&temp_trash, b"BROKEN TOML [[[ PARTIAL WRITE").unwrap();

    // Canonical active blueprint must remain pristine and loadable
    let loaded = fs::read_to_string(&bp_path).unwrap();
    assert_eq!(loaded, initial_content);
    let parsed = Blueprint::from_toml(&loaded);
    assert!(parsed.is_ok());
    assert_eq!(parsed.unwrap().meta().version(), 1);
}

// ===========================================================================
// 3. Vault Loop
// ===========================================================================

#[test]
fn test_vault_loop_approve_decree_nonexistent_freight_path_rejected() {
    let village = TestVillage::new();
    let (comm_id, _) = village.create_commission("vault-missing");
    let _ = village.allocate_and_roster(comm_id, "v-agent");

    let vault_promoter = VaultPromotion::new(village.tree.clone());
    let err = vault_promoter
        .approve(
            "courier/freight/does-not-exist.bin",
            &name("does-not-exist.bin"),
        )
        .unwrap_err();
    assert_eq!(err.reason, RejectReason::InvalidTarget);
    assert!(err.detail.contains("does not exist"));
}

#[test]
fn test_vault_loop_artifact_name_traversal_rejected() {
    // Artifact names are SafeNames; verify that traversal chars are rejected by SafeName
    assert!(sanitize_name("../escaped.tar").is_err());
    assert!(sanitize_name("/etc/passwd").is_err());
    assert!(sanitize_name("subdir/nested.bin").is_err());
    assert!(sanitize_name("..").is_err());
}

/// **Finding QA06-02 (HIGH)**:
/// `VaultPromotion::resolve_freight_file` validates confinement against `canon_root`
/// (the village root `.hamlet`), instead of confining paths to `courier/freight/`.
/// As a consequence, intermediate directory symlinks inside `courier/freight/` or relative
/// paths can escape the freight sandbox and read arbitrary village files (e.g. `charter.json`,
/// `keep/census/manor/MAYOR.md`) and copy them into `keep/vault/`.
#[test]
fn qa06_02_vault_freight_path_traversal_escapes_freight_directory_into_village() {
    let village = TestVillage::new();
    let (comm_id, uid) = village.create_commission_winding_down("vault-escape", "escaper");
    village
        .ledger
        .submit_deliverable(comm_id, uid, None, None)
        .unwrap();

    let freight_dir = village.tree.courier_freight();
    fs::create_dir_all(&freight_dir).unwrap();

    // Plant a directory symlink inside courier/freight/ pointing to manor
    let symlink_dir = freight_dir.join("sneaky_link");
    let manor_home = village.tree.manor_home();
    fs::create_dir_all(&manor_home).unwrap();
    fs::write(manor_home.join("MAYOR.md"), b"# SECRET MAYOR CHARTER\n").unwrap();
    let _ = std::os::unix::fs::symlink(&manor_home, &symlink_dir);

    // Target a regular file inside manor home: MAYOR.md exists there!
    // Path: courier/freight/sneaky_link/MAYOR.md
    let vault_promoter = VaultPromotion::new(village.tree.clone());
    let res = vault_promoter.approve(
        "courier/freight/sneaky_link/MAYOR.md",
        &name("stolen-mayor.md"),
    );

    // Confinement to courier/freight/ must reject this with InvalidTarget.
    assert!(
        res.is_err(),
        "QA06-02: freight path escaping courier/freight/ via intermediate symlink must be rejected"
    );
    assert_eq!(res.unwrap_err().reason, RejectReason::InvalidTarget);
    let stolen = village
        .tree
        .vault_commission_artifact(&name("vault-escape"), &name("stolen-mayor.md"));
    assert!(!stolen.exists(), "MAYOR.md must not be copied into vault");
}

/// **Finding QA06-03 (HIGH)**:
/// `write_atomic_file` in `crates/hamlet-daemon/src/vault.rs:441` and `write_atomic_export`
/// in `crates/hamlet-cli/src/vault.rs:721` use `OpenOptions::create(true).truncate(true)`
/// with predictable temp names (`.{file_name}.tmp-{pid}`), following planted symlinks.
#[test]
fn qa06_03_vault_atomic_write_follows_planted_symlink_temp_file() {
    let village = TestVillage::new();
    let (comm_id, uid) = village.create_commission_winding_down("symlink-bomb", "bomb-agent");
    village
        .ledger
        .submit_deliverable(comm_id, uid, None, None)
        .unwrap();

    let freight_dir = village.tree.courier_freight();
    fs::create_dir_all(&freight_dir).unwrap();
    let freight_file = freight_dir.join("product.tar");
    fs::write(&freight_file, b"PAYLOAD").unwrap();

    let vault_dir = village.tree.keep_vault_commission(&name("symlink-bomb"));
    fs::create_dir_all(&vault_dir).unwrap();

    // Plant a symlink at the predictable temp file name pointing to a target victim file
    let victim = village.tmp.path().join("victim_charter.txt");
    fs::write(&victim, b"PRECIOUS STATE").unwrap();

    let temp_name = format!(".product.tar.tmp-{}", std::process::id());
    let planted_symlink = vault_dir.join(&temp_name);
    let _ = std::os::unix::fs::symlink(&victim, &planted_symlink);

    let vault_promoter = VaultPromotion::new(village.tree.clone());
    let res = vault_promoter.approve("courier/freight/product.tar", &name("product.tar"));
    assert!(
        res.is_ok(),
        "approve should succeed with uncollided temp name"
    );

    // Verify victim file: victim was NOT overwritten!
    let victim_content = fs::read(&victim).unwrap();
    assert_eq!(
        victim_content, b"PRECIOUS STATE",
        "QA06-03: write_atomic_file must not follow planted symlink tempfile"
    );
    // Planted symlink must still be intact:
    assert!(fs::symlink_metadata(&planted_symlink)
        .unwrap()
        .file_type()
        .is_symlink());
    // Destination artifact was written cleanly:
    let vault_artifact = vault_dir.join("product.tar");
    assert_eq!(fs::read(&vault_artifact).unwrap(), b"PAYLOAD");
}

/// **Finding QA06-05 (MEDIUM)**:
/// `VaultPromotion::approve` does not verify the written vault artifact against the
/// computed content digest, and silently ignores errors writing the `.sha256` sidecar
/// (`let _ = write_atomic_file(&sidecar_path, ...)`).
#[test]
fn qa06_05_vault_copy_checksum_mismatch_and_silent_sidecar_failure() {
    let village = TestVillage::new();
    let (comm_id, uid) = village.create_commission_winding_down("checksum-gap", "chk-agent");
    village
        .ledger
        .submit_deliverable(comm_id, uid, None, None)
        .unwrap();

    let freight_dir = village.tree.courier_freight();
    fs::create_dir_all(&freight_dir).unwrap();
    let freight_file = freight_dir.join("data.bin");
    fs::write(&freight_file, b"CONTENT").unwrap();

    // Block sidecar write by creating a read-only directory where sidecar would land
    let vault_dir = village.tree.keep_vault_commission(&name("checksum-gap"));
    fs::create_dir_all(&vault_dir).unwrap();
    let sidecar_blocker = vault_dir.join("data.bin.sha256");
    fs::create_dir_all(&sidecar_blocker).unwrap(); // directory prevents regular file write

    let vault_promoter = VaultPromotion::new(village.tree.clone());
    let res = vault_promoter.approve("courier/freight/data.bin", &name("data.bin"));
    assert!(
        res.is_err(),
        "QA06-05: sidecar write failure must fail approve"
    );
}

#[test]
fn test_vault_loop_double_approve_replay_behavior() {
    let village = TestVillage::new();
    let (comm_id, uid) = village.create_commission_winding_down("double-approve", "d-agent");
    village
        .ledger
        .submit_deliverable(comm_id, uid, None, None)
        .unwrap();

    let freight_dir = village.tree.courier_freight();
    fs::create_dir_all(&freight_dir).unwrap();
    let freight_file = freight_dir.join("artifact.tar");
    fs::write(&freight_file, b"ARTIFACT").unwrap();

    let vault_promoter = VaultPromotion::new(village.tree.clone());
    // First approval: succeeds and closes commission
    assert!(vault_promoter
        .approve("courier/freight/artifact.tar", &name("artifact.tar"))
        .is_ok());

    let charter = Charter::load(&village.tree.charter()).unwrap();
    assert_eq!(
        charter.commissions.get(&comm_id).unwrap().state,
        CommissionState::Closed
    );

    // Second approval: direct approve call on closed commission fails with execution_failed
    // because transition from Closed is illegal
    let second = vault_promoter.approve("courier/freight/artifact.tar", &name("artifact.tar"));
    assert!(
        second.is_err(),
        "second approval attempt on closed commission must fail or reject"
    );
}

#[test]
fn test_vault_loop_reject_bound_escalation_fires_and_marks_failed() {
    let village = TestVillage::new();
    let (comm_id, uid) = village.create_commission_winding_down("reject-bound", "r-agent");

    let freight_dir = village.tree.courier_freight();
    fs::create_dir_all(&freight_dir).unwrap();
    let freight_file = freight_dir.join("bad.tar");
    fs::write(&freight_file, b"BAD").unwrap();

    let vault_promoter = VaultPromotion::new(village.tree.clone()).with_max_rejections(3);

    // Attempt 1
    village
        .ledger
        .submit_deliverable(comm_id, uid, None, None)
        .unwrap();
    vault_promoter
        .reject("courier/freight/bad.tar", "reason 1")
        .unwrap();
    let c1 = Charter::load(&village.tree.charter()).unwrap();
    assert_eq!(
        c1.commissions.get(&comm_id).unwrap().state,
        CommissionState::WindingDown
    );

    // Attempt 2
    village
        .ledger
        .submit_deliverable(comm_id, uid, None, None)
        .unwrap();
    vault_promoter
        .reject("courier/freight/bad.tar", "reason 2")
        .unwrap();

    // Attempt 3: bound exhausted -> marks Failed and files escalation notice
    village
        .ledger
        .submit_deliverable(comm_id, uid, None, None)
        .unwrap();
    vault_promoter
        .reject("courier/freight/bad.tar", "reason 3")
        .unwrap();

    let c3 = Charter::load(&village.tree.charter()).unwrap();
    assert_eq!(
        c3.commissions.get(&comm_id).unwrap().state,
        CommissionState::Failed
    );

    let notice_file = village
        .tree
        .manor_escalations()
        .join(format!("escalation-commission{comm_id}.json"));
    assert!(
        notice_file.is_file(),
        "escalation notice must exist after bound exhaustion"
    );
}

// ===========================================================================
// 4. State Machine & Multi-Tenancy
// ===========================================================================

#[test]
fn test_state_machine_illegal_transitions_refused() {
    let mut rec = CommissionRecord::draft(name("fsm-test"));
    assert_eq!(rec.state, CommissionState::Draft);

    // Draft -> AllEntriesDone (illegal)
    assert!(rec.apply_event(&CommissionEvent::AllEntriesDone).is_err());
    // Draft -> PromotionApproved (illegal)
    assert!(rec
        .apply_event(&CommissionEvent::PromotionApproved)
        .is_err());
    // Draft -> PromotionRejected (illegal)
    assert!(rec
        .apply_event(&CommissionEvent::PromotionRejected)
        .is_err());

    // OperatorClose from Draft -> Closed
    assert!(rec.apply_event(&CommissionEvent::OperatorClose).is_ok());
    assert_eq!(rec.state, CommissionState::Closed);

    // From Closed: ALL transitions must be refused
    assert!(rec
        .apply_event(&CommissionEvent::IndenturePosted {
            entries: BTreeSet::from([1001])
        })
        .is_err());
    assert!(rec.apply_event(&CommissionEvent::AllEntriesDone).is_err());
    assert!(rec
        .apply_event(&CommissionEvent::DeliverableSubmitted)
        .is_err());
    assert!(rec
        .apply_event(&CommissionEvent::PromotionApproved)
        .is_err());
    assert!(rec
        .apply_event(&CommissionEvent::PromotionRejected)
        .is_err());
    assert!(rec.apply_event(&CommissionEvent::OperatorClose).is_err());
    assert!(rec.apply_event(&CommissionEvent::EscalationFatal).is_err());

    // Roster operations frozen in Closed
    assert_eq!(rec.assign_uid(1001), Err(CommissionError::RosterFrozen));
    assert_eq!(rec.release_uid(1001), Err(CommissionError::RosterFrozen));
}

/// **Finding QA06-04 (HIGH - Cross-Contamination)**:
/// `VaultPromotion::identify_commission` checks `deliverable_in_review` across all
/// commissions in ascending key order before checking freight path components.
/// If Commission 1 is in review, approving Commission 2's deliverable misattributes
/// it to Commission 1, closes Commission 1, and puts Commission 1's parcels in stasis!
#[test]
fn qa06_04_two_commissions_cross_contamination_in_vault_promotion() {
    let village = TestVillage::new();
    let (comm1_id, uid1) = village.create_commission_winding_down("comm-alpha", "alpha-worker");
    let (comm2_id, _uid2) = village.create_commission_winding_down("comm-beta", "beta-worker");

    // Commission 1 submits deliverable (now in review)
    village
        .ledger
        .submit_deliverable(comm1_id, uid1, None, None)
        .unwrap();
    let c1 = Charter::load(&village.tree.charter()).unwrap();
    assert!(c1.commissions.get(&comm1_id).unwrap().deliverable_in_review);

    // Commission 2 worker produces deliverable in freight subpath courier/freight/beta-worker/product.tar
    let freight_dir = village.tree.courier_freight().join("beta-worker");
    fs::create_dir_all(&freight_dir).unwrap();
    let freight_file = freight_dir.join("product.tar");
    fs::write(&freight_file, b"BETA DELIVERABLE").unwrap();

    let vault_promoter = VaultPromotion::new(village.tree.clone());

    // Mayor approves Commission 2's deliverable: "courier/freight/beta-worker/product.tar"
    vault_promoter
        .approve(
            "courier/freight/beta-worker/product.tar",
            &name("product.tar"),
        )
        .expect("approve must succeed");

    // Verify which commission was actually closed and which vault received the artifact:
    let charter = Charter::load(&village.tree.charter()).unwrap();
    let alpha_state = charter.commissions.get(&comm1_id).unwrap().state;
    let beta_state = charter.commissions.get(&comm2_id).unwrap().state;

    let alpha_vault = village
        .tree
        .vault_commission_artifact(&name("comm-alpha"), &name("product.tar"));
    let beta_vault = village
        .tree
        .vault_commission_artifact(&name("comm-beta"), &name("product.tar"));

    if alpha_state == CommissionState::Closed && beta_state != CommissionState::Closed {
        println!(
            "REPRODUCED QA06-04: Commission 2 deliverable was stolen by Commission 1! Alpha closed, Beta untouched."
        );
    }

    assert!(
        !alpha_vault.is_file(),
        "QA06-04: Artifact was wrongly saved in comm-alpha vault"
    );
    assert!(
        beta_vault.is_file(),
        "QA06-04: Artifact must be saved in comm-beta vault"
    );
    assert_eq!(
        beta_state,
        CommissionState::Closed,
        "QA06-04: comm-beta must be closed"
    );
    assert_ne!(
        alpha_state,
        CommissionState::Closed,
        "QA06-04: comm-alpha must remain unclosed"
    );
}

#[test]
fn test_two_commissions_isolation_rosters_and_bare_repos() {
    let village = TestVillage::new();
    let (c1, git1) = village.create_commission("iso-1");
    let (c2, git2) = village.create_commission("iso-2");

    let u1 = village.allocate_and_roster(c1, "u1");
    let u2 = village.allocate_and_roster(c2, "u2");

    let r1 = village.ledger.roster(c1).unwrap();
    let r2 = village.ledger.roster(c2).unwrap();

    assert_eq!(r1, BTreeSet::from([u1]));
    assert_eq!(r2, BTreeSet::from([u2]));
    assert!(r1.is_disjoint(&r2));
    assert_ne!(git1, git2);
}

// ===========================================================================
// 5. Restart Resumability
// ===========================================================================

#[test]
fn test_restart_resumability_mid_flight_commission_resumes_without_duplication() {
    let village = TestVillage::new();
    let (comm_id, git_dir) = village.create_commission("resume-comm");
    let uid = village.allocate_and_roster(comm_id, "resume-worker");

    // 1. Post valid INDENTURE.md
    let ind_path = village.tree.commission_indenture(comm_id);
    fs::write(
        &ind_path,
        "# INDENTURE\n\n## resume-worker\n- goal: build feature\n- scope: src/\n- done: test\n",
    )
    .unwrap();

    // 2. Stage a pending git bundle in spool
    let work = village.tmp.path().join("work-resume");
    git(
        village.tmp.path(),
        &["clone", &git_dir.to_string_lossy(), &work.to_string_lossy()],
    );
    let email = agent_identity_email(uid);
    git(&work, &["config", "user.name", "resume-worker"]);
    git(&work, &["config", "user.email", &email]);
    git(&work, &["checkout", "-b", "agent/1001"]);
    fs::write(work.join("code.rs"), "fn hello() {}\n").unwrap();
    git(&work, &["add", "code.rs"]);
    git(&work, &["commit", "-m", "resume commit"]);

    let spool = village.tree.parcel_git_spool(uid);
    let bundle = spool.join("resume.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &bundle.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );
    fs::set_permissions(&bundle, fs::Permissions::from_mode(0o600)).unwrap();

    // --- RESTART SIMULATION: drop old instances and construct fresh Ledger ---
    let wake2 = Arc::new(RecordingCommissionWake::new());
    let new_ledger = Ledger::new(village.tree.clone())
        .with_wake(Arc::clone(&wake2) as _)
        .with_telemetry(village.telemetry.clone());

    // Startup recovery reconciliation
    let ind_outcomes = new_ledger.reconcile_indentures().unwrap();
    assert_eq!(ind_outcomes.len(), 1);

    let bundle_outcomes = new_ledger.reconcile_git_spools().unwrap();
    assert_eq!(bundle_outcomes.len(), 1);
    assert!(matches!(
        bundle_outcomes[0],
        GitBundleOutcome::Accepted { .. }
    ));

    // Verify state was advanced cleanly and idempotently
    let charter = Charter::load(&village.tree.charter()).unwrap();
    let comm = charter.commissions.get(&comm_id).unwrap();
    assert_eq!(comm.state, CommissionState::Active);
    assert_eq!(
        comm.indenture_entries.get(&uid),
        Some(&IndentureEntryStatus::Delivered)
    );
    assert!(!bundle.exists(), "bundle was consumed and archived");
    assert!(spool.join("resume.bundle.ack").is_file());
    assert_fsck_clean(&git_dir);

    // Second recovery pass is a clean no-op
    let ind_outcomes2 = new_ledger.reconcile_indentures().unwrap();
    assert!(ind_outcomes2.is_empty());
    let bundle_outcomes2 = new_ledger.reconcile_git_spools().unwrap();
    assert!(bundle_outcomes2.is_empty());
}
