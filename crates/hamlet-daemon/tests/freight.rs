//! Freight ACL audit integration tests (Sprint 04 step 03; SPEC §4/§8.3,
//! ADR-0021 — CONVENTIONS §9 doctrine: tempdir tree + `FakeRuntime`,
//! ACL listings as committed fixtures, no podman).
//!
//! Real [`FreightGuard`] over a tempdir village: root-ACL provisioning,
//! the four audit rules (owner-not-slot, world-writable, foreign UID,
//! named group), quarantine flags (0444, JSON, file-as-truth escalation),
//! sled traces, the Mayor-wake hook, promotion-read (argv golden + symlink
//! refusal), restart convergence via `audit_all`, and the full watcher
//! plane end to end.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::charter::{Charter, CommissionStatus};
use hamlet_core::config::Config;
use hamlet_core::freight::ViolationKind;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::freight::{
    spawn_freight_plane, AuditOutcome, FreightError, FreightGuard, QuarantineFlag, RecordingWake,
    ESCALATION_THRESHOLD,
};
use hamlet_daemon::podman::{ExecOutput, FakeRuntime};
use hamlet_daemon::supervise::{HostProcessSpawner, SessionSpawner, Supervisor};
use hamlet_daemon::telemetry::{AgentKey, Telemetry};
use hamlet_daemon::watch::{spawn_tree_watcher, Dispatcher};

const FAKEAGENT: &str = env!("CARGO_BIN_EXE_fakeagent");
const COMMISSION: u64 = 7;
const CLOSED: u64 = 8;
const MAYOR: u32 = 1000;
const SCRIBE: u32 = 1001;
const HERALD: u32 = 1002;
const CHRONICLER: u32 = 1003; // bound to the CLOSED commission

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

fn agent_key(uid: u32) -> AgentKey {
    AgentKey {
        commission: COMMISSION,
        uid,
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    tree: HamletTree,
    telemetry: Arc<Telemetry>,
    runtime: Arc<FakeRuntime>,
    wake: Arc<RecordingWake>,
    guard: Arc<FreightGuard>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(COMMISSION, CommissionStatus::Open);
    charter.commissions.insert(CLOSED, CommissionStatus::Closed);
    charter.reserve_mayor(name("mayor")).unwrap();
    assert_eq!(
        charter
            .allocate_slot(name("scribe"), Some(COMMISSION))
            .unwrap(),
        SCRIBE
    );
    assert_eq!(
        charter
            .allocate_slot(name("herald"), Some(COMMISSION))
            .unwrap(),
        HERALD
    );
    assert_eq!(
        charter
            .allocate_slot(name("chronicler"), Some(CLOSED))
            .unwrap(),
        CHRONICLER
    );
    charter.save_atomic(&tree.charter()).unwrap();

    let config = Config::default();
    let telemetry = Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).unwrap());
    let spawner: Arc<dyn SessionSpawner> = Arc::new(HostProcessSpawner::new(FAKEAGENT));
    let supervisor = Arc::new(Supervisor::new(
        tree.clone(),
        config,
        spawner,
        Arc::clone(&telemetry),
    ));
    let runtime = Arc::new(FakeRuntime::new());
    let wake = Arc::new(RecordingWake::new());
    let guard = Arc::new(FreightGuard::new(
        Arc::clone(&supervisor),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
        Arc::clone(&wake) as Arc<dyn hamlet_daemon::freight::MayorWake>,
    ));
    Fixture {
        _tmp: tmp,
        tree,
        telemetry,
        runtime,
        wake,
        guard,
    }
}

