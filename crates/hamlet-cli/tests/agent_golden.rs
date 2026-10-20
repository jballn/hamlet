//! CLI tests for `hamlet agent list` (Sprint 07 Step 02; SPEC §10, §12.1).

use std::fs;
use std::process::Command;

use hamlet_cli::init;
use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{CommissionRecord, CommissionState};
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;

mod support;
use support::start_test_daemon;

const BIN: &str = env!("CARGO_BIN_EXE_hamlet");

fn setup_village(work: &std::path::Path) -> HamletTree {
    init::run(work).expect("hamlet init must succeed");
    let tree = HamletTree::new(work.join(".hamlet"));

    // Seed Manor TITLE.md
    let manor_title = tree.manor_title();
    fs::create_dir_all(manor_title.parent().unwrap()).unwrap();
    fs::write(
        &manor_title,
        "# TITLE — manor\n\n- [2026-10-19T01:00:00Z] provision uid=1000 archetype=manor blueprint=manor version=1\n",
    )
    .unwrap();

    let mut charter = Charter::load(&tree.charter()).unwrap();
    let slot = charter
        .allocate_slot(sanitize_name("worker1001").unwrap(), None)
        .unwrap();
    assert_eq!(slot, 1001);
    charter.parcels.insert(
        1001,
        ParcelLease {
            identity: sanitize_name("worker1001").unwrap(),
            stasis: false,
        },
    );

    let mut rec = CommissionRecord::draft(sanitize_name("alpha").unwrap());
    rec.state = CommissionState::Active;
    rec.roster.insert(1001);
    charter.commissions.insert(1, rec);
    if let Some(s) = charter.slots.get_mut(&1001) {
        s.commission = Some(1);
    }

    charter.save_atomic(&tree.charter()).unwrap();

    // Seed parcel 1001 TITLE.md
    let parcel_title = tree.parcel_title(1001);
    fs::create_dir_all(parcel_title.parent().unwrap()).unwrap();
    fs::write(
        &parcel_title,
        "# TITLE — parcel1001\n\n\
         - [2026-10-19T01:05:00Z] trigger commission=1 prompt=\"test commission prompt\"\n\
         - [2026-10-19T01:05:00Z] provision uid=1001 archetype=worker1001 blueprint=default version=1\n\
         - [2026-10-19T01:06:00Z] session session_id=1001.0 incarnation=1\n",
    )
    .unwrap();

    tree
}

#[test]
fn cli_agent_list_shows_manor_and_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_village(work);
    let _daemon = start_test_daemon(work);

    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "list"])
        .output()
        .expect("hamlet agent list failed");

    assert!(out.status.success(), "status: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("UID"));
    assert!(stdout.contains("IDENTITY"));
    assert!(stdout.contains("COMMISSION"));
    assert!(stdout.contains("STATE"));
    assert!(stdout.contains("1000"));
    assert!(stdout.contains("manor"));
    assert!(stdout.contains("exempt"));
    assert!(stdout.contains("1001"));
    assert!(stdout.contains("worker1001"));
    assert!(stdout.contains("alpha"));
    assert!(stdout.contains("session"));
}

#[test]
fn cli_agent_list_filters_by_commission() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_village(work);
    let _daemon = start_test_daemon(work);

    // 1. Filter by matching commission
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "list", "--commission", "alpha"])
        .output()
        .expect("hamlet agent list --commission alpha failed");

    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("1001"));
    assert!(stdout.contains("worker1001"));
    assert!(stdout.contains("alpha"));
    assert!(
        !stdout.contains("manor"),
        "manor is not rostered on commission alpha"
    );

    // 2. Filter by non-matching commission
    let out_non_match = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "list", "--commission", "nonexistent"])
        .output()
        .expect("hamlet agent list --commission nonexistent failed");

    assert!(out_non_match.status.success());
    let stdout_non_match = String::from_utf8(out_non_match.stdout).unwrap();
    assert!(stdout_non_match.contains("(no agents)"));
}

#[test]
fn cli_agent_list_with_dir_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let other_tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_village(work);
    let _daemon = start_test_daemon(work);

    let out = Command::new(BIN)
        .current_dir(other_tmp.path())
        .args(["agent", "list", "--dir", &work.to_string_lossy()])
        .output()
        .expect("hamlet agent list --dir failed");

    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("1000"));
    assert!(stdout.contains("1001"));
}

