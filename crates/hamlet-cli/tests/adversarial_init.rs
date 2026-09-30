//! Adversarial suite — `hamlet init` (Sprint 01, prompt 05-qa-engineer,
//! items 5–6 at the CLI boundary).
//!
//! SPEC §2 (normative topology), §11.4 (`.env` 0600), CONVENTIONS §6.
//!
//! Tests marked **EXPECTED FAILURE** document open HANDOFF findings (QA
//! does not fix product code):
//! - `init_refuses_foreign_content_inside_dot_hamlet` → **F01-03**
//! - `init_must_not_follow_symlinked_env_outside_tree` → **F01-02**
//! - `reinit_renormalizes_json_artifact_modes` → **F01-05**
//! - `env_path_occupied_by_directory_is_refused` → **F01-06**

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use hamlet_cli::init::{self, InitState};

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ---------------------------------------------------------------------------
// Passing contracts (regression armor for the QA prompt's init items)
// ---------------------------------------------------------------------------

/// Re-init idempotency: a second run changes NOTHING — every artifact is
/// byte-identical, every mode identical, report says Existed everywhere.
#[test]
fn reinit_is_byte_and_mode_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let first = init::run(tmp.path()).unwrap();
    let root = first.village_root.clone();

    fn snapshot(root: &Path) -> Vec<(String, Vec<u8>, u32)> {
        let mut out = Vec::new();
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>, u32)>) {
            let mut entries: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            entries.sort();
            for p in entries {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                let meta = fs::symlink_metadata(&p).unwrap();
                let mode = meta.permissions().mode() & 0o777;
                let bytes = if meta.is_dir() {
                    Vec::new()
                } else {
                    fs::read(&p).unwrap()
                };
                out.push((rel, bytes, mode));
                if meta.is_dir() {
                    walk(root, &p, out);
                }
            }
        }
        walk(root, root, &mut out);
        out
    }

    let before = snapshot(&root);
    let second = init::run(tmp.path()).unwrap();
    assert_eq!(second.tree, InitState::Existed);
    assert_eq!(second.config, InitState::Existed);
    assert_eq!(second.charter, InitState::Existed);
    assert_eq!(second.env, InitState::Existed);
    assert_eq!(snapshot(&root), before, "re-init must be a complete no-op");
}

/// `.env` is EXACTLY 0600 — after fresh init under a loose umask (via
/// subprocess so the umask is real), after tampering, and after re-init.
/// SPEC §11.4: `.env` is the only secret-bearing file in the tree.
#[test]
fn env_mode_is_exactly_0600_under_any_umask() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_hamlet");

    // Fresh init in a subprocess with umask 000 (maximally permissive).
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "umask 000; '{}' init '{}'",
            bin,
            tmp.path().display()
        ))
        .output()
        .unwrap();
    assert!(out.status.success(), "init failed: {:?}", out);
    let env_path = tmp.path().join(".hamlet/.env");
    assert_eq!(
        mode_of(&env_path),
        0o600,
        "SPEC §11.4: .env must be exactly 0600 even under umask 000"
    );

    // Tamper loose → re-init tightens back to 0600 (ensure() forces it),
    // content preserved.
    fs::write(&env_path, "HAMLET_MODEL_API_KEY=test-key-do-not-use\n").unwrap();
    fs::set_permissions(&env_path, fs::Permissions::from_mode(0o644)).unwrap();
    init::run(tmp.path()).unwrap();
    assert_eq!(
        mode_of(&env_path),
        0o600,
        "re-init must re-tighten a loosened .env"
    );
    assert_eq!(
        fs::read_to_string(&env_path).unwrap(),
        "HAMLET_MODEL_API_KEY=test-key-do-not-use\n",
        ".env content must survive re-init"
    );

    // Other artifacts must NOT be 0600-class secrets (canonical 0644).
    assert_eq!(mode_of(&tmp.path().join(".hamlet/charter.json")), 0o644);
    assert_eq!(mode_of(&tmp.path().join(".hamlet/config.json")), 0o644);
}

/// A regular FILE squatting on `.hamlet` must be refused with a typed error
/// (never overwritten, never adopted).
#[test]
fn dot_hamlet_occupied_by_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let squatter = tmp.path().join(".hamlet");
    fs::write(&squatter, "i am not a directory").unwrap();
    let err = init::run(tmp.path()).unwrap_err();
    assert!(
        squatter.is_file(),
        "refused init must not touch the squatter"
    );
    assert_eq!(
        fs::read_to_string(&squatter).unwrap(),
        "i am not a directory"
    );
    // Typed I/O error naming the failing step (stat pre-check or ensure).
    assert!(
        matches!(err, hamlet_cli::init::InitError::Io { .. }),
        "{err}"
    );
    assert!(!err.to_string().is_empty());
}