fn out(code: i32, stdout: &str) -> ExecOutput {
    ExecOutput {
        code,
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

/// Committed ACL-listing fixture shape (`getfacl -p -n -c` output).
fn listing(owner: u32, entries: &str) -> String {
    format!("# file: var/freight/sandbox\n# owner: {owner}\n# group: {owner}\n{entries}")
}

/// The freight-root listing before any grants (daemon-owned, base entries).
const ROOT_LISTING: &str = "\
# file: var/freight
# owner: 0
# group: 0
user::rwx
group::r-x
mask::rwx
other::---
";

/// A well-formed sandbox: owner 1001, secured for 1002 (+ Mayor read),
/// nothing for `other`.
const CLEAN_SANDBOX: &str = "\
user::rwx
user:1000:r-x
user:1002:rwx
group::---
mask::rwx
other::---
";

/// Deliberately world-writable.
const WORLD_WRITABLE: &str = "\
user::rwx
group::r-x
other::rwx
";

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

// ---------------------------------------------------------------------------
// Root ACL provisioning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ensure_root_acl_grants_every_slot_and_is_idempotent() {
    let fx = fixture();
    fx.runtime.script_exec_for("getfacl", out(0, ROOT_LISTING));

    let report = fx.guard.ensure_root_acl().await.unwrap();
    assert_eq!(report.granted, vec![MAYOR, SCRIBE, HERALD, CHRONICLER]);
    assert_eq!(report.present, 0);

    // Golden argv: one setfacl through the userns, sorted -m tokens, `--`
    // separator, root path last.
    let setfacl = fx
        .runtime
        .argv_log()
        .into_iter()
        .find(|argv| argv.iter().any(|tok| tok == "setfacl"))
        .expect("setfacl ran");
    assert_eq!(
        setfacl,
        vec![
            "podman",
            "unshare",
            "setfacl",
            "-m",
            "u:1000:rwx",
            "-m",
            "u:1001:rwx",
            "-m",
            "u:1002:rwx",
            "-m",
            "u:1003:rwx",
            "--",
            fx.tree.courier_freight().to_str().unwrap()
        ]
    );

    // Host-side braces: sticky mask-friendly root, hidden evidence dir.
    assert_eq!(mode_of(&fx.tree.courier_freight()), 0o1770);
    assert_eq!(mode_of(&fx.tree.courier_freight_quarantine()), 0o755);
}

#[tokio::test]
async fn ensure_root_acl_skips_setfacl_when_grants_present() {
    let fx = fixture();
    let granted = "# file: var/freight\n# owner: 0\n# group: 0\nuser::rwx\nuser:1000:rwx\nuser:1001:rwx\nuser:1002:rwx\nuser:1003:rwx\ngroup::r-x\nmask::rwx\nother::---\n";
    fx.runtime.script_exec_for("getfacl", out(0, granted));
    let report = fx.guard.ensure_root_acl().await.unwrap();
    assert!(report.granted.is_empty());
    assert_eq!(report.present, 4);
    assert!(
        !fx.runtime
            .argv_log()
            .iter()
            .any(|argv| argv.iter().any(|tok| tok == "setfacl")),
        "no ACL write when every slot grant exists"
    );
}

#[tokio::test]
async fn grant_slot_refuses_non_slot_uids_without_side_effects() {
    let fx = fixture();
    for uid in [0, 999, 2001, u32::MAX] {
        let err = fx.guard.grant_slot(uid).await.unwrap_err();
        assert!(
            matches!(err, FreightError::NotASlot { uid: u } if u == uid),
            "uid {uid} must be refused: {err}"
        );
    }
    assert!(fx.runtime.argv_log().is_empty(), "zero podman calls");
    // The Mayor (1000) and real slots pass argv validation (the manor
    // container mounts /var/freight/ too — SPEC §4 mount table).
    fx.runtime.script_exec_for("getfacl", out(0, ROOT_LISTING));
    fx.guard.grant_slot(MAYOR).await.unwrap();
    fx.guard.grant_slot(1500).await.unwrap();
    let argv = fx.runtime.argv_log().pop().unwrap();
    assert_eq!(
        argv,
        vec![
            "podman",
            "unshare",
            "setfacl",
            "-m",
            "u:1500:rwx",
            "--",
            fx.tree.courier_freight().to_str().unwrap()
        ]
    );
}

// ---------------------------------------------------------------------------
// The audit rules (ACL listings as fixtures)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn clean_sandbox_audits_clean_and_writes_nothing() {
    let fx = fixture();
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("exchange"))).unwrap();
    fx.runtime
        .script_exec_for("getfacl", out(0, &listing(SCRIBE, CLEAN_SANDBOX)));

    let outcome = fx.guard.audit(&name("exchange")).await.unwrap();
    assert_eq!(outcome, AuditOutcome::Clean { owner: SCRIBE });
    assert!(
        !fx.tree
            .courier_freight_quarantine_flag(&name("exchange"))
            .exists(),
        "clean sandboxes get no flag"
    );
}

