//! Parcel provisioning integration tests (Sprint 02 step 03; CONVENTIONS §9).
//!
//! Daemon behavior against a tempdir `.hamlet/` tree with the mock seam:
//! [`FakeRuntime`] replaces podman (argv recorded through the same pure
//! builders, failure injection per verb), the charter is real (`save_atomic`
//! / `load` are the unit under test and cannot be meaningfully mocked).
//! Container *semantics* live in the guarded suite (`provision_guarded.rs`).
//!
//! Step 04 amendment: provisioning now ENDS with blueprint bootstrap (SPEC §5
//! — the daemon executes blueprints at provisioning), so every fixture scripts
//! the pinned artifact digests through [`script_pinned_digests`] and the
//! assertions below cover the image/bootstrap verbs too. Bootstrap itself is
//! tested in depth in `tests/bootstrap.rs`; nothing here was weakened.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{Charter, CommissionStatus, ParcelLease};
use hamlet_core::config::Config;
use hamlet_core::pins::ToolchainPins;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::slots::{SlotRecord, AGENT_SLOT_CAPACITY, FIRST_AGENT_UID};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::bootstrap::{script_pinned_digests, DEED_BASELINE_MARKER};
use hamlet_daemon::census::{deed_stub, proprietor_stub, ProvisionError, Provisioner};
use hamlet_daemon::hostperm::{
    MODE_FREIGHT, MODE_MAIL_ROOT, MODE_MAIL_SPOOL, MODE_PARCEL_DIR, MODE_PARCEL_DOC,
    MODE_PARCEL_HOME, MODE_PKGCACHE, MODE_SHARED_READONLY,
};
use hamlet_daemon::podman::{FakeRuntime, PodmanErrorKind};

const BRIEF: &str = "# scribe\n\nCore principles: copy faithfully, annotate sparingly.";

/// Digest the fake answers for `$HOME/.hamlet/toolchain.json` (the real one
/// is computed in-container from the manifest bytes bootstrap writes).
const TEST_MANIFEST_SHA256: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A [`FakeRuntime`] that answers every pinned-artifact digest check, so a
/// full bootstrap flow can run against the mock seam.
fn scripted_runtime(uids: &[u32]) -> Arc<FakeRuntime> {
    let runtime = Arc::new(FakeRuntime::new());
    let pins = ToolchainPins::default_hamlet().unwrap();
    for uid in uids {
        script_pinned_digests(&runtime, *uid, &pins, TEST_MANIFEST_SHA256);
    }
    runtime
}

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("test name must sanitize")
}