/// Item 6 end-to-end through the REAL binary: init → charter on disk →
/// allocate via core against that disk state → save → simulated daemon
/// restart (subprocess child re-execs, loads from disk ONLY, allocates
/// more, saves, `exit(9)` = kill -9 semantics) → parent reloads from disk
/// → identical state, byte-identical canonical render.
#[test]
fn full_cycle_binary_init_then_crash_restart_is_identical() {
    use hamlet_core::charter::Charter;
    use hamlet_core::sanitize::sanitize_name;
    use hamlet_core::slots::{self, SlotStatus};

    fn name(s: &str) -> hamlet_core::sanitize::SafeName {
        sanitize_name(s).unwrap()
    }

    if std::env::var_os("HAMLET_RESTART_CHILD").is_some() {
        // Child phase: fresh load from disk only, mutate, save, die hard.
        let path = std::path::PathBuf::from(std::env::var_os("HAMLET_RESTART_CHARTER").unwrap());
        let mut c = Charter::load(&path).unwrap();
        c.allocate_slot(name("child-agent"), None).unwrap();
        c.save_atomic(&path).unwrap();
        std::process::exit(9); // kill -9 semantics: no destructors
    }

    let tmp = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_hamlet");
    let out = Command::new(bin)
        .args(["init", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let charter_path = tmp.path().join(".hamlet/charter.json");

    // Run 1 (in-process "daemon"): load from disk only, allocate, save.
    let mut c1 = Charter::load(&charter_path).unwrap();
    c1.reserve_mayor(name("manor")).unwrap();
    let ada = c1.allocate_slot(name("ada"), None).unwrap();
    let grace = c1.allocate_slot(name("grace"), None).unwrap();
    slots::hibernate(&mut c1.slots, grace).unwrap();
    c1.save_atomic(&charter_path).unwrap();
    let expected_bytes = c1.render();

    // Simulated daemon restart in a SEPARATE PROCESS that dies by exit(9).
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "full_cycle_binary_init_then_crash_restart_is_identical",
            "--exact",
        ])
        .env("HAMLET_RESTART_CHILD", "1")
        .env("HAMLET_RESTART_CHARTER", &charter_path)
        .status()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), None);
    assert_eq!(
        status.code(),
        Some(9),
        "child must die via exit(9): {status:?}"
    );

    // Parent: fresh load from disk only → identical state.
    let mut c2 = Charter::load(&charter_path).unwrap();
    assert_eq!(c2.slots[&ada].identity, name("ada"));
    assert_eq!(c2.slots[&grace].status, SlotStatus::Hibernated);
    assert_eq!(c2.allocate_slot(name("child-agent"), None).unwrap(), 1003);
    // Everything the pre-crash run committed is intact and canonical:
    // re-render WITHOUT the child's slot equals the pre-crash bytes.
    let mut without_child = c2.clone();
    without_child.slots.remove(&1003);
    assert_eq!(
        without_child.render(),
        expected_bytes,
        "byte-identical canonical render"
    );

    // `hamlet init` again after all of this: complete no-op on artifacts.
    let report = init::run(tmp.path()).unwrap();
    assert_eq!(report.charter, InitState::Existed);
    assert_eq!(fs::read_to_string(&charter_path).unwrap(), c2.render());
}

// ---------------------------------------------------------------------------
// EXPECTED FAILURES — open findings (do not "fix" the tests)
// ---------------------------------------------------------------------------

