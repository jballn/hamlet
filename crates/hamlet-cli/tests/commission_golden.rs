//! Golden and adversarial CLI tests for `hamlet commission {new|list|show}`
//! (Sprint 06 Step 02; SPEC §9.1, §12.1; CONVENTIONS §9).
//!
//! All tests run in tempdirs with no podman:
//! - byte-exact golden comparisons for `commission list` (empty & populated),
//!   `commission new` (with `--brief <file>` and `--brief - --seed <dir>`),
//!   and `commission show <slug>`;
//! - slug sanitization rejections (`../escape`, `-leading-hyphen`, empty) leave
//!   the village untouched and exit `1`;
//! - usage errors (missing `--brief`, missing `<slug>`, unknown subcommand)
//!   exit `2`;
//! - `list` and `show` read strictly from `charter.json` + `.hamlet/` tree.

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};

use hamlet_cli::init;

mod support;
use support::start_test_daemon;

const BIN: &str = env!("CARGO_BIN_EXE_hamlet");

#[test]
fn cli_commission_new_list_show_match_golden_outputs() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    init::run(work).expect("hamlet init must succeed");
    let _daemon = start_test_daemon(work);

    // 1. Empty `hamlet commission list` matches golden.
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "list"])
        .output()
        .unwrap();
    assert!(out.status.success(), "list failed: {out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        include_str!("golden/commission-list-empty.txt")
    );

    // 2. `hamlet commission new alpha --brief <file>` matches golden.
    let brief_alpha = work.join("brief-alpha.md");
    fs::write(
        &brief_alpha,
        "# Commission 1 — Alpha\n\nBuild the northern watchtower.\n",
    )
    .unwrap();
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "commission",
            "new",
            "alpha",
            "--brief",
            brief_alpha.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "commission new alpha failed: {out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        include_str!("golden/commission-new-alpha.txt")
    );

    // 3. `hamlet commission new beta --brief - --seed <dir>` matches golden.
    let seed_dir = work.join("seed-src");
    fs::create_dir_all(seed_dir.join("src")).unwrap();
    fs::write(seed_dir.join("README.md"), "# Beta Seed\n").unwrap();
    fs::write(seed_dir.join("src/main.rs"), "fn main() {}\n").unwrap();

    let mut child = Command::new(BIN)
        .current_dir(work)
        .args([
            "commission",
            "new",
            "beta",
            "--brief",
            "-",
            "--seed",
            seed_dir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"# Commission 2 \xe2\x80\x94 Beta\n\nExtend the seeded repository.\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "commission new beta failed: {out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        include_str!("golden/commission-new-beta-seeded.txt")
    );

    // 4. `hamlet commission list` with alpha and beta matches golden.
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "list"])
        .output()
        .unwrap();
    assert!(out.status.success(), "populated list failed: {out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        include_str!("golden/commission-list-populated.txt")
    );

    // 5. `hamlet commission show alpha` matches golden.
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "show", "alpha"])
        .output()
        .unwrap();
    assert!(out.status.success(), "show alpha failed: {out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        include_str!("golden/commission-show-alpha.txt")
    );

    // 6. `hamlet commission show beta` matches golden.
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "show", "beta"])
        .output()
        .unwrap();
    assert!(out.status.success(), "show beta failed: {out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        include_str!("golden/commission-show-beta.txt")
    );
}

#[test]
fn cli_commission_rejects_bad_slugs_symlinks_and_usage_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    init::run(work).unwrap();
    let _daemon = start_test_daemon(work);

    let brief = work.join("brief.md");
    fs::write(&brief, "# Valid brief\n").unwrap();
    let charter_before = fs::read_to_string(work.join(".hamlet/charter.json")).unwrap();

    // Hostile slugs exit 1 and leave charter/tree untouched.
    for bad_slug in ["../escape", "-leading", "bad/slash", "has space"] {
        let out = Command::new(BIN)
            .current_dir(work)
            .args([
                "commission",
                "new",
                bad_slug,
                "--brief",
                brief.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "bad slug {bad_slug:?} must exit 1: {out:?}"
        );
    }
    assert!(!work.join(".hamlet/guild/ledger/commission1").exists());
    assert_eq!(
        fs::read_to_string(work.join(".hamlet/charter.json")).unwrap(),
        charter_before
    );

    // Symlink brief file is refused (ADR-0007) with exit code 1.
    let sym_brief = work.join("sym-brief.md");
    symlink(&brief, &sym_brief).unwrap();
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "commission",
            "new",
            "alpha",
            "--brief",
            sym_brief.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!work.join(".hamlet/guild/ledger/commission1").exists());

    // Usage errors exit 2.
    for bad_args in [
        &["commission"][..],
        &["commission", "bogus"][..],
        &["commission", "new"][..],
        &["commission", "new", "alpha"][..],
        &["commission", "show"][..],
        &["commission", "list", "extra"][..],
    ] {
        let out = Command::new(BIN)
            .current_dir(work)
            .args(bad_args)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "usage error {bad_args:?} must exit 2: {out:?}"
        );
    }
}

#[test]
fn cli_commission_show_renders_indenture_entry_statuses() {
    use hamlet_core::charter::Charter;
    use hamlet_core::commission::{CommissionEvent, IndentureEntryStatus};
    use hamlet_core::sanitize::sanitize_name;
    use std::collections::BTreeSet;

    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    init::run(work).unwrap();
    let _daemon = start_test_daemon(work);

    let brief = work.join("brief.md");
    fs::write(
        &brief,
        "# Commission 1 — Alpha\n\nBuild the northern watchtower.\n",
    )
    .unwrap();
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "commission",
            "new",
            "alpha",
            "--brief",
            brief.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Roster ada (1001) and grace (1002), write INDENTURE.md, and advance entries.
    let charter_path = work.join(".hamlet/charter.json");
    let mut charter = Charter::load(&charter_path).unwrap();
    let uid_ada = charter
        .allocate_slot(sanitize_name("ada").unwrap(), Some(1))
        .unwrap();
    let uid_grace = charter
        .allocate_slot(sanitize_name("grace").unwrap(), Some(1))
        .unwrap();
    charter.assign_uid(1, uid_ada).unwrap();
    charter.assign_uid(1, uid_grace).unwrap();
    charter
        .apply_commission_event(
            1,
            &CommissionEvent::IndenturePosted {
                entries: BTreeSet::from([uid_ada, uid_grace]),
            },
        )
        .unwrap();
    charter
        .mark_indenture_entry(1, uid_ada, IndentureEntryStatus::Delivered)
        .unwrap();
    charter
        .mark_indenture_entry(1, uid_grace, IndentureEntryStatus::Done)
        .unwrap();
    charter.save_atomic(&charter_path).unwrap();

    fs::write(
        work.join(".hamlet/guild/ledger/commission1/INDENTURE.md"),
        "# INDENTURE\n\n## ada\n- goal: g1\n- scope: s1\n- done: d1\n\n## grace\n- goal: g2\n- scope: s2\n- done: d2\n",
    )
    .unwrap();

    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "show", "alpha"])
        .output()
        .unwrap();
    assert!(out.status.success(), "show alpha failed: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("  state:      active\n"),
        "expected active state in show output:\n{stdout}"
    );
    assert!(
        stdout.contains("  indenture:  guild/ledger/commission1/INDENTURE.md (present)\n"),
        "expected present INDENTURE.md in show output:\n{stdout}"
    );
    assert!(
        stdout.contains("  entries:    1001 (ada): delivered, 1002 (grace): done\n"),
        "expected indenture entry statuses in show output:\n{stdout}"
    );
}