struct Fixture {
    tree: HamletTree,
    runtime: Arc<FakeRuntime>,
    provisioner: Provisioner,
    blueprint: Blueprint,
    _tmp: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    Charter::empty().save_atomic(&tree.charter()).unwrap();
    fs::write(tree.register_brief(&name("scribe")), BRIEF).unwrap();
    let runtime = scripted_runtime(&[1001, 1002]);
    let provisioner = Provisioner::new(
        tree.clone(),
        Config::default(),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    Fixture {
        tree,
        runtime,
        provisioner,
        blueprint: Blueprint::default_village_alpine().unwrap(),
        _tmp: tmp,
    }
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn argv_verbs(log: &[Vec<String>]) -> Vec<String> {
    log.iter()
        .map(|argv| argv.get(1).cloned().unwrap_or_default())
        .collect()
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

/// The full flow: slot → dirs/stubs → permissions → run → charter lease.
#[tokio::test]
async fn provision_happy_path_records_lease_and_hardens_host() {
    let f = fixture();
    let lease = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap();
    assert_eq!(
        lease,
        ParcelLease {
            identity: name("scribe"),
            stasis: false,
        }
    );
    let uid = 1001; // first sequential slot (SPEC §4)

    // --- tree state -------------------------------------------------------
    assert!(f.tree.parcel(uid).is_dir());
    assert_eq!(mode_of(&f.tree.parcel(uid)), MODE_PARCEL_DIR);
    assert_eq!(mode_of(&f.tree.parcel_home(uid)), MODE_PARCEL_HOME);
    for doc in [
        f.tree.parcel_title(uid),
        f.tree.parcel_deed(uid),
        f.tree.parcel_proprietor(uid),
    ] {
        assert_eq!(mode_of(&doc), MODE_PARCEL_DOC, "{}", doc.display());
    }
    let title = fs::read_to_string(f.tree.parcel_title(uid)).unwrap();
    assert!(title.starts_with(&format!("# TITLE — chain of title · parcel{uid}")));
    assert!(
        title.contains("provision_parcel(identity=\"scribe\", commission=7)"),
        "first chain-of-title entry names the trigger: {title}"
    );
    assert!(
        title.contains("unix "),
        "timestamp as lineage data: {title}"
    );
    let deed = fs::read_to_string(f.tree.parcel_deed(uid)).unwrap();
    assert!(
        deed.starts_with(&deed_stub(&name("scribe"), uid, 7, &f.blueprint)),
        "DEED.md keeps the step-03 stub verbatim: {deed}"
    );
    // Step 04: bootstrap records the toolchain baseline INSIDE the stub's
    // `### Dynamic Alterations` block (SPEC §9.4 parse target; the exact
    // entry shape is golden-tested in tests/bootstrap.rs).
    let (head, alterations) = deed
        .split_once("### Dynamic Alterations\n")
        .expect("the stub's alterations block survives");
    assert!(head.starts_with("# DEED — environment record"));
    assert!(
        alterations.contains(DEED_BASELINE_MARKER),
        "baseline entry recorded: {alterations}"
    );
    assert!(
        alterations.contains(TEST_MANIFEST_SHA256),
        "the manifest digest is the baseline's identity: {alterations}"
    );
    assert!(
        !alterations.contains("PIXI_CACHE_DIR=") && !alterations.contains("C.UTF-8"),
        "env VALUES are never recorded, keys only (SPEC §11.4): {alterations}"
    );
    let proprietor = fs::read_to_string(f.tree.parcel_proprietor(uid)).unwrap();
    assert_eq!(proprietor, proprietor_stub(&name("scribe"), uid, BRIEF));
    assert!(
        proprietor.contains("copy faithfully"),
        "brief seeded verbatim"
    );

    // --- permission model (ADR-0011 cl. 3 table) --------------------------
    let spool = f.tree.courier_mail_recipient(&name("1001"));
    assert_eq!(mode_of(&spool), MODE_MAIL_SPOOL);
    assert_eq!(mode_of(&f.tree.courier_mail()), MODE_MAIL_ROOT);
    assert_eq!(mode_of(&f.tree.courier_freight()), MODE_FREIGHT);
    assert_eq!(mode_of(&f.tree.commission_git(7)), MODE_SHARED_READONLY);
    assert_eq!(mode_of(&f.tree.tavern_gossip()), MODE_SHARED_READONLY);
    assert_eq!(mode_of(&f.tree.market_pkgcache()), MODE_PKGCACHE);

    // --- charter on disk (step 5, atomic) ----------------------------------
    let charter = Charter::load(&f.tree.charter()).unwrap();
    assert_eq!(charter.slots[&uid].identity, name("scribe"));
    assert_eq!(charter.slots[&uid].commission, Some(7));
    assert_eq!(charter.parcels[&uid], lease);
    assert_eq!(charter.commissions[&7], CommissionStatus::Open);
    assert_eq!(charter.agents[&name("scribe")].slot, uid);

    // --- podman call sequence (all through the pure builders) --------------
    // chown home → chown spool → derived-image probe → run → bootstrap execs.
    let log = f.runtime.argv_log();
    let verbs = argv_verbs(&log);
    assert_eq!(&verbs[..5], ["unshare", "unshare", "image", "run", "exec"]);
    assert!(
        verbs[4..].iter().all(|v| v == "exec"),
        "bootstrap only ever execs inside the running container: {verbs:?}"
    );
    assert!(
        !verbs.iter().any(|v| v == "build"),
        "the derived image already exists → no rebuild (ADR-0012 cl. 2)"
    );
    assert_eq!(
        log[0][..5],
        ["podman", "unshare", "chown", "1001:1001", "--"]
    );
    assert_eq!(log[0][5], f.tree.parcel_home(uid).to_str().unwrap());
    assert_eq!(log[1][5], spool.to_str().unwrap());
    let run = &log[3];
    // The parcel runs the DERIVED image, never the raw parent (ADR-0012).
    assert!(
        run.iter()
            .any(|a| a.starts_with("localhost/hamlet/village-alpine:v1-f2-c")),
        "derived image tag: {run:?}"
    );
    assert!(
        !run.contains(&"docker.io/library/alpine:3.24".to_owned()),
        "the parent image is a build input, not the parcel image"
    );
    assert!(run.contains(&"--user".to_owned()));
    assert!(run.contains(&"1001:1001".to_owned()));
    assert!(
        !run.iter().any(|a| a.starts_with("--userns")),
        "ADR-0011: default rootless userns, no explicit flag"
    );
    // The normative mount rows are in the run argv, ro where SPEC §4 says ro.
    let joined = run.join(" ");
    assert!(joined.contains(&format!(
        "{}:/home/1001/",
        f.tree.parcel_home(uid).display()
    )));
    assert!(joined.contains(&format!(
        "{}:/var/git/:ro",
        f.tree.commission_git(7).display()
    )));
    assert!(joined.contains(&format!(
        "{}:/var/context/:ro",
        f.tree.tavern_gossip().display()
    )));
}

/// Provisioning is crash-convergent: an intact parcel from a failed earlier
/// attempt is adopted (stubs never clobbered, same permanent slot), and a
/// second provision of the same identity is fully idempotent.
#[tokio::test]
async fn reprovision_adopts_intact_state_and_is_idempotent() {
    let f = fixture();
    // Simulate a crash remnant: parcel dir + a mutated TITLE.md survive.
    fs::create_dir_all(f.tree.parcel_home(1001)).unwrap();
    fs::write(
        f.tree.parcel_title(1001),
        "# TITLE — hand-amended lineage\n",
    )
    .unwrap();

    let lease = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(f.tree.parcel_title(1001)).unwrap(),
        "# TITLE — hand-amended lineage\n",
        "existing TITLE.md is adopted, never rewritten"
    );
    // DEED/PROPRIETOR were absent → created.
    assert!(f.tree.parcel_deed(1001).is_file());

    // Second provision: same slot, same lease, charter unchanged.
    let before = Charter::load(&f.tree.charter()).unwrap();
    let again = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap();
    assert_eq!(again, lease);
    assert_eq!(Charter::load(&f.tree.charter()).unwrap(), before);
    assert_eq!(before.slots.len(), 1, "permanent slot, allocated once");
}

// ---------------------------------------------------------------------------
// Rollback paths (FakeRuntime failure injection)
// ---------------------------------------------------------------------------

/// run_detached fails → dirs unwound, no container to remove, charter on
/// disk never saw the lease (slot included — save is step 5).
#[tokio::test]
async fn rollback_on_run_failure_leaves_no_orphans() {
    let f = fixture();
    f.runtime
        .fail_next_for("run", PodmanErrorKind::NonZeroExit { code: Some(125) });
    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProvisionError::Podman(e) if e.kind == PodmanErrorKind::NonZeroExit { code: Some(125) }),
        "{err}"
    );
    assert!(!f.tree.parcel(1001).exists(), "parcel dir unwound");
    assert!(!f.tree.courier_mail_recipient(&name("1001")).exists());
    assert!(!f.tree.commission(7).exists(), "commission dir unwound");
    assert_eq!(
        Charter::load(&f.tree.charter()).unwrap(),
        Charter::empty(),
        "no slot, no lease recorded on disk"
    );
    // The container never started → no `rm` in the log; the run was attempted.
    let verbs = argv_verbs(&f.runtime.argv_log());
    assert!(verbs.contains(&"run".to_owned()));
    assert!(!verbs.contains(&"rm".to_owned()));
}