/// **F01-03 — EXPECTED FAILURE until fixed.**
///
/// QA contract (prompt 05 item 5): "init into a non-empty foreign dir
/// refuses". SPEC §2: the tree is NORMATIVE topology; adopting arbitrary
/// foreign content as a village means `charter.json`-driven behavior is
/// derived from a directory hamlet never validated.
///
/// EXPECTED: `init::run` refuses when `.hamlet/` exists but contains
/// entries outside the normative layout (foreign files/dirs), with a typed
/// error, leaving the foreign content untouched.
/// ACTUAL: `crates/hamlet-cli/src/init.rs` `run()` only stats its three
/// artifacts (`has_content`); any foreign content is silently adopted
/// (report says `tree: Existed`) and hamlet artifacts are scattered into
/// the foreign directory.
#[test]
fn init_refuses_foreign_content_inside_dot_hamlet() {
    let tmp = tempfile::tempdir().unwrap();
    let hamlet = tmp.path().join(".hamlet");
    fs::create_dir_all(hamlet.join("some-other-app/cache")).unwrap();
    fs::write(hamlet.join("NOT_MINE.txt"), "foreign app data").unwrap();

    let result = init::run(tmp.path());
    assert!(
        result.is_err(),
        "F01-03: init must REFUSE a foreign non-empty .hamlet/ dir; instead it adopted it: {:?}",
        result.map(|r| (r.tree, r.config, r.charter, r.env))
    );
    // Refusal must not have touched the foreign content.
    assert_eq!(
        fs::read_to_string(hamlet.join("NOT_MINE.txt")).unwrap(),
        "foreign app data"
    );
    assert!(
        !hamlet.join("charter.json").exists(),
        "no artifacts inside a refused dir"
    );
}

/// **F01-02 — EXPECTED FAILURE until fixed.** (SECURITY)
///
/// `.hamlet/.env` planted as a SYMLINK pointing outside the tree:
/// `HamletTree::ensure` (`crates/hamlet-core/src/tree.rs`) uses
/// `env.exists()` (follows symlinks), `File::create` (follows symlinks) and
/// `set_permissions` (follows symlinks). Observed:
/// 1. Existing external target file gets chmod 0600 THROUGH the symlink
///    (victim mode 0644 → 0600) — a mode-oracle/DoS primitive against any
///    file the invoking user can write.
/// 2. Dangling symlink: `File::create` CREATES the external target file
///    (0600, empty — or template-filled via init's empty-file branch),
///    i.e. init writes outside the village tree.
///
/// Violates SPEC §2 name-safety ("traversal is blocked at the API
/// boundary"), §11.4 (`.env` is the tree's secret file), CONVENTIONS §6.
/// EXPECTED: `ensure`/init detect a non-regular `.env` (lstat) and either
/// refuse with a typed error or atomically replace the symlink with a real
/// 0600 file — never touch the link target.
#[test]
fn init_must_not_follow_symlinked_env_outside_tree() {
    // Case 1: symlink to an EXISTING external file.
    let tmp = tempfile::tempdir().unwrap();
    let victim = tmp.path().join("victim-outside-tree");
    fs::write(&victim, "IMPORTANT=victim-data\n").unwrap();
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
    let hamlet = tmp.path().join(".hamlet");
    fs::create_dir_all(&hamlet).unwrap();
    std::os::unix::fs::symlink(&victim, hamlet.join(".env")).unwrap();

    // Refusal OR safe replacement of the symlink are both acceptable fixes;
    // operating THROUGH the link (any effect on the external file) is not.
    let result = init::run(tmp.path());
    assert_eq!(
        mode_of(&victim),
        0o644,
        "F01-02: external file was chmod 0600 THROUGH the .env symlink (SPEC §11.4 violation)"
    );
    assert_eq!(
        fs::read_to_string(&victim).unwrap(),
        "IMPORTANT=victim-data\n"
    );
    if result.is_ok() {
        let env = hamlet.join(".env");
        let meta = fs::symlink_metadata(&env).unwrap();
        assert!(
            meta.file_type().is_file(),
            "F01-02: .env must end as a regular file, not a link"
        );
        assert_eq!(mode_of(&env), 0o600);
    }

    // Case 2: DANGLING symlink — init must not create files outside the tree.
    let tmp2 = tempfile::tempdir().unwrap();
    let ghost = tmp2.path().join("created-outside-tree");
    let hamlet2 = tmp2.path().join(".hamlet");
    fs::create_dir_all(&hamlet2).unwrap();
    std::os::unix::fs::symlink(&ghost, hamlet2.join(".env")).unwrap();

    let result2 = init::run(tmp2.path());
    assert!(
        !ghost.exists(),
        "F01-02: init FOLLOWED a dangling .env symlink and created {:?} outside the tree",
        ghost
    );
    if result2.is_ok() {
        let env = hamlet2.join(".env");
        let meta = fs::symlink_metadata(&env).unwrap();
        assert!(
            meta.file_type().is_file(),
            "F01-02: .env must end as a regular file, not a link"
        );
        assert_eq!(mode_of(&env), 0o600);
    }
}