#[tokio::test]
async fn world_writable_sandbox_is_quarantined_traced_and_ro() {
    let fx = fixture();
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("openbox"))).unwrap();
    fx.runtime
        .script_exec_for("getfacl", out(0, &listing(SCRIBE, WORLD_WRITABLE)));

    let outcome = fx.guard.audit(&name("openbox")).await.unwrap();
    match outcome {
        AuditOutcome::Flagged {
            owner,
            violations,
            escalated,
        } => {
            assert_eq!(owner, SCRIBE);
            assert!(!escalated, "first violation never escalates");
            assert_eq!(
                violations.iter().map(|v| v.kind).collect::<Vec<_>>(),
                vec![ViolationKind::WorldWritable]
            );
        }
        other => panic!("expected Flagged, got {other:?}"),
    }

    // Flag file: JSON evidence, mode 0444 (ro to agents).
    let flag_path = fx.tree.courier_freight_quarantine_flag(&name("openbox"));
    assert_eq!(mode_of(&flag_path) & 0o777, 0o444);
    let flag = QuarantineFlag::parse(&fs::read(&flag_path).unwrap()).unwrap();
    assert_eq!(flag.dir, "openbox");
    assert_eq!(flag.owner, SCRIBE);
    assert_eq!(flag.violations.len(), 1);
    assert_eq!(flag.violations[0].kind, "world_writable");

    // Sled trace under the owner's agent key (commission from the charter).
    let traces = fx.telemetry.recent_traces(agent_key(SCRIBE), 16).unwrap();
    assert!(
        traces.iter().any(|t| t.event == "freight-violation"
            && t.detail.contains("openbox")
            && t.detail.contains("world_writable")),
        "{traces:?}"
    );
    assert!(fx.wake.events().is_empty());
}

#[tokio::test]
async fn foreign_and_closed_commission_uids_are_flagged() {
    let fx = fixture();
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("rogue"))).unwrap();
    let entries = "\
user::rwx
user:4242:rwx
user:1003:r-x
group::---
group:77:r-x
mask::rwx
other::---
";
    fx.runtime
        .script_exec_for("getfacl", out(0, &listing(SCRIBE, entries)));

    let outcome = fx.guard.audit(&name("rogue")).await.unwrap();
    let AuditOutcome::Flagged { violations, .. } = outcome else {
        panic!("expected Flagged, got {outcome:?}");
    };
    assert_eq!(
        violations.iter().map(|v| v.kind).collect::<Vec<_>>(),
        vec![
            ViolationKind::ForeignUid, // 4242: not a charter slot at all
            ViolationKind::ForeignUid, // 1003: slot of the CLOSED commission
            ViolationKind::NamedGroup, // group:77 — UID-scoped sharing only
        ],
        "commission-scoped sharing: closed-commission slots are not grantees"
    );
}

#[tokio::test]
async fn owner_outside_the_slot_table_is_flagged() {
    let fx = fixture();
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("ghost"))).unwrap();
    fx.runtime
        .script_exec_for("getfacl", out(0, &listing(31337, CLEAN_SANDBOX)));

    let outcome = fx.guard.audit(&name("ghost")).await.unwrap();
    let AuditOutcome::Flagged {
        owner, violations, ..
    } = outcome
    else {
        panic!("expected Flagged, got {outcome:?}");
    };
    assert_eq!(owner, 31337);
    assert_eq!(violations[0].kind, ViolationKind::OwnerNotSlot);
    let flag = QuarantineFlag::parse(
        &fs::read(fx.tree.courier_freight_quarantine_flag(&name("ghost"))).unwrap(),
    )
    .unwrap();
    assert_eq!(flag.owner, 31337);
}

#[tokio::test]
async fn symlink_entries_are_flagged_without_a_listing() {
    let fx = fixture();
    std::os::unix::fs::symlink(
        Path::new("/etc"),
        fx.tree.courier_freight_sandbox(&name("evil")),
    )
    .unwrap();

    let outcome = fx.guard.audit(&name("evil")).await.unwrap();
    let AuditOutcome::Flagged {
        owner, violations, ..
    } = outcome
    else {
        panic!("expected Flagged, got {outcome:?}");
    };
    assert_eq!(owner, 0);
    assert_eq!(violations[0].kind, ViolationKind::NotADirectory);
    assert!(
        fx.runtime.argv_log().is_empty(),
        "lstat decides before any podman call: {:?}",
        fx.runtime.argv_log()
    );
}