/// chown (podman unshare) fails → unwound before any container exists.
#[tokio::test]
async fn rollback_on_chown_failure_never_starts_a_container() {
    let f = fixture();
    f.runtime
        .fail_next_for("unshare", PodmanErrorKind::NonZeroExit { code: Some(1) });
    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(matches!(err, ProvisionError::HostPerm(_)), "{err}");
    assert!(!f.tree.parcel(1001).exists());
    assert!(!f.tree.commission(7).exists());
    let verbs = argv_verbs(&f.runtime.argv_log());
    assert!(
        !verbs.contains(&"run".to_owned()),
        "no container was started"
    );
    assert_eq!(Charter::load(&f.tree.charter()).unwrap(), Charter::empty());
}

/// Charter save fails (a directory squats the legacy temp name — ADR-0006
/// fail-hard) AFTER the container started → rollback removes the container
/// (`rm --force`) and the dirs; the committed charter is untouched.
#[tokio::test]
async fn rollback_on_save_failure_removes_the_started_container() {
    let f = fixture();
    fs::create_dir(Charter::temp_path(&f.tree.charter())).unwrap();
    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(matches!(err, ProvisionError::Charter(_)), "{err}");
    assert!(!f.tree.parcel(1001).exists(), "parcel dir unwound");
    assert_eq!(
        Charter::load(&f.tree.charter()).unwrap(),
        Charter::empty(),
        "committed charter intact and lease-free"
    );
    let log = f.runtime.argv_log();
    assert!(
        log.iter()
            .any(|argv| argv == &["podman", "rm", "--force", "scribe"]),
        "the started container is force-removed — no orphans: {log:?}"
    );
    // Adversarial-rollback invariant: the run chowned home+spool AWAY from
    // the daemon (subuid, 0700) — the unwind must re-own them inside the
    // daemon's userns before removal, or removal would be locked out by
    // the very permissions step 3 applied (ADR-0011 consequence).
    let mut reclaim = vec![
        "podman".to_owned(),
        "unshare".to_owned(),
        "chown".to_owned(),
        "0:0".to_owned(),
        "--".to_owned(),
    ];
    reclaim.push(f.tree.parcel_home(1001).to_string_lossy().into_owned());
    reclaim.push(
        f.tree
            .courier_mail_recipient(&name("1001"))
            .to_string_lossy()
            .into_owned(),
    );
    assert!(
        log.contains(&reclaim),
        "reclaim chown 0:0 must precede dir removal: {log:?}"
    );
    let rm_pos = log.iter().position(|argv| argv[1] == "rm").unwrap();
    let reclaim_pos = log
        .iter()
        .rposition(|argv| argv[1] == "unshare" && argv[3] == "0:0")
        .unwrap();
    assert!(rm_pos < reclaim_pos, "container killed before reclaim");
}

