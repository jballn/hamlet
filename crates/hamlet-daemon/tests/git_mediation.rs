//! Integration tests for Sprint 06 Step 04: Git bundle push mediation
//! (`SPEC §14`, `ADR-0037`).
//!
//! Covers:
//! 1. **Bundle round-trip**:
//!    - Seeded `main` and unborn `main` bare repositories.
//!    - `agent/<uid>` and `agent/<uid>/<topic>` branches.
//!    - Execution of the generated `hamlet-git-push` helper script and
//!      `agent_gitconfig` inside an isolated worktree.
//!    - Sequential fast-forward pushes advancing `refs/heads/agent/<uid>`.
//!    - Archive copy at `keep/census/archives/git/<n>/<uid>/<name>`, spool
//!      cleanup, `<name>.bundle.ack` receipt, `git_diff_ready` Mayor wake, and
//!      sled trace (`git_bundle_accepted`).
//!    - Diff body degradation from unified diff to deterministic `--stat`
//!      summary when the unified diff exceeds `MAX_DIFF_BODY_BYTES` (64 KiB).
//! 2. **Full rejection matrix (`ADR-0037 cl. 3/6`)**:
//!    - `BundleRejection::Oversize` (`> 32 MiB`)
//!    - `BundleRejection::VerifyFailed` (corrupt bundle bytes)
//!    - `BundleRejection::NotOwnBranch` (push to `refs/heads/main`, push to
//!      another agent's branch `refs/heads/agent/1002`, multi-ref bundle)
//!    - `BundleRejection::ForeignIdentity` (forged author email, forged
//!      committer email, wrong file owner UID, world-writable bundle mode,
//!      symlinked bundle)
//!    - `BundleRejection::NonFastForward` (divergent commit not descending from
//!      `main` or current `agent/<uid>` tip)
//!    - Every rejection leaves `commission<n>/git/` untouched (`git fsck --full`
//!      clean), renames the bundle to `<name>.bundle.<token>`, writes
//!      `<name>.bundle.ack`, mails the agent with the diagnostic, and logs
//!      `git_bundle_rejected` to sled traces.
//! 3. **Watcher & startup recovery (`Reaction::GitBundle`)**:
//!    - `spawn_git_spool_plane` + `spawn_tree_watcher` mediates bundles staged
//!      before startup and bundles dropped live via atomic rename.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::charter::Charter;
use hamlet_core::gitmed::{
    agent_branch_ref, agent_identity_email, BundleRejection, MAX_BUNDLE_BYTES, MAX_DIFF_BODY_BYTES,
};
use hamlet_core::packet::{EventKind, WakeReason};
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::ledger::{
    agent_gitconfig, git_push_helper_script, GitBundleAck, GitBundleAckStatus, GitBundleOutcome,
    Ledger, RecordingCommissionWake, EMPTY_TREE_OID,
};
use hamlet_daemon::telemetry::{MetricsScope, Telemetry};
use hamlet_daemon::watch::{spawn_tree_watcher, Dispatcher};

struct TestEnv {
    _tmp: tempfile::TempDir,
    tree: HamletTree,
    wake: Arc<RecordingCommissionWake>,
    telemetry: Arc<Telemetry>,
    ledger: Arc<Ledger>,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(".hamlet");
        let tree = HamletTree::ensure(&root).expect("ensure tree");
        Charter::empty()
            .save_atomic(&tree.charter())
            .expect("save empty charter");
        let telemetry =
            Arc::new(Telemetry::open(tmp.path().join(".hamlet-telemetry")).expect("open sled"));
        let wake = Arc::new(RecordingCommissionWake::new());
        let ledger = Arc::new(
            Ledger::new(tree.clone())
                .with_wake(Arc::clone(&wake) as _)
                .with_telemetry(Arc::clone(&telemetry)),
        );
        Self {
            _tmp: tmp,
            tree,
            wake,
            telemetry,
            ledger,
        }
    }

    fn allocate_and_roster(&self, commission: u64, identity: &str) -> u32 {
        let ident = sanitize_name(identity).expect("valid identity");
        let mut charter = Charter::load(&self.tree.charter()).expect("load charter");
        let uid = charter
            .allocate_slot(ident, Some(commission))
            .expect("allocate slot");
        charter
            .save_atomic(&self.tree.charter())
            .expect("save charter");
        self.ledger
            .assign_uid(commission, uid)
            .expect("assign uid to commission");
        let spool = self.tree.parcel_git_spool(uid);
        fs::create_dir_all(&spool).expect("create git-spool");
        fs::set_permissions(&spool, fs::Permissions::from_mode(0o700)).expect("chmod 0700 spool");
        uid
    }
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