#[tokio::test]
async fn vanished_dir_is_an_idempotent_noop() {
    let fx = fixture();
    let outcome = fx.guard.audit(&name("never-existed")).await.unwrap();
    assert_eq!(outcome, AuditOutcome::Missing);
    assert!(fx.runtime.argv_log().is_empty());
}

#[tokio::test]
async fn corrupt_listing_is_a_typed_error_not_a_verdict() {
    let fx = fixture();
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("junk"))).unwrap();
    fx.runtime
        .script_exec_for("getfacl", out(0, "not a listing at all\n"));
    let err = fx.guard.audit(&name("junk")).await.unwrap_err();
    assert!(matches!(err, FreightError::Parse(_)), "{err}");
    assert!(
        !fx.tree
            .courier_freight_quarantine_flag(&name("junk"))
            .exists(),
        "errors never masquerade as verdicts"
    );
}

#[tokio::test]
async fn failing_getfacl_is_a_tool_failure() {
    let fx = fixture();
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("junk"))).unwrap();
    let mut err_out = out(1, "");
    err_out.stderr = b"getfacl: Permission denied".to_vec();
    fx.runtime.script_exec_for("getfacl", err_out);
    let err = fx.guard.audit(&name("junk")).await.unwrap_err();
    match err {
        FreightError::ToolFailure { tool, code, .. } => {
            assert_eq!(tool, "getfacl");
            assert_eq!(code, 1);
        }
        other => panic!("{other}"),
    }
    assert!(
        !fx.tree
            .courier_freight_quarantine_flag(&name("junk"))
            .exists(),
        "tool failures leave the sandbox unaudited for retry"
    );
}

// ---------------------------------------------------------------------------
// Escalation (repeated violations → Mayor wake hook)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeated_violations_escalate_once_at_threshold() {
    let fx = fixture();
    fx.runtime
        .script_exec_for("getfacl", out(0, &listing(SCRIBE, WORLD_WRITABLE)));

    for i in 1..ESCALATION_THRESHOLD {
        let dir = name(&format!("leak-{i}"));
        fs::create_dir(fx.tree.courier_freight_sandbox(&dir)).unwrap();
        let outcome = fx.guard.audit(&dir).await.unwrap();
        match outcome {
            AuditOutcome::Flagged { escalated, .. } => assert!(!escalated, "below threshold"),
            other => panic!("{other:?}"),
        }
    }
    assert!(fx.wake.events().is_empty());

    // The threshold hit: hook fires once, with every quarantined dir.
    let last = name(&format!("leak-{}", ESCALATION_THRESHOLD));
    fs::create_dir(fx.tree.courier_freight_sandbox(&last)).unwrap();
    let outcome = fx.guard.audit(&last).await.unwrap();
    match outcome {
        AuditOutcome::Flagged { escalated, .. } => assert!(escalated),
        other => panic!("{other:?}"),
    }
    let events = fx.wake.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].owner, SCRIBE);
    assert_eq!(
        events[0].quarantined,
        vec![
            "leak-1".to_owned(),
            "leak-2".to_owned(),
            "leak-3".to_owned()
        ]
    );

    // A further violation by the same owner does not re-fire in-process.
    let extra = name("leak-4");
    fs::create_dir(fx.tree.courier_freight_sandbox(&extra)).unwrap();
    let outcome = fx.guard.audit(&extra).await.unwrap();
    match outcome {
        AuditOutcome::Flagged { escalated, .. } => assert!(!escalated, "dedupe"),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.wake.events().len(), 1);

    // The escalation traced under the owner's agent key.
    let traces = fx.telemetry.recent_traces(agent_key(SCRIBE), 32).unwrap();
    assert!(
        traces.iter().any(|t| t.event == "freight-escalation"),
        "{traces:?}"
    );
}