/// An unwind action that itself fails is never swallowed: the caller gets
/// [`ProvisionError::RollbackFailed`] naming both the cause and the failure.
#[tokio::test]
async fn incomplete_unwind_is_reported_as_rollback_failed() {
    let f = fixture();
    fs::create_dir(Charter::temp_path(&f.tree.charter())).unwrap(); // save fails after run
    f.runtime
        .fail_next_for("rm", PodmanErrorKind::NonZeroExit { code: Some(1) });
    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, ProvisionError::RollbackFailed { .. }),
        "{msg}"
    );
    assert!(msg.contains("ROLLBACK INCOMPLETE"), "{msg}");
    assert!(msg.contains("podman rm --force scribe"), "{msg}");
    // The remaining unwind still completed: no orphan dirs.
    assert!(!f.tree.parcel(1001).exists());
    assert!(!f.tree.commission(7).exists());
}

/// A home THIS run created inside a pre-existing parcel dir is tracked on
/// its own: rollback removes it (reclaiming it from the slot first) while
/// the pre-existing parcel dir survives.
#[tokio::test]
async fn rollback_removes_home_created_inside_preexisting_parcel() {
    let f = fixture();
    fs::create_dir_all(f.tree.parcel(1001)).unwrap(); // remnant, home absent
    f.runtime
        .fail_next_for("run", PodmanErrorKind::NonZeroExit { code: Some(125) });
    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(matches!(err, ProvisionError::Podman(_)), "{err}");
    assert!(f.tree.parcel(1001).is_dir(), "pre-existing parcel survives");
    assert!(
        !f.tree.parcel_home(1001).exists(),
        "home created by THIS run is reclaimed and removed"
    );
    let log = f.runtime.argv_log();
    assert!(
        log.iter()
            .any(|argv| argv[1] == "unshare" && argv[3] == "0:0"),
        "reclaim chown 0:0 before removal: {log:?}"
    );
}