fn read_mail_files(tree: &HamletTree, uid: u32) -> Vec<String> {
    let recipient = sanitize_name(&uid.to_string()).unwrap();
    let dir = tree.courier_mail_recipient(&recipient);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "mail"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| fs::read_to_string(p).expect("read mail"))
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Bundle round-trip (seeded, unborn, sequential pushes, topic branch, helper)
// ---------------------------------------------------------------------------

#[test]
fn bundle_round_trip_seeded_and_sequential_pushes_and_topic_branch() {
    let env = TestEnv::new();
    let seed = setup_seed_dir(env._tmp.path());
    let created = env
        .ledger
        .create_commission("alpha", "# Alpha\nShip feature.\n", Some(&seed))
        .expect("create seeded commission");
    let main_oid = created.seeded_commit.clone().expect("seeded commit oid");
    let uid = env.allocate_and_roster(created.id, "coder");

    // Clone the commission bare repo into a worktree and commit as `1001@hamlet.local`.
    let work = env._tmp.path().join("work-1001");
    git(
        env._tmp.path(),
        &[
            "clone",
            &created.git_dir.to_string_lossy(),
            &work.to_string_lossy(),
        ],
    );
    let email = agent_identity_email(uid);
    git(&work, &["config", "user.name", "coder (parcel1001)"]);
    git(&work, &["config", "user.email", &email]);
    git(&work, &["checkout", "-b", "agent/1001"]);

    fs::write(work.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").expect("write lib.rs");
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-m", "feat: add answer()"]);
    let head1 = git(&work, &["rev-parse", "HEAD"]);

    let spool = env.tree.parcel_git_spool(uid);
    let bundle1 = spool.join("push-01.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &bundle1.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );

    let outcome1 = env
        .ledger
        .process_git_bundle(uid, "push-01.bundle")
        .expect("process push-01");
    let GitBundleOutcome::Accepted {
        commission,
        uid: out_uid,
        bundle,
        branch_ref,
        base_oid,
        head_oid,
        archive_path,
        wake,
    } = outcome1
    else {
        panic!("expected Accepted, got {outcome1:?}");
    };
    assert_eq!(commission, created.id);
    assert_eq!(out_uid, uid);
    assert_eq!(bundle, "push-01.bundle");
    assert_eq!(branch_ref, agent_branch_ref(uid));
    assert_eq!(base_oid, main_oid);
    assert_eq!(head_oid, head1);

    // 1. Bare repo ref updated and fsck clean.
    assert_eq!(
        git_bare(&created.git_dir, &["rev-parse", "refs/heads/agent/1001"]),
        head1
    );
    assert_eq!(
        git_bare(&created.git_dir, &["rev-parse", "refs/heads/main"]),
        main_oid,
        "shared refs/heads/main must never be moved by an agent bundle"
    );
    assert_fsck_clean(&created.git_dir);

    // 2. Bundle archived to keep/census/archives/git/<n>/<uid>/push-01.bundle and removed from spool.
    assert_eq!(
        archive_path,
        env.tree
            .census_git_archive(created.id, uid)
            .join("push-01.bundle")
    );
    assert!(archive_path.is_file(), "archived bundle must exist");
    assert!(
        !bundle1.exists(),
        "applied bundle must be removed from spool"
    );

    // 3. `.ack` written in spool.
    let ack_raw = fs::read_to_string(spool.join("push-01.bundle.ack")).expect("read ack");
    let ack = GitBundleAck::parse(&ack_raw).expect("parse ack");
    assert_eq!(ack.status, GitBundleAckStatus::Accepted);
    assert_eq!(ack.branch_ref.as_deref(), Some("refs/heads/agent/1001"));
    assert_eq!(ack.base_oid.as_deref(), Some(main_oid.as_str()));
    assert_eq!(ack.head_oid.as_deref(), Some(head1.as_str()));

    // 4. Mayor woken with GitDiffReady carrying unified diff.
    assert_eq!(wake.kind().wake_reason(), WakeReason::GitDiffReady);
    assert!(
        matches!(
            wake.kind(),
            EventKind::GitDiffReady { commission: c, base, head }
                if *c == created.id && base.as_str() == main_oid && head.as_str() == head1
        ),
        "unexpected wake kind: {:?}",
        wake.kind()
    );
    assert!(
        wake.body().contains("+pub fn answer() -> u32 { 42 }"),
        "wake body must carry unified diff: {}",
        wake.body()
    );

    // 5. Second sequential fast-forward push on `agent/1001`.
    fs::write(
        work.join("lib.rs"),
        "pub fn answer() -> u32 { 42 }\npub fn double() -> u32 { 84 }\n",
    )
    .expect("update lib.rs");
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-m", "feat: add double()"]);
    let head2 = git(&work, &["rev-parse", "HEAD"]);

    let bundle2 = spool.join("push-02.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &bundle2.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );
    let outcome2 = env
        .ledger
        .process_git_bundle(uid, "push-02.bundle")
        .expect("process push-02");
    let GitBundleOutcome::Accepted {
        base_oid: base2,
        head_oid: out_head2,
        wake: wake2,
        ..
    } = outcome2
    else {
        panic!("expected Accepted on second push, got {outcome2:?}");
    };
    assert_eq!(base2, head1, "second push base must be previous agent tip");
    assert_eq!(out_head2, head2);
    assert!(wake2.body().contains("+pub fn double() -> u32 { 84 }"));

    // 6. Topic branch `refs/heads/agent/1002/feat-x` from a second rostered slot (1002) is accepted.
    let uid2 = env.allocate_and_roster(created.id, "reviewer");
    let work2 = env._tmp.path().join("work-1002");
    git(
        env._tmp.path(),
        &[
            "clone",
            &created.git_dir.to_string_lossy(),
            &work2.to_string_lossy(),
        ],
    );
    git(&work2, &["config", "user.name", "reviewer (parcel1002)"]);
    git(
        &work2,
        &["config", "user.email", &agent_identity_email(uid2)],
    );
    git(&work2, &["checkout", "-b", "agent/1002/feat-x"]);
    fs::write(work2.join("topic.txt"), "topic work\n").expect("write topic.txt");
    git(&work2, &["add", "topic.txt"]);
    git(&work2, &["commit", "-m", "feat: topic branch commit"]);
    let head_topic = git(&work2, &["rev-parse", "HEAD"]);
    let spool2 = env.tree.parcel_git_spool(uid2);
    let bundle_topic = spool2.join("topic.bundle");
    git(
        &work2,
        &[
            "bundle",
            "create",
            &bundle_topic.to_string_lossy(),
            "refs/heads/agent/1002/feat-x",
        ],
    );
    let outcome_topic = env
        .ledger
        .process_git_bundle(uid2, "topic.bundle")
        .expect("process topic.bundle");
    assert!(
        matches!(
            &outcome_topic,
            GitBundleOutcome::Accepted { branch_ref, head_oid, .. }
                if branch_ref == "refs/heads/agent/1002/feat-x" && head_oid == &head_topic
        ),
        "expected topic branch Accepted, got {outcome_topic:?}"
    );
    assert_eq!(
        git_bare(
            &created.git_dir,
            &["rev-parse", "refs/heads/agent/1002/feat-x"]
        ),
        head_topic
    );

    // 7. Wake spy recorded CommissionCreated + 3 GitDiffReady wakes; sled traces recorded 3 accepted bundles.
    let wakes = env.wake.events();
    let diff_wakes: Vec<_> = wakes
        .iter()
        .filter(|(_, w)| w.kind().wake_reason() == WakeReason::GitDiffReady)
        .collect();
    assert_eq!(diff_wakes.len(), 3);

    let snap = env
        .telemetry
        .metrics_for(MetricsScope::Commission(created.id), 32)
        .expect("metrics snapshot");
    let accepted_traces: Vec<_> = snap
        .recent_traces
        .iter()
        .filter(|t| t.event == "git_bundle_accepted")
        .collect();
    assert_eq!(accepted_traces.len(), 3);
}

#[test]
fn unborn_main_first_push_and_large_diff_degrades_to_stat_summary() {
    let env = TestEnv::new();
    let created = env
        .ledger
        .create_commission("unborn-comm", "# Unborn\nNo seed dir.\n", None)
        .expect("create unborn commission");
    assert!(created.seeded_commit.is_none());
    let uid = env.allocate_and_roster(created.id, "builder");

    let work = env._tmp.path().join("work-unborn");
    fs::create_dir_all(&work).expect("create work");
    git(&work, &["init", "-b", "agent/1001"]);
    let ident = sanitize_name("builder").unwrap();
    let gitconfig_path = env._tmp.path().join("builder.gitconfig");
    fs::write(&gitconfig_path, agent_gitconfig(uid, &ident)).expect("write gitconfig");
    git(&work, &["config", "user.name", "builder (parcel1001)"]);
    git(&work, &["config", "user.email", &agent_identity_email(uid)]);

    // Create a >64 KiB file so unified diff exceeds MAX_DIFF_BODY_BYTES (64 KiB).
    let line = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
    let large_body = line.repeat(1200); // ~78 KiB > 64 KiB
    assert!(large_body.len() as u64 > MAX_DIFF_BODY_BYTES);
    fs::write(work.join("big.txt"), &large_body).expect("write big.txt");
    git(&work, &["add", "big.txt"]);
    git(&work, &["commit", "-m", "feat: add large payload"]);
    let head = git(&work, &["rev-parse", "HEAD"]);

    let spool = env.tree.parcel_git_spool(uid);
    let bundle_path = spool.join("big.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &bundle_path.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );

    let outcome = env
        .ledger
        .process_git_bundle(uid, "big.bundle")
        .expect("process big.bundle");
    let GitBundleOutcome::Accepted {
        base_oid,
        head_oid,
        wake,
        ..
    } = outcome
    else {
        panic!("expected Accepted, got {outcome:?}");
    };
    assert_eq!(base_oid, EMPTY_TREE_OID);
    assert_eq!(head_oid, head);
    let body = wake.body();
    assert!(
        (body.len() as u64) <= MAX_DIFF_BODY_BYTES,
        "wake body must fit within 64 KiB cap"
    );
    assert!(
        body.contains("big.txt") && body.contains("1 file changed"),
        "wake body above 64 KiB must degrade to deterministic --stat summary, got:\n{body}"
    );
    assert!(
        !body.contains("0123456789abcdef"),
        "degraded --stat summary must not contain truncated unified diff lines"
    );
}

#[test]
fn helper_script_stages_valid_bundle_and_refuses_foreign_branch() {
    let env = TestEnv::new();
    let seed = setup_seed_dir(env._tmp.path());
    let created = env
        .ledger
        .create_commission("helper-comm", "# Helper\nTest script.\n", Some(&seed))
        .expect("create commission");
    let uid = env.allocate_and_roster(created.id, "coder");
    let spool = env.tree.parcel_git_spool(uid);

    let script = git_push_helper_script(uid).replace(
        &format!("SPOOL=\"/home/{uid}/git-spool\""),
        &format!("SPOOL=\"{}\"", spool.display()),
    );
    let script_path = env._tmp.path().join("hamlet-git-push");
    fs::write(&script_path, script).expect("write helper script");
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod 0755");

    let work = env._tmp.path().join("work-helper");
    git(
        env._tmp.path(),
        &[
            "clone",
            &created.git_dir.to_string_lossy(),
            &work.to_string_lossy(),
        ],
    );
    git(&work, &["config", "user.name", "coder (parcel1001)"]);
    git(&work, &["config", "user.email", &agent_identity_email(uid)]);

    // Running helper while on `main` must refuse to stage a bundle.
    let refuse = Command::new(&script_path)
        .current_dir(&work)
        .output()
        .expect("run helper on main");
    assert!(
        !refuse.status.success(),
        "hamlet-git-push must fail when HEAD is on main"
    );

    // Switch to `agent/1001`, commit, and invoke `hamlet-git-push my-push`.
    git(&work, &["checkout", "-b", "agent/1001"]);
    fs::write(work.join("helper.txt"), "pushed via helper\n").expect("write helper.txt");
    git(&work, &["add", "helper.txt"]);
    git(&work, &["commit", "-m", "feat: helper push"]);
    let head = git(&work, &["rev-parse", "HEAD"]);

    let ok = Command::new(&script_path)
        .arg("my-push")
        .current_dir(&work)
        .output()
        .expect("run helper on agent/1001");
    assert!(
        ok.status.success(),
        "hamlet-git-push failed: {}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(
        spool.join("my-push.bundle").is_file(),
        "helper must stage my-push.bundle into spool"
    );

    let outcome = env
        .ledger
        .process_git_bundle(uid, "my-push.bundle")
        .expect("process my-push.bundle");
    assert!(
        matches!(
            &outcome,
            GitBundleOutcome::Accepted { head_oid, .. } if head_oid == &head
        ),
        "expected Accepted for helper bundle, got {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Full rejection matrix (ADR-0037 cl. 3/6)
// ---------------------------------------------------------------------------

#[test]
fn rejection_matrix_enforces_all_adr_0037_clauses_and_keeps_fsck_clean() {
    let env = TestEnv::new();
    let seed = setup_seed_dir(env._tmp.path());
    let created = env
        .ledger
        .create_commission("matrix", "# Matrix\nTest rejections.\n", Some(&seed))
        .expect("create commission");
    let main_oid = created.seeded_commit.clone().expect("main oid");
    let uid = env.allocate_and_roster(created.id, "coder");
    let spool = env.tree.parcel_git_spool(uid);

    // Prepare a valid clone of the repo for building test bundles.
    let work = env._tmp.path().join("work-matrix");
    git(
        env._tmp.path(),
        &[
            "clone",
            &created.git_dir.to_string_lossy(),
            &work.to_string_lossy(),
        ],
    );
    let email = agent_identity_email(uid);
    git(&work, &["config", "user.name", "coder"]);
    git(&work, &["config", "user.email", &email]);

    // --- (a) Oversize bundle (> 32 MiB) -> BundleRejection::Oversize --------
    {
        let oversize_path = spool.join("huge.bundle");
        let f = File::create(&oversize_path).expect("create huge.bundle");
        f.set_len(MAX_BUNDLE_BYTES + 1).expect("set_len > 32 MiB");
        fs::set_permissions(&oversize_path, fs::Permissions::from_mode(0o600)).unwrap();

        let out = env
            .ledger
            .process_git_bundle(uid, "huge.bundle")
            .expect("process oversize");
        assert!(
            matches!(
                &out,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::Oversize,
                    ..
                }
            ),
            "expected Oversize rejection, got {out:?}"
        );
        assert!(!oversize_path.exists());
        assert!(spool.join("huge.bundle.oversize").is_file());
        let ack = GitBundleAck::parse(
            &fs::read_to_string(spool.join("huge.bundle.ack")).expect("read oversize ack"),
        )
        .unwrap();
        assert_eq!(ack.status, GitBundleAckStatus::Rejected);
        assert_eq!(ack.reason.as_deref(), Some("oversize"));
        assert_fsck_clean(&created.git_dir);
    }

    // --- (b) Corrupt bundle -> BundleRejection::VerifyFailed ----------------
    {
        let corrupt_path = spool.join("corrupt.bundle");
        fs::write(&corrupt_path, b"not a valid v2 git bundle header\n").unwrap();
        fs::set_permissions(&corrupt_path, fs::Permissions::from_mode(0o600)).unwrap();

        let out = env
            .ledger
            .process_git_bundle(uid, "corrupt.bundle")
            .expect("process corrupt");
        assert!(
            matches!(
                &out,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::VerifyFailed,
                    ..
                }
            ),
            "expected VerifyFailed rejection, got {out:?}"
        );
        assert!(!corrupt_path.exists());
        assert!(spool.join("corrupt.bundle.verify").is_file());
        let ack = GitBundleAck::parse(
            &fs::read_to_string(spool.join("corrupt.bundle.ack")).expect("read verify ack"),
        )
        .unwrap();
        assert_eq!(ack.reason.as_deref(), Some("verify"));
        assert_fsck_clean(&created.git_dir);
    }

    // --- (c) Push to shared `refs/heads/main` -> BundleRejection::NotOwnBranch
    {
        git(&work, &["checkout", "main"]);
        fs::write(work.join("main-hijack.txt"), "attempt to overwrite main\n").unwrap();
        git(&work, &["add", "main-hijack.txt"]);
        git(&work, &["commit", "-m", "hijack main"]);

        let main_bundle = spool.join("hijack-main.bundle");
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

        let out = env
            .ledger
            .process_git_bundle(uid, "hijack-main.bundle")
            .expect("process hijack-main");
        assert!(
            matches!(
                &out,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::NotOwnBranch,
                    ..
                }
            ),
            "expected NotOwnBranch on main push, got {out:?}"
        );
        assert!(spool.join("hijack-main.bundle.ref").is_file());
        assert_eq!(
            git_bare(&created.git_dir, &["rev-parse", "refs/heads/main"]),
            main_oid,
            "refs/heads/main in bare repo must remain untouched"
        );
        assert_fsck_clean(&created.git_dir);
        git(&work, &["reset", "--hard", &main_oid]);
    }

    // --- (d) Push to another agent's branch (`refs/heads/agent/1002`) -> NotOwnBranch
    {
        git(&work, &["checkout", "-b", "agent/1002", &main_oid]);
        fs::write(work.join("foreign-branch.txt"), "spoof 1002 branch\n").unwrap();
        git(&work, &["add", "foreign-branch.txt"]);
        git(&work, &["commit", "-m", "spoof 1002"]);

        let foreign_branch_bundle = spool.join("spoof-branch.bundle");
        git(
            &work,
            &[
                "bundle",
                "create",
                &foreign_branch_bundle.to_string_lossy(),
                "refs/heads/agent/1002",
            ],
        );
        fs::set_permissions(&foreign_branch_bundle, fs::Permissions::from_mode(0o600)).unwrap();

        let out = env
            .ledger
            .process_git_bundle(uid, "spoof-branch.bundle")
            .expect("process spoof-branch");
        assert!(
            matches!(
                &out,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::NotOwnBranch,
                    ..
                }
            ),
            "expected NotOwnBranch on agent/1002 push from uid 1001, got {out:?}"
        );
        assert!(spool.join("spoof-branch.bundle.ref").is_file());
        assert!(
            !created.git_dir.join("refs/heads/agent/1002").exists(),
            "refs/heads/agent/1002 must not be created"
        );
        assert_fsck_clean(&created.git_dir);
    }

    // --- (e) Multi-ref bundle -> BundleRejection::NotOwnBranch --------------
    {
        git(&work, &["checkout", "-b", "agent/1001", &main_oid]);
        fs::write(work.join("valid.txt"), "valid work\n").unwrap();
        git(&work, &["add", "valid.txt"]);
        git(&work, &["commit", "-m", "valid commit"]);

        let multi_bundle = spool.join("multi-ref.bundle");
        git(
            &work,
            &[
                "bundle",
                "create",
                &multi_bundle.to_string_lossy(),
                "refs/heads/agent/1001",
                "refs/heads/main",
            ],
        );
        fs::set_permissions(&multi_bundle, fs::Permissions::from_mode(0o600)).unwrap();

        let out = env
            .ledger
            .process_git_bundle(uid, "multi-ref.bundle")
            .expect("process multi-ref");
        assert!(
            matches!(
                &out,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::NotOwnBranch,
                    ..
                }
            ),
            "expected NotOwnBranch on multi-ref bundle, got {out:?}"
        );
        assert!(spool.join("multi-ref.bundle.ref").is_file());
        assert_fsck_clean(&created.git_dir);
    }

    // --- (f) Forged author email -> BundleRejection::ForeignIdentity --------
    {
        git(&work, &["checkout", "-B", "agent/1001", &main_oid]);
        fs::write(work.join("forged-author.txt"), "forged author\n").unwrap();
        git(&work, &["add", "forged-author.txt"]);
        git(
            &work,
            &[
                "-c",
                "user.email=1002@hamlet.local",
                "commit",
                "--author=Impersonator <1002@hamlet.local>",
                "-m",
                "forged author commit",
            ],
        );

        let forged_author_bundle = spool.join("forged-author.bundle");
        git(
            &work,
            &[
                "bundle",
                "create",
                &forged_author_bundle.to_string_lossy(),
                "refs/heads/agent/1001",
            ],
        );
        fs::set_permissions(&forged_author_bundle, fs::Permissions::from_mode(0o600)).unwrap();

        let out = env
            .ledger
            .process_git_bundle(uid, "forged-author.bundle")
            .expect("process forged-author");
        assert!(
            matches!(
                &out,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::ForeignIdentity,
                    ..
                }
            ),
            "expected ForeignIdentity on forged author email, got {out:?}"
        );
        assert!(spool.join("forged-author.bundle.identity").is_file());
        assert_fsck_clean(&created.git_dir);
    }

    // --- (g) Forged committer email (valid author, forged committer) --------
    {
        git(&work, &["checkout", "-B", "agent/1001", &main_oid]);
        fs::write(work.join("forged-committer.txt"), "forged committer\n").unwrap();
        git(&work, &["add", "forged-committer.txt"]);
        git(
            &work,
            &[
                "-c",
                "user.email=evil@example.com",
                "commit",
                &format!("--author=coder <{email}>"),
                "-m",
                "forged committer commit",
            ],
        );

        let forged_committer_bundle = spool.join("forged-committer.bundle");
        git(
            &work,
            &[
                "bundle",
                "create",
                &forged_committer_bundle.to_string_lossy(),
                "refs/heads/agent/1001",
            ],
        );
        fs::set_permissions(&forged_committer_bundle, fs::Permissions::from_mode(0o600)).unwrap();

        let out = env
            .ledger
            .process_git_bundle(uid, "forged-committer.bundle")
            .expect("process forged-committer");
        assert!(
            matches!(
                &out,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::ForeignIdentity,
                    ..
                }
            ),
            "expected ForeignIdentity on forged committer email, got {out:?}"
        );
        assert!(spool.join("forged-committer.bundle.identity").is_file());
        assert_fsck_clean(&created.git_dir);
    }

    // --- (h) Wrong file owner UID & world-writable permissions --------------
    {
        git(&work, &["checkout", "-B", "agent/1001", &main_oid]);
        fs::write(work.join("good-commit.txt"), "good commit\n").unwrap();
        git(&work, &["add", "good-commit.txt"]);
        git(&work, &["commit", "-m", "good commit"]);

        let wrong_owner_bundle = spool.join("wrong-owner.bundle");
        git(
            &work,
            &[
                "bundle",
                "create",
                &wrong_owner_bundle.to_string_lossy(),
                "refs/heads/agent/1001",
            ],
        );
        fs::set_permissions(&wrong_owner_bundle, fs::Permissions::from_mode(0o600)).unwrap();

        let out = env
            .ledger
            .process_git_bundle_with_expected_owner(uid, "wrong-owner.bundle", Some(999_999))
            .expect("process wrong-owner");
        assert!(
            matches!(
                &out,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::ForeignIdentity,
                    ..
                }
            ),
            "expected ForeignIdentity on wrong file owner UID, got {out:?}"
        );
        assert!(spool.join("wrong-owner.bundle.identity").is_file());

        let world_writable = spool.join("world-writable.bundle");
        git(
            &work,
            &[
                "bundle",
                "create",
                &world_writable.to_string_lossy(),
                "refs/heads/agent/1001",
            ],
        );
        fs::set_permissions(&world_writable, fs::Permissions::from_mode(0o666)).unwrap();

        let out_ww = env
            .ledger
            .process_git_bundle(uid, "world-writable.bundle")
            .expect("process world-writable");
        assert!(
            matches!(
                &out_ww,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::ForeignIdentity,
                    ..
                }
            ),
            "expected ForeignIdentity on world-writable bundle, got {out_ww:?}"
        );
        assert!(spool.join("world-writable.bundle.identity").is_file());
        assert_fsck_clean(&created.git_dir);
    }

    // --- (i) Non-fast-forward push (divergent history) ----------------------
    {
        // First apply the valid commit on `agent/1001` so `refs/heads/agent/1001` exists.
        let valid_bundle = spool.join("valid-base.bundle");
        git(
            &work,
            &[
                "bundle",
                "create",
                &valid_bundle.to_string_lossy(),
                "refs/heads/agent/1001",
            ],
        );
        fs::set_permissions(&valid_bundle, fs::Permissions::from_mode(0o600)).unwrap();
        let accepted = env
            .ledger
            .process_git_bundle(uid, "valid-base.bundle")
            .expect("apply valid-base");
        assert!(matches!(accepted, GitBundleOutcome::Accepted { .. }));

        // Now reset `agent/1001` back to `main` and create a divergent commit
        // that does NOT descend from the current `refs/heads/agent/1001` tip.
        git(&work, &["checkout", "-B", "agent/1001", &main_oid]);
        fs::write(work.join("divergent.txt"), "divergent history\n").unwrap();
        git(&work, &["add", "divergent.txt"]);
        git(&work, &["commit", "-m", "divergent commit"]);

        let nonff_bundle = spool.join("nonff.bundle");
        git(
            &work,
            &[
                "bundle",
                "create",
                &nonff_bundle.to_string_lossy(),
                "refs/heads/agent/1001",
            ],
        );
        fs::set_permissions(&nonff_bundle, fs::Permissions::from_mode(0o600)).unwrap();

        let out_nonff = env
            .ledger
            .process_git_bundle(uid, "nonff.bundle")
            .expect("process nonff");
        assert!(
            matches!(
                &out_nonff,
                GitBundleOutcome::Rejected {
                    rejection: BundleRejection::NonFastForward,
                    ..
                }
            ),
            "expected NonFastForward on divergent push, got {out_nonff:?}"
        );
        assert!(spool.join("nonff.bundle.nonff").is_file());
        let ack = GitBundleAck::parse(
            &fs::read_to_string(spool.join("nonff.bundle.ack")).expect("read nonff ack"),
        )
        .unwrap();
        assert_eq!(ack.reason.as_deref(), Some("nonff"));
        assert_fsck_clean(&created.git_dir);
    }

    // Verify refusal mails and sled traces were recorded for every rejection.
    let mails = read_mail_files(&env.tree, uid);
    assert!(
        mails.len() >= 9,
        "expected refusal mail per rejected bundle, got {}",
        mails.len()
    );
    assert!(mails.iter().any(|m| m.contains("oversize")));
    assert!(mails.iter().any(|m| m.contains("verify")));
    assert!(mails.iter().any(|m| m.contains("ref")));
    assert!(mails.iter().any(|m| m.contains("identity")));
    assert!(mails.iter().any(|m| m.contains("nonff")));

    let snap = env
        .telemetry
        .metrics_for(MetricsScope::Commission(created.id), 32)
        .expect("telemetry snapshot");
    let rejected_traces: Vec<_> = snap
        .recent_traces
        .iter()
        .filter(|t| t.event == "git_bundle_rejected")
        .collect();
    assert!(
        rejected_traces.len() >= 9,
        "expected sled traces for all rejections, got {}",
        rejected_traces.len()
    );
}