#[tokio::test]
async fn escalation_counts_are_file_as_truth_across_guards() {
    let fx = fixture();
    fx.runtime
        .script_exec_for("getfacl", out(0, &listing(SCRIBE, WORLD_WRITABLE)));
    for i in 1..ESCALATION_THRESHOLD {
        let dir = name(&format!("box-{i}"));
        fs::create_dir(fx.tree.courier_freight_sandbox(&dir)).unwrap();
        fx.guard.audit(&dir).await.unwrap();
    }
    // A FRESH guard (restart) sees the threshold from the flag files alone:
    // the next violation escalates even though this process fired nothing.
    let fresh = Arc::new(FreightGuard::new(
        Arc::new(Supervisor::new(
            fx.tree.clone(),
            Config::default(),
            Arc::new(HostProcessSpawner::new(FAKEAGENT)) as Arc<dyn SessionSpawner>,
            Arc::clone(&fx.telemetry),
        )),
        Arc::clone(&fx.runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
        Arc::clone(&fx.wake) as Arc<dyn hamlet_daemon::freight::MayorWake>,
    ));
    let last = name(&format!("box-{}", ESCALATION_THRESHOLD));
    fs::create_dir(fx.tree.courier_freight_sandbox(&last)).unwrap();
    let outcome = fresh.audit(&last).await.unwrap();
    match outcome {
        AuditOutcome::Flagged { escalated, .. } => assert!(escalated, "restart-resumable count"),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.wake.events().len(), 1);
}

// ---------------------------------------------------------------------------
// Promotion-read (Sprint 06 helper)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promotion_read_goes_through_the_userns_and_refuses_symlinks() {
    let fx = fixture();
    let sandbox = fx.tree.courier_freight_sandbox(&name("exchange"));
    fs::create_dir(&sandbox).unwrap();
    fs::write(sandbox.join("payload.md"), "artifact bytes").unwrap();
    // Host fs perms are IRRELEVANT to the daemon path: tighten them like a
    // real 0700 agent sandbox and the scripted `cat` still answers — the
    // capability lives in the userns, not in the ACLs (which stay untouched).
    fs::set_permissions(&sandbox, fs::Permissions::from_mode(0o700)).unwrap();
    fx.runtime
        .script_exec_for("cat", out(0, "PROMOTED-ARTIFACT"));

    let bytes = fx
        .guard
        .promotion_read(&name("exchange"), &name("payload.md"))
        .await
        .unwrap();
    assert_eq!(bytes, b"PROMOTED-ARTIFACT");
    let argv = fx
        .runtime
        .argv_log()
        .into_iter()
        .find(|argv| argv.iter().any(|tok| tok == "cat"))
        .expect("cat ran");
    assert_eq!(
        argv,
        vec![
            "podman",
            "unshare",
            "cat",
            "--",
            sandbox.join("payload.md").to_str().unwrap()
        ],
        "promotion-read golden: no ACL was touched, ever"
    );
    assert!(
        !argv
            .iter()
            .any(|tok| tok.contains("setfacl") || tok.contains("chmod")),
        "never relaxes ACLs on an agent's behalf (SPEC §4)"
    );

    // Symlinked sandbox → refused before any read.
    std::os::unix::fs::symlink(&sandbox, fx.tree.courier_freight_sandbox(&name("mirror"))).unwrap();
    let err = fx
        .guard
        .promotion_read(&name("mirror"), &name("payload.md"))
        .await
        .unwrap_err();
    assert!(matches!(err, FreightError::SymlinkRefused { .. }), "{err}");

    // Symlinked artifact inside a real sandbox → refused (SPEC §4 traversal
    // invariant: promoted names never escape).
    std::os::unix::fs::symlink(Path::new("/etc/passwd"), sandbox.join("leak")).unwrap();
    let err = fx
        .guard
        .promotion_read(&name("exchange"), &name("leak"))
        .await
        .unwrap_err();
    assert!(matches!(err, FreightError::SymlinkRefused { .. }), "{err}");

    // Directory target → NotAFile.
    fs::create_dir(sandbox.join("subdir")).unwrap();
    let err = fx
        .guard
        .promotion_read(&name("exchange"), &name("subdir"))
        .await
        .unwrap_err();
    assert!(matches!(err, FreightError::NotAFile { .. }), "{err}");
}