/// Rollback never destroys pre-existing state: a remnant parcel dir (crash
/// convergence case) survives a failed re-provision; only what THIS run
/// created is unwound.
#[tokio::test]
async fn rollback_preserves_preexisting_directories() {
    let f = fixture();
    fs::create_dir_all(f.tree.parcel_home(1001)).unwrap();
    fs::write(f.tree.parcel(1001).join("keep.txt"), "operator data").unwrap();
    f.runtime
        .fail_next_for("run", PodmanErrorKind::NonZeroExit { code: Some(125) });
    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(matches!(err, ProvisionError::Podman(_)), "{err}");
    assert!(
        f.tree.parcel(1001).is_dir(),
        "pre-existing parcel untouched"
    );
    assert_eq!(
        fs::read_to_string(f.tree.parcel(1001).join("keep.txt")).unwrap(),
        "operator data"
    );
    assert!(
        !f.tree.courier_mail_recipient(&name("1001")).exists(),
        "spool created by THIS run is unwound"
    );
    assert!(!f.tree.commission(7).exists());
}

// ---------------------------------------------------------------------------
// Pre-flight refusals (no side effects at all)
// ---------------------------------------------------------------------------

/// No register brief → typed refusal BEFORE any mutation (SPEC §5).
#[tokio::test]
async fn missing_register_brief_refuses_with_zero_side_effects() {
    let f = fixture();
    let err = f
        .provisioner
        .provision_parcel(&name("ghost"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProvisionError::MissingRegisterBrief { identity, .. } if *identity == name("ghost")),
        "{err}"
    );
    assert!(!f.tree.parcel(1001).exists());
    assert_eq!(Charter::load(&f.tree.charter()).unwrap(), Charter::empty());
    assert!(f.runtime.argv_log().is_empty(), "nothing spawned");
}

/// A symlinked register brief is refused, never followed (ADR-0007 duty).
#[tokio::test]
async fn symlinked_register_brief_is_refused_not_followed() {
    let f = fixture();
    let outside = f._tmp.path().join("outside-secret.md");
    fs::write(&outside, "must not be read").unwrap();
    let brief = f.tree.register_brief(&name("scribe"));
    fs::remove_file(&brief).unwrap();
    std::os::unix::fs::symlink(&outside, &brief).unwrap();
    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProvisionError::NotRegularFile { .. }),
        "{err}"
    );
    assert!(!f.tree.parcel(1001).exists());
    assert!(f.runtime.argv_log().is_empty());
}

/// Slot exhaustion is a hard error surfaced to the operator, never a wrap
/// (SPEC §4; CONVENTIONS §3 rule 5) — and leaves no side effects.
#[tokio::test]
async fn slot_exhaustion_is_a_hard_error() {
    let f = fixture();
    let mut charter = Charter::empty();
    for i in 0..AGENT_SLOT_CAPACITY {
        charter.slots.insert(
            FIRST_AGENT_UID + i as u32,
            SlotRecord::new(name(&format!("agent-{i}")), None),
        );
    }
    charter.save_atomic(&f.tree.charter()).unwrap();

    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProvisionError::Slot(hamlet_core::slots::SlotError::Exhausted { identity }) if *identity == name("scribe")),
        "{err}"
    );
    assert!(!f.tree.parcel(2001).exists());
    assert!(f.runtime.argv_log().is_empty());
    assert_eq!(
        Charter::load(&f.tree.charter()).unwrap().slots.len(),
        AGENT_SLOT_CAPACITY,
        "charter unchanged"
    );
}

