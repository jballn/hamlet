//! Golden-file tests for `hamlet init` (CONVENTIONS §9: committed golden
//! vectors). The expected listing/contents under `golden/` are the contract:
//! any change to the normative tree shape, canonical modes, default
//! `config.json`, the empty `charter.json` skeleton, or the `.env` template
//! must be a deliberate diff here.
//!
//! Unix-only: modes (0755/0644/0600) are part of the golden contract.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use hamlet_cli::init::{self, InitState};

/// Full recursive listing of `root`: `{mode:04o} {dir|file} {relpath}`,
/// sorted by relpath (root itself listed as `.`).
fn tree_listing(root: &Path) -> String {
    let mut entries: Vec<(String, String)> = Vec::new(); // (relpath, line)
    collect(root, root, &mut entries);
    entries.sort();
    entries
        .into_iter()
        .map(|(_, line)| line + "\n")
        .collect::<String>()
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let rel = dir.strip_prefix(root).expect("walk stays under root");
    let rel = if rel.as_os_str().is_empty() {
        ".".to_string()
    } else {
        rel.to_str().expect("utf-8 tree").to_string()
    };
    let meta = fs::metadata(dir).unwrap();
    out.push((
        rel.clone(),
        format!("{:04o} dir {}", meta.permissions().mode() & 0o777, rel),
    ));
    let mut children: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    children.sort();
    for child in children {
        let name = child.file_name().unwrap().to_str().unwrap();
        let child_rel = if rel == "." {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        let meta = fs::metadata(&child).unwrap();
        if meta.is_dir() {
            collect(root, &child, out);
        } else {
            out.push((
                child_rel.clone(),
                format!(
                    "{:04o} file {}",
                    meta.permissions().mode() & 0o777,
                    child_rel
                ),
            ));
        }
    }
}

#[test]
fn init_matches_golden_tree_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let report = init::run(tmp.path()).unwrap();
    assert_eq!(report.tree, InitState::Created);
    assert_eq!(report.config, InitState::Created);
    assert_eq!(report.charter, InitState::Created);
    assert_eq!(report.env, InitState::Created);
    assert_eq!(
        tree_listing(&report.village_root),
        include_str!("golden/init-tree.txt"),
        "tree shape/modes drifted from the committed golden listing"
    );
}

#[test]
fn init_writes_golden_file_contents() {
    let tmp = tempfile::tempdir().unwrap();
    let report = init::run(tmp.path()).unwrap();
    let root = &report.village_root;
    assert_eq!(
        fs::read_to_string(root.join("config.json")).unwrap(),
        include_str!("golden/config.json")
    );
    assert_eq!(
        fs::read_to_string(root.join("charter.json")).unwrap(),
        include_str!("golden/charter.json")
    );
    assert_eq!(
        fs::read_to_string(root.join(".env")).unwrap(),
        include_str!("golden/env-template")
    );
}

#[test]
fn env_file_is_0600() {
    let tmp = tempfile::tempdir().unwrap();
    let report = init::run(tmp.path()).unwrap();
    let mode = fs::metadata(report.village_root.join(".env"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, ".env must be 0600 (SPEC §11.4)");
}

#[test]
fn init_is_idempotent_and_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let first = init::run(tmp.path()).unwrap();
    let listing = tree_listing(&first.village_root);
    let config = fs::read(first.village_root.join("config.json")).unwrap();
    let charter = fs::read(first.village_root.join("charter.json")).unwrap();
    let env = fs::read(first.village_root.join(".env")).unwrap();

    let second = init::run(tmp.path()).unwrap();
    assert_eq!(second.tree, InitState::Existed);
    assert_eq!(second.config, InitState::Existed);
    assert_eq!(second.charter, InitState::Existed);
    assert_eq!(second.env, InitState::Existed);
    // Byte-identical artifacts, identical listing (modes included).
    assert_eq!(
        fs::read(second.village_root.join("config.json")).unwrap(),
        config
    );
    assert_eq!(
        fs::read(second.village_root.join("charter.json")).unwrap(),
        charter
    );
    assert_eq!(fs::read(second.village_root.join(".env")).unwrap(), env);
    assert_eq!(tree_listing(&second.village_root), listing);
    assert_eq!(listing, include_str!("golden/init-tree.txt"));
}

/// End-to-end through the real binary: `hamlet init <dir>` reports created,
/// re-run reports exists; bad usage exits 2.
#[test]
fn cli_init_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_hamlet");

    let out = Command::new(bin)
        .args(["init", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "init failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("created"), "{stdout}");
    assert!(stdout.contains(".hamlet"), "{stdout}");

    let out = Command::new(bin)
        .args(["init", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("exists — untouched"), "{stdout}");

    let out = Command::new(bin).arg("bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));

    let out = Command::new(bin).arg("init").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// `hamlet init` works when the target working directory does not exist yet.
#[test]
fn init_creates_missing_target_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("new-village");
    let report = init::run(&target).unwrap();
    assert_eq!(report.village_root, target.join(".hamlet"));
    assert!(report.village_root.join("keep/census/manor/home").is_dir());
}