/// **F01-05 — EXPECTED FAILURE until fixed.**
///
/// HANDOFF sprint-01 step 02 claims: "Init enforces canonical modes on
/// every run (static dirs 0755, artifacts 0644/0600)". Observed: directory
/// modes ARE re-normalized every run, but JSON artifact modes are only set
/// when the file is (re)written — a tampered 0666 `config.json` or 0600
/// `charter.json` survives re-init untouched.
/// (`crates/hamlet-cli/src/init.rs` `run()` — no chmod on the
/// `*_existed == true` branches.)
///
/// EXPECTED: re-init re-normalizes config.json/charter.json to 0644
/// (content untouched). ACTUAL: tampered modes persist.
#[test]
fn reinit_renormalizes_json_artifact_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let report = init::run(tmp.path()).unwrap();
    let root = report.village_root.clone();
    let config = root.join("config.json");
    let charter = root.join("charter.json");
    let config_bytes = fs::read(&config).unwrap();
    let charter_bytes = fs::read(&charter).unwrap();

    fs::set_permissions(&config, fs::Permissions::from_mode(0o666)).unwrap();
    fs::set_permissions(&charter, fs::Permissions::from_mode(0o600)).unwrap();

    init::run(tmp.path()).unwrap();
    assert_eq!(
        fs::read(&config).unwrap(),
        config_bytes,
        "content must be untouched"
    );
    assert_eq!(
        fs::read(&charter).unwrap(),
        charter_bytes,
        "content must be untouched"
    );
    assert_eq!(
        mode_of(&config),
        0o644,
        "F01-05: canonical artifact modes must be enforced on EVERY run"
    );
    assert_eq!(
        mode_of(&charter),
        0o644,
        "F01-05: canonical artifact modes must be enforced on EVERY run"
    );
}

/// **F01-06 — EXPECTED FAILURE until fixed.**
///
/// `.hamlet/.env` existing as a DIRECTORY is silently accepted
/// (`env: Existed`, the directory even gets chmod 0600 by `ensure`),
/// leaving a village with no usable secrets file and no diagnostic.
/// EXPECTED: typed refusal (`.env` must be a regular file).
/// ACTUAL: `init::run` succeeds; `has_content` in
/// `crates/hamlet-cli/src/init.rs` treats any metadata len>0 (a dir's is
/// 4096) as "exists with content".
#[test]
fn env_path_occupied_by_directory_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let hamlet = tmp.path().join(".hamlet");
    fs::create_dir_all(hamlet.join(".env")).unwrap();
    let result = init::run(tmp.path());
    assert!(
        result.is_err(),
        "F01-06: .env-as-directory must be refused; got {:?}",
        result.map(|r| r.env)
    );
}

// ---------------------------------------------------------------------------
// Fix-pass re-verification pins (QA re-run after 0c858ac / ADR-0006+0007).
// These pin the NEW bootstrap boundary so a future refactor cannot silently
// reopen F01-02/F01-03/F01-06 or the init side of F01-01.
// ---------------------------------------------------------------------------

/// ADR-0007 cl.2: a symlink planted at `charter.json` or `config.json` is a
/// typed refusal — init must never write, chmod, or template through it, and
/// the external target must be byte-identical afterwards.
#[test]
fn symlinked_json_artifacts_are_refused_target_untouched() {
    for artifact in ["charter.json", "config.json"] {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim.json");
        fs::write(&victim, "{\"not\":\"a hamlet artifact\"}").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        let hamlet = tmp.path().join(".hamlet");
        fs::create_dir_all(&hamlet).unwrap();
        std::os::unix::fs::symlink(&victim, hamlet.join(artifact)).unwrap();

        let result = init::run(tmp.path());
        assert!(
            result.is_err(),
            "symlinked {artifact} must be refused (ADR-0007)"
        );
        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "{\"not\":\"a hamlet artifact\"}",
            "{artifact}: external target was written through the symlink"
        );
        assert_eq!(
            mode_of(&victim),
            0o644,
            "{artifact}: target mode was changed through the symlink"
        );
    }
}