#[test]
fn cli_agent_usage_errors() {
    let tmp = tempfile::tempdir().unwrap();

    // 1. Missing subcommand
    let out = Command::new(BIN)
        .current_dir(tmp.path())
        .args(["agent"])
        .output()
        .expect("run hamlet agent");
    assert_eq!(out.status.code(), Some(2));

    // 2. Unknown subcommand
    let out = Command::new(BIN)
        .current_dir(tmp.path())
        .args(["agent", "unknown"])
        .output()
        .expect("run hamlet agent unknown");
    assert_eq!(out.status.code(), Some(2));

    // 3. Unexpected argument
    let out = Command::new(BIN)
        .current_dir(tmp.path())
        .args(["agent", "list", "extra"])
        .output()
        .expect("run hamlet agent list extra");
    assert_eq!(out.status.code(), Some(2));

    // 4. Missing hibernate UID
    let out = Command::new(BIN)
        .current_dir(tmp.path())
        .args(["agent", "hibernate"])
        .output()
        .expect("run hamlet agent hibernate");
    assert_eq!(out.status.code(), Some(2));

    // 5. Invalid hibernate UID
    let out = Command::new(BIN)
        .current_dir(tmp.path())
        .args(["agent", "hibernate", "not-a-number"])
        .output()
        .expect("run hamlet agent hibernate invalid");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn cli_agent_hibernate_rejects_manor_and_unknown_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_village(work);
    let _daemon = start_test_daemon(work);

    // 1. Manor (UID 1000) rejection
    let out_manor = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "hibernate", "1000"])
        .output()
        .expect("run hamlet agent hibernate 1000");
    assert!(!out_manor.status.success());
    let stderr = String::from_utf8(out_manor.stderr).unwrap();
    assert!(
        stderr.contains("uid 1000 is exempt from hibernation"),
        "stderr was: {stderr}"
    );

    // 2. Unknown slot rejection (e.g. 1099)
    let out_unknown = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "hibernate", "1099"])
        .output()
        .expect("run hamlet agent hibernate 1099");
    assert!(!out_unknown.status.success());
    let stderr_unknown = String::from_utf8(out_unknown.stderr).unwrap();
    assert!(
        stderr_unknown.contains("unknown slot uid 1099"),
        "stderr was: {stderr_unknown}"
    );

    // 3. Village not found (daemon not running) -> exit code 3
    let other_tmp = tempfile::tempdir().unwrap();
    let out_no_village = Command::new(BIN)
        .current_dir(other_tmp.path())
        .args(["agent", "hibernate", "1001"])
        .output()
        .expect("run hamlet agent hibernate 1001 in empty dir");
    assert_eq!(out_no_village.status.code(), Some(3));
}

#[test]
fn cli_agent_wake_usage_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_village(work);

    // 1. Missing patron name
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "wake"])
        .output()
        .expect("run hamlet agent wake");
    assert_eq!(out.status.code(), Some(2));

    // 2. Missing slug for --commission
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "wake", "mypatron", "--commission"])
        .output()
        .expect("run hamlet agent wake missing commission slug");
    assert_eq!(out.status.code(), Some(2));

    // 3. Missing dir for --dir
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "wake", "mypatron", "--dir"])
        .output()
        .expect("run hamlet agent wake missing dir");
    assert_eq!(out.status.code(), Some(2));

    // 4. Unexpected argument
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "wake", "mypatron", "--unknown"])
        .output()
        .expect("run hamlet agent wake unexpected arg");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn cli_agent_wake_unknown_patron_and_village_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_village(work);
    let _daemon = start_test_daemon(work);

    // 1. Unknown patron error
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "wake", "ghost_patron"])
        .output()
        .expect("run hamlet agent wake ghost_patron");
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("no such patron 'ghost_patron'"),
        "stderr was: {stderr}"
    );

    // 2. Village not found (daemon not running) -> exit code 3
    let empty_tmp = tempfile::tempdir().unwrap();
    let out_no_village = Command::new(BIN)
        .current_dir(empty_tmp.path())
        .args(["agent", "wake", "ghost_patron"])
        .output()
        .expect("run hamlet agent wake in empty dir");
    assert_eq!(out_no_village.status.code(), Some(3));
}
