//! CLI tests for `hamlet vault {list|export}` and `hamlet commission close`
//! (Sprint 06 Step 06; SPEC §7, §9.4, §12.1; CONVENTIONS §9).
//!
//! Tests:
//! - `hamlet vault list` (empty & populated);
//! - `hamlet vault export <artifact> <dest> [--commission <slug>]`;
//! - `hamlet commission close <slug>`;
//! - Usage errors (exit code 2);
//! - Symlink refusal under ADR-0007 (exit code 1);
//! - Ambiguous artifact refusal across commissions (exit code 1).

use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;

use hamlet_cli::init;
use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{CommissionRecord, CommissionState, IndentureEntryStatus};
use hamlet_core::decree::content_digest;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;

mod support;
use support::start_test_daemon;

const BIN: &str = env!("CARGO_BIN_EXE_hamlet");

fn setup_village(work: &std::path::Path) -> HamletTree {
    init::run(work).expect("hamlet init must succeed");
    let tree = HamletTree::new(work.join(".hamlet"));

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
    rec.state = CommissionState::WindingDown;
    rec.roster.insert(1001);
    rec.indenture_entries
        .insert(1001, IndentureEntryStatus::Done);
    charter.commissions.insert(1, rec);
    if let Some(s) = charter.slots.get_mut(&1001) {
        s.commission = Some(1);
    }

    charter.save_atomic(&tree.charter()).unwrap();
    tree
}

#[test]
fn cli_vault_list_and_export_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let tree = setup_village(work);
    let _daemon = start_test_daemon(work);

    // 1. Empty `hamlet vault list`
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["vault", "list"])
        .output()
        .unwrap();
    assert!(out.status.success(), "vault list failed: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("(no vault artifacts)"));

    // 2. Populate an artifact into keep/vault/alpha/
    let vault_alpha = tree.keep_vault_commission(&sanitize_name("alpha").unwrap());
    fs::create_dir_all(&vault_alpha).unwrap();
    let artifact_data = b"ALPHA_PROMOTED_PAYLOAD_V1";
    let artifact_path = vault_alpha.join("bundle.tar");
    fs::write(&artifact_path, artifact_data).unwrap();
    let digest = content_digest(artifact_data);
    fs::write(
        vault_alpha.join("bundle.tar.sha256"),
        format!("{digest}  bundle.tar\n"),
    )
    .unwrap();

    // 3. `hamlet vault list` shows artifact
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["vault", "list"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("alpha"));
    assert!(stdout.contains("bundle.tar"));
    assert!(stdout.contains(&artifact_data.len().to_string()));
    assert!(stdout.contains(&digest));

    // 4. `hamlet vault list --commission alpha` matches
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["vault", "list", "--commission", "alpha"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("alpha"));
    assert!(stdout.contains("bundle.tar"));

    // 5. `hamlet vault export bundle.tar <dest>`
    let export_dest = work.join("exported_bundle.tar");
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "vault",
            "export",
            "bundle.tar",
            export_dest.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "export failed: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("exported bundle.tar from commission 'alpha'"));

    // Verify exported file is byte-identical
    assert!(export_dest.is_file());
    assert_eq!(fs::read(&export_dest).unwrap(), artifact_data);

    // 6. `hamlet commission close alpha`
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "close", "alpha"])
        .output()
        .unwrap();
    assert!(out.status.success(), "commission close failed: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("commission1 (alpha)"));
    assert!(stdout.contains("state:"));
    assert!(stdout.contains("closed"));

    // Verify charter state is Closed
    let charter = Charter::load(&tree.charter()).unwrap();
    let comm = charter.commissions.get(&1).unwrap();
    assert_eq!(comm.state, CommissionState::Closed);
}

#[test]
fn cli_vault_security_symlinks_and_ambiguity_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let tree = setup_village(work);
    let _daemon = start_test_daemon(work);

    // Populate commission 1 alpha with artifact "result.bin"
    let vault_alpha = tree.keep_vault_commission(&sanitize_name("alpha").unwrap());
    fs::create_dir_all(&vault_alpha).unwrap();
    fs::write(vault_alpha.join("result.bin"), b"ALPHA").unwrap();

    // Populate commission 2 beta with artifact "result.bin"
    let vault_beta = tree.keep_vault_commission(&sanitize_name("beta").unwrap());
    fs::create_dir_all(&vault_beta).unwrap();
    fs::write(vault_beta.join("result.bin"), b"BETA").unwrap();

    // 1. Ambiguous export without --commission -> exits 1 with error
    let dest_file = work.join("out.bin");
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["vault", "export", "result.bin", dest_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("ambiguous"));
    assert!(stderr.contains("--commission <slug> to disambiguate"));

    // 2. Disambiguated with --commission beta -> succeeds
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "vault",
            "export",
            "result.bin",
            dest_file.to_str().unwrap(),
            "--commission",
            "beta",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(fs::read(&dest_file).unwrap(), b"BETA");

    // 3. Symlink destination refused under ADR-0007
    let real_outside = work.join("outside.txt");
    fs::write(&real_outside, b"ORIGINAL").unwrap();
    let symlink_dest = work.join("symlink_target.bin");
    symlink(&real_outside, &symlink_dest).unwrap();

    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "vault",
            "export",
            "result.bin",
            symlink_dest.to_str().unwrap(),
            "--commission",
            "alpha",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("refusing symlink"));
    assert_eq!(fs::read(&real_outside).unwrap(), b"ORIGINAL");
}

#[test]
fn cli_vault_usage_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    init::run(work).unwrap();

    // Missing subcommand -> exit code 2
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["vault"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    // Unknown subcommand -> exit code 2
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["vault", "frobnicate"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    // Export missing arguments -> exit code 2
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["vault", "export"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    // Commission close missing slug -> exit code 2
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "close"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