#[tokio::test]
async fn promotion_read_tool_failure_is_typed() {
    let fx = fixture();
    let sandbox = fx.tree.courier_freight_sandbox(&name("exchange"));
    fs::create_dir(&sandbox).unwrap();
    fs::write(sandbox.join("payload.md"), "bytes").unwrap();
    let mut err_out = out(1, "");
    err_out.stderr = b"cat: read error".to_vec();
    fx.runtime.script_exec_for("cat", err_out);
    let err = fx
        .guard
        .promotion_read(&name("exchange"), &name("payload.md"))
        .await
        .unwrap_err();
    match err {
        FreightError::ToolFailure { tool, code, .. } => {
            assert_eq!(tool, "cat");
            assert_eq!(code, 1);
        }
        other => panic!("{other}"),
    }
}

// ---------------------------------------------------------------------------
// Restart convergence + full plane
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plane_startup_converges_preexisting_sandboxes() {
    let fx = fixture();
    // Created while the daemon was down: one clean, one world-writable.
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("exchange"))).unwrap();
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("openbox"))).unwrap();
    // A visible entry that fails the §4 sanitizer (raw join — no SafeName
    // exists for it, exactly the hostile shape audit_all must skip).
    fs::create_dir(fx.tree.courier_freight().join("weird..name")).unwrap();

    fx.runtime
        .script_exec_for("exchange", out(0, &listing(SCRIBE, CLEAN_SANDBOX)));
    fx.runtime
        .script_exec_for("openbox", out(0, &listing(HERALD, WORLD_WRITABLE)));
    fx.runtime.script_exec_for("getfacl", out(0, ROOT_LISTING)); // root (registered last)

    let dispatcher = Dispatcher::new();
    let (guard, handle) = spawn_freight_plane(
        Arc::new(Supervisor::new(
            fx.tree.clone(),
            Config::default(),
            Arc::new(HostProcessSpawner::new(FAKEAGENT)) as Arc<dyn SessionSpawner>,
            Arc::clone(&fx.telemetry),
        )),
        Arc::clone(&fx.runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
        Arc::clone(&fx.wake) as Arc<dyn hamlet_daemon::freight::MayorWake>,
        &dispatcher,
    )
    .await
    .unwrap();
    assert_eq!(guard.tree(), &fx.tree);

    // The pre-existing violation converged to a flag before any event.
    let flag = QuarantineFlag::parse(
        &fs::read(fx.tree.courier_freight_quarantine_flag(&name("openbox"))).unwrap(),
    )
    .unwrap();
    assert_eq!(flag.owner, HERALD);
    // The sanitizer-rejecting name was skipped, never flagged.
    assert!(!fx
        .tree
        .courier_freight_quarantine()
        .read_dir()
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().contains("weird")));

    // Event-driven audit through the shared dispatcher: a NEW world-writable
    // dir created after startup lands via the watcher reaction.
    let watcher = spawn_tree_watcher(fx.tree.clone(), Arc::clone(&dispatcher), || {}).unwrap();
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("dropzone"))).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let flag_path = fx.tree.courier_freight_quarantine_flag(&name("dropzone"));
    while !flag_path.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        flag_path.exists(),
        "watcher reaction audited the new sandbox"
    );
    let flag = QuarantineFlag::parse(&fs::read(&flag_path).unwrap()).unwrap();
    // dropzone's getfacl matches the generic ROOT_LISTING script (owner 0
    // → not a slot): the verdict is OwnerNotSlot — proof the real listing
    // path ran, not a stale fixture.
    assert_eq!(flag.violations[0].kind, "owner_not_slot");

    let mut watcher = watcher;
    watcher.stop().await;
    handle.abort();
}

#[tokio::test]
async fn audit_all_reports_and_survives_vanished_entries() {
    let fx = fixture();
    fs::create_dir(fx.tree.courier_freight_sandbox(&name("one"))).unwrap();
    fx.runtime
        .script_exec_for("getfacl", out(0, &listing(SCRIBE, CLEAN_SANDBOX)));
    let report = fx.guard.audit_all().await.unwrap();
    assert_eq!((report.scanned, report.clean, report.flagged), (1, 1, 0));
    assert_eq!(report.skipped, 0);

    // Unsanitizable visible entry: skipped with a count, never a flag.
    fs::create_dir(fx.tree.courier_freight().join("-leading-dash")).unwrap();
    let report = fx.guard.audit_all().await.unwrap();
    assert_eq!(report.skipped, 1);
    assert_eq!(report.scanned, 1);
}