/// A symlink squatting a stub document (TITLE.md) is refused — never
/// written through — and the refusal is non-destructive (ADR-0007).
#[tokio::test]
async fn symlinked_stub_document_is_refused_not_written_through() {
    let f = fixture();
    let outside = f._tmp.path().join("victim.md");
    fs::write(&outside, "original").unwrap();
    fs::create_dir_all(f.tree.parcel(1001)).unwrap();
    std::os::unix::fs::symlink(&outside, f.tree.parcel_title(1001)).unwrap();

    let err = f
        .provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProvisionError::NotRegularFile { .. }),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "original",
        "never written through the link"
    );
    // Pre-existing parcel dir survives the rollback (non-destructive).
    assert!(f.tree.parcel(1001).is_dir());
    assert_eq!(Charter::load(&f.tree.charter()).unwrap(), Charter::empty());
}

/// Restart-resumability (CONVENTIONS §3 rule 6) over the state this sprint
/// introduces: a FRESH provisioner on the same tree recovers identical
/// state from disk alone and continues sequential allocation.
#[tokio::test]
async fn restart_recovers_identical_state_from_the_tree() {
    let f = fixture();
    f.provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap();
    let before = Charter::load(&f.tree.charter()).unwrap();

    // "Kill the daemon": drop everything but the tree; rebuild from disk.
    let restarted = Provisioner::new(
        f.tree.clone(),
        Config::default(),
        scripted_runtime(&[1001, 1002]) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    assert_eq!(
        Charter::load(&restarted.tree().charter()).unwrap(),
        before,
        "recovered state is byte-identical (file-as-truth)"
    );
    fs::write(f.tree.register_brief(&name("mason")), "# mason").unwrap();
    restarted
        .provision_parcel(&name("mason"), 7, &f.blueprint)
        .await
        .unwrap();
    let after = Charter::load(&f.tree.charter()).unwrap();
    assert_eq!(after.slots[&1001].identity, name("scribe"), "slot persists");
    assert_eq!(
        after.slots[&1002].identity,
        name("mason"),
        "sequential continues"
    );
    assert_eq!(after.parcels[&1001], before.parcels[&1001]);
}

/// Two identities provision sequentially onto permanent slots 1001/1002,
/// each with its own container name, spool, and chown argv.
#[tokio::test]
async fn sequential_parcels_get_distinct_slots_and_spools() {
    let f = fixture();
    fs::write(f.tree.register_brief(&name("mason")), "# mason brief").unwrap();
    f.provisioner
        .provision_parcel(&name("scribe"), 7, &f.blueprint)
        .await
        .unwrap();
    f.provisioner
        .provision_parcel(&name("mason"), 7, &f.blueprint)
        .await
        .unwrap();

    let charter = Charter::load(&f.tree.charter()).unwrap();
    assert_eq!(charter.slots[&1001].identity, name("scribe"));
    assert_eq!(charter.slots[&1002].identity, name("mason"));
    assert_eq!(charter.parcels.len(), 2);
    assert!(f.tree.courier_mail_recipient(&name("1002")).is_dir());
    assert_eq!(mode_of(&f.tree.parcel_home(1002)), MODE_PARCEL_HOME);

    let log = f.runtime.argv_log();
    let runs: Vec<&Vec<String>> = log
        .iter()
        .filter(|argv| argv.get(1).map(String::as_str) == Some("run"))
        .collect();
    assert_eq!(runs.len(), 2);
    assert!(runs[0].contains(&"scribe".to_owned()));
    assert!(runs[1].contains(&"mason".to_owned()));
    assert!(runs[1].join(" ").contains(&format!(
        "{}:/home/1002/",
        f.tree.parcel_home(1002).display()
    )));
}