/// ADR-0007 cl.3: a village root that is not a REAL directory (symlink to an
/// outside dir, or dangling symlink) is refused — nothing is created inside
/// or through it.
#[test]
fn village_root_symlink_is_refused_outside_untouched() {
    // Symlink to an outside directory holding foreign content.
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside-dir");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("precious.txt"), "keep me").unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&outside, target.join(".hamlet")).unwrap();

    assert!(
        init::run(&target).is_err(),
        "symlinked village root must be refused"
    );
    let entries: Vec<_> = fs::read_dir(&outside)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from("precious.txt")],
        "init must not create ANYTHING inside the symlinked root"
    );

    // Dangling symlink root: refusal must not materialize the target dir.
    let tmp2 = tempfile::tempdir().unwrap();
    let ghost = tmp2.path().join("ghost-dir");
    std::os::unix::fs::symlink(&ghost, tmp2.path().join(".hamlet")).unwrap();
    assert!(
        init::run(tmp2.path()).is_err(),
        "dangling root symlink must be refused"
    );
    assert!(
        !ghost.exists(),
        "refusal must not create the symlink target"
    );
}

/// ADR-0007 consequence (documented, intentional): crash-litter temp files
/// inside the village root count as foreign content — `hamlet init` refuses
/// loudly until an operator cleans them, and the litter (crash evidence) is
/// never touched. Applies to both the ADR-0006 unique temp names and the
/// legacy deterministic name. The committed charter stays loadable
/// regardless (load ignores litter — pinned in adversarial_state.rs).
#[test]
fn crash_litter_temps_make_init_refuse_loudly() {
    use hamlet_core::charter::Charter;
    let tmp = tempfile::tempdir().unwrap();
    let report = init::run(tmp.path()).unwrap();
    let root = report.village_root.clone();

    for litter_name in [".charter.json.424242.7.tmp", ".charter.json.tmp"] {
        let litter = root.join(litter_name);
        fs::write(&litter, "{\"charter_version\": ").unwrap(); // torn crash remnant
        let err = init::run(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("foreign content"),
            "litter {litter_name} must be refused as foreign: {err}"
        );
        assert!(litter.exists(), "refusal must not destroy crash evidence");
        // The village itself is still sound underneath the litter.
        assert!(Charter::load(&root.join("charter.json")).is_ok());
        fs::remove_file(&litter).unwrap(); // operator cleanup
        assert!(
            init::run(tmp.path()).is_ok(),
            "init recovers once litter is cleaned"
        );
    }
}

/// ADR-0007 cl.1/2: ANY non-regular file at `.env` (here: a FIFO) is a
/// typed refusal — never adopted, never opened.
#[test]
fn env_fifo_is_refused() {
    use std::os::unix::fs::FileTypeExt;
    let tmp = tempfile::tempdir().unwrap();
    let hamlet = tmp.path().join(".hamlet");
    fs::create_dir_all(&hamlet).unwrap();
    let fifo = hamlet.join(".env");
    let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success(), "mkfifo must work for this test");

    let result = init::run(tmp.path());
    assert!(result.is_err(), "FIFO .env must be refused, not adopted");
    assert!(
        fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo(),
        "refusal must not touch the FIFO"
    );
}

/// ADR-0006 on the init side: two concurrent `hamlet init` runs over the
/// same fresh target must never produce torn artifacts — whichever init
/// wins (or both), every artifact is complete and parseable afterwards.
#[test]
fn concurrent_inits_never_tear_artifacts() {
    use hamlet_core::charter::Charter;
    use hamlet_core::config::Config;
    for round in 0..50 {
        let tmp = tempfile::tempdir().unwrap();
        let (p1, p2) = (tmp.path().to_path_buf(), tmp.path().to_path_buf());
        let h1 = std::thread::spawn(move || init::run(&p1).map(|_| ()));
        let h2 = std::thread::spawn(move || init::run(&p2).map(|_| ()));
        let (r1, r2) = (h1.join().unwrap(), h2.join().unwrap());
        let root = tmp.path().join(".hamlet");

        // Clean errors are acceptable; torn artifacts are not.
        let charter_bytes = fs::read(root.join("charter.json")).unwrap_or_else(|e| {
            panic!("round {round}: charter unreadable ({e}); r1={r1:?} r2={r2:?}")
        });
        assert!(
            Charter::from_bytes(&charter_bytes).is_ok(),
            "round {round}: TORN charter.json from concurrent init: {}",
            String::from_utf8_lossy(&charter_bytes)
        );
        let config_text = fs::read_to_string(root.join("config.json")).unwrap();
        assert!(
            Config::parse(&config_text).is_ok(),
            "round {round}: TORN config.json"
        );
        assert!(
            !fs::read(root.join(".env")).unwrap().is_empty(),
            "round {round}: TORN .env"
        );
        assert_eq!(mode_of(&root.join(".env")), 0o600);
    }
}
