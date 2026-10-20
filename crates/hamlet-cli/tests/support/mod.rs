//! Test harness utilities for hamlet-cli integration tests.

use std::path::Path;
use std::process::Command;

pub const BIN: &str = env!("CARGO_BIN_EXE_hamlet");

pub struct DaemonGuard<'a>(pub &'a Path);

impl<'a> Drop for DaemonGuard<'a> {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .current_dir(self.0)
            .args(["daemon", "stop"])
            .output();
    }
}

pub fn start_test_daemon(work: &Path) -> DaemonGuard<'_> {
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["daemon", "start"])
        .output()
        .expect("daemon start failed");
    assert!(out.status.success(), "daemon start failed: {out:?}");
    DaemonGuard(work)
}