// ---------------------------------------------------------------------------
// 3. Watcher (`Reaction::GitBundle`) & startup recovery (`spawn_git_spool_plane`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn watcher_and_startup_recovery_mediate_staged_bundles() {
    let env = TestEnv::new();
    let seed = setup_seed_dir(env._tmp.path());
    let created = env
        .ledger
        .create_commission("watched", "# Watched\nWatch spool.\n", Some(&seed))
        .expect("create commission");
    let uid = env.allocate_and_roster(created.id, "coder");
    let spool = env.tree.parcel_git_spool(uid);

    // Build two commits in a local worktree.
    let work = env._tmp.path().join("work-watch");
    git(
        env._tmp.path(),
        &[
            "clone",
            &created.git_dir.to_string_lossy(),
            &work.to_string_lossy(),
        ],
    );
    git(&work, &["config", "user.name", "coder"]);
    git(&work, &["config", "user.email", &agent_identity_email(uid)]);
    git(&work, &["checkout", "-b", "agent/1001"]);

    // Stage `01-recovery.bundle` BEFORE starting the git-spool plane (tests startup recovery).
    fs::write(work.join("step1.txt"), "step 1\n").unwrap();
    git(&work, &["add", "step1.txt"]);
    git(&work, &["commit", "-m", "feat: step 1"]);
    let head1 = git(&work, &["rev-parse", "HEAD"]);
    let b1 = spool.join("01-recovery.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &b1.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );
    fs::set_permissions(&b1, fs::Permissions::from_mode(0o600)).unwrap();

    let dispatcher = Dispatcher::new();
    let _plane_handle =
        Ledger::spawn_git_spool_plane(Arc::clone(&env.ledger), &dispatcher).expect("spawn plane");
    let mut watcher_handle =
        spawn_tree_watcher(env.tree.clone(), Arc::clone(&dispatcher), || {}).expect("watcher");

    // Startup recovery should have already applied `01-recovery.bundle`.
    assert_eq!(
        git_bare(&created.git_dir, &["rev-parse", "refs/heads/agent/1001"]),
        head1
    );
    assert!(spool.join("01-recovery.bundle.ack").is_file());

    // Now stage `02-live.bundle` via hidden temp file + rename while watcher is live.
    fs::write(work.join("step2.txt"), "step 2\n").unwrap();
    git(&work, &["add", "step2.txt"]);
    git(&work, &["commit", "-m", "feat: step 2"]);
    let head2 = git(&work, &["rev-parse", "HEAD"]);
    let tmp_bundle = spool.join(".staging.tmp");
    let live_bundle = spool.join("02-live.bundle");
    git(
        &work,
        &[
            "bundle",
            "create",
            &tmp_bundle.to_string_lossy(),
            "refs/heads/agent/1001",
        ],
    );
    fs::set_permissions(&tmp_bundle, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&tmp_bundle, &live_bundle).expect("atomic rename into spool");

    // Wait up to 3s for the debounced watcher reaction to apply `02-live.bundle`.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if spool.join("02-live.bundle.ack").is_file() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for watcher to mediate 02-live.bundle"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        git_bare(&created.git_dir, &["rev-parse", "refs/heads/agent/1001"]),
        head2
    );
    watcher_handle.stop().await;
}
