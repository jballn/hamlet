//! Comprehensive CLI integration tests covering the complete SPEC §12.1 and ADR-0042 surface.
//!
//! Verifies:
//! - Daemon lifecycle: `daemon start`, `daemon status`, `daemon stop`, kill/restart resumability.
//! - Verbs: `mail {send|list|read}`, `decree {list|show}`, `metrics {show|export}`, `agent steer`, `completions`.
//! - Output formats: human formatted tables and strict `--json` outputs.
//! - Exit codes per ADR-0042: 0 (success), 1 (domain/operational error), 2 (usage), 3 (daemon unreachable).

use std::fs;
use std::process::Command;

use hamlet_cli::init;
use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{CommissionRecord, CommissionState};
use hamlet_core::decree::DecreeId;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;

mod support;
use support::start_test_daemon;

const BIN: &str = env!("CARGO_BIN_EXE_hamlet");

fn setup_test_village(work: &std::path::Path) -> HamletTree {
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

    let mut rec = CommissionRecord::draft(sanitize_name("testcomm").unwrap());
    rec.state = CommissionState::Active;
    rec.roster.insert(1001);
    charter.commissions.insert(1, rec);
    if let Some(s) = charter.slots.get_mut(&1001) {
        s.commission = Some(1);
    }
    charter.save_atomic(&tree.charter()).unwrap();

    tree
}

#[test]
fn test_daemon_lifecycle_and_resumability() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let tree = setup_test_village(work);

    // 1. Daemon status when not running -> exit code 3
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["daemon", "status"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("daemon is not running"));

    // 2. Daemon status --json when not running -> JSON error with code: 3
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "daemon", "status"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let json_err: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json_err["code"], 3);

    // 3. Start daemon in background
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["daemon", "start"])
        .output()
        .unwrap();
    assert!(out.status.success(), "start failed: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("hamlet daemon started"));

    // Verify discovery files exist
    assert!(tree.daemon_endpoint().is_file());
    assert!(tree.daemon_token().is_file());
    assert!(tree.daemon_pid().is_file());

    // 4. Daemon status (human)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["daemon", "status"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("Hamlet Daemon Status"));
    assert!(stdout.contains("Village Root:"));
    assert!(stdout.contains("Commissions:"));

    // 5. Daemon status (--json)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "daemon", "status"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(status_json["pid"].is_number());
    assert!(status_json["version"].is_string());
    assert_eq!(status_json["commissions_count"], 1);

    // 6. Send mail while running to test state persistence across restart
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "mail",
            "send",
            "1001",
            "Pre-restart Check",
            "Testing persistence",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "mail send failed: {out:?}");

    // 7. Stop daemon gracefully over local API
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["daemon", "stop"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stop failed: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("hamlet daemon stopped"));

    // Verify discovery files cleaned up
    assert!(!tree.daemon_endpoint().exists());
    assert!(!tree.daemon_token().exists());
    assert!(!tree.daemon_pid().exists());

    // 8. Restart daemon (resumability test)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["daemon", "start"])
        .output()
        .unwrap();
    assert!(out.status.success(), "restart failed: {out:?}");

    // Verify state persisted across daemon restart
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "mail", "list", "1001"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let mail_list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(mail_list.as_array().unwrap().len(), 1);
    assert_eq!(mail_list[0]["subject"], "Pre-restart Check");

    // Clean up daemon
    let _ = Command::new(BIN)
        .current_dir(work)
        .args(["daemon", "stop"])
        .output();
}

#[test]
fn test_mail_send_list_read_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_test_village(work);
    let _daemon = start_test_daemon(work);

    // 1. Initial empty mail list
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["mail", "list", "1001"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("(no mail for slot 1001)"));

    // 2. Send mail with custom --from and body
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "mail",
            "send",
            "1001",
            "Commission Brief Ready",
            "Please review the attached brief.",
            "--from",
            "mayor",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "mail send failed: {out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("delivered to slot 1001"));

    // 3. Send mail with --json output
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "--json",
            "mail",
            "send",
            "1001",
            "Urgent Notice",
            "Secondary notification text.",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let send_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(send_json["delivered"], true);

    // 4. List mail (human formatted table)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["mail", "list", "1001"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("FILE"));
    assert!(stdout.contains("FROM"));
    assert!(stdout.contains("TO"));
    assert!(stdout.contains("STATUS"));
    assert!(stdout.contains("SUBJECT"));
    assert!(stdout.contains("Commission Brief Ready"));
    assert!(stdout.contains("Urgent Notice"));

    // 5. List mail (--json)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "mail", "list", "1001"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let list_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let messages = list_json.as_array().unwrap();
    assert_eq!(messages.len(), 2);
    let first_file = messages[0]["file_name"].as_str().unwrap();

    // 6. Read specific mail message (human)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["mail", "read", "1001", first_file])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("Subject:"));
    assert!(stdout.contains("From:"));
    assert!(stdout.contains("Status:"));

    // 7. Read specific mail message (--json)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "mail", "read", "1001", first_file])
        .output()
        .unwrap();
    assert!(out.status.success());
    let detail_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(detail_json["summary"]["file_name"], first_file);
    assert!(detail_json["body"].is_string());
}

#[test]
fn test_decrees_list_and_show() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let tree = setup_test_village(work);

    // Seed a decree into keep/census/manor/decrees/
    let decrees_dir = tree.manor_decrees();
    fs::create_dir_all(&decrees_dir).unwrap();
    let decree_id = DecreeId::new(1, "0123456789abcdef").unwrap();
    let decree_payload = serde_json::json!({
        "id": decree_id.to_string(),
        "action": "reprovision",
        "target": 1001,
        "reason": "routine upgrade"
    });
    fs::write(
        tree.manor_decree_file(&decree_id),
        serde_json::to_string_pretty(&decree_payload).unwrap(),
    )
    .unwrap();

    let _daemon = start_test_daemon(work);

    // 1. List decrees (human)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["decree", "list"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("DECREE_ID"));
    assert!(stdout.contains("SEQ"));
    assert!(stdout.contains("ACTION"));
    assert!(stdout.contains(&decree_id.to_string()));
    assert!(stdout.contains("reprovision"));

    // 2. List decrees (--json)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "decree", "list"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let decrees_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(decrees_json.as_array().unwrap().len(), 1);
    assert_eq!(decrees_json[0]["decree_id"], decree_id.to_string());
    assert_eq!(decrees_json[0]["action"], "reprovision");

    // 3. Show decree (human)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["decree", "show", &decree_id.to_string()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains(&format!("decree: {}", decree_id)));
    assert!(stdout.contains("action:  reprovision"));
    assert!(stdout.contains("--- Payload JSON ---"));
    assert!(stdout.contains("routine upgrade"));

    // 4. Show decree (--json)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "decree", "show", &decree_id.to_string()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let show_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(show_json["summary"]["decree_id"], decree_id.to_string());
    assert!(show_json["raw_json"]
        .as_str()
        .unwrap()
        .contains("routine upgrade"));

    // 5. Show non-existent decree -> exit code 1
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["decree", "show", "d-000000000999-0123456789abcdef"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn test_metrics_show_and_export() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_test_village(work);
    let _daemon = start_test_daemon(work);

    // 1. Metrics show (human)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["metrics", "show"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("Hamlet Village Metrics"));
    assert!(stdout.contains("Total Input Tokens:"));
    assert!(stdout.contains("Total Cost:"));

    // 2. Metrics show (--json)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "metrics"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let metrics_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(metrics_json["total_input_tokens"].is_number());
    assert!(metrics_json["total_cost_micro_usd"].is_number());

    // 3. Metrics export to file
    let export_path = work.join("metrics_export.json");
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["metrics", "export", export_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(export_path.is_file());
    let exported_content = fs::read_to_string(&export_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&exported_content).unwrap();
    assert!(parsed["total_input_tokens"].is_number());

    // 4. Metrics export with --json output
    let export_path2 = work.join("metrics_export2.json");
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "--json",
            "metrics",
            "export",
            export_path2.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(export_path2.is_file());
    let out_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out_json["exported"], export_path2.to_str().unwrap());
}

#[test]
fn test_agent_steer_and_failure_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_test_village(work);
    let _daemon = start_test_daemon(work);

    // 1. Steer inactive agent -> operational/domain error (exit code 1)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "steer", "1001", "halt current task"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("steer injection failed"));

    // 2. Steer with --json on failure -> JSON error object with code: 1
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "agent", "steer", "1001", "halt current task"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let json_err: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json_err["code"], 1);
    assert!(json_err["error"]
        .as_str()
        .unwrap()
        .contains("steer injection failed"));
}

#[test]
fn test_shell_completions() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();

    // 1. Bash completions
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["completions", "bash"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("# bash completion for hamlet"));
    assert!(stdout.contains("commission"));
    assert!(stdout.contains("vault"));

    // 2. Zsh completions
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("#compdef hamlet"));

    // 3. Fish completions
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["completions", "fish"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("complete -c hamlet"));

    // 4. Unsupported shell -> exit code 2 (usage error)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["completions", "powershell"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn test_exit_code_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();

    // 1. Exit code 2: Usage / clap parse errors
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["nonexistent-subcommand"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "show"]) // missing UID
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    let out = Command::new(BIN)
        .current_dir(work)
        .args(["mail", "send"]) // missing args
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    // 2. Exit code 3: Daemon unavailable / connection failure
    init::run(work).unwrap();
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));

    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));

    let out = Command::new(BIN)
        .current_dir(work)
        .args(["vault", "list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));

    // 3. Exit code 1: Operational / domain error with running daemon
    let _daemon = start_test_daemon(work);

    // Invalid commission slug (fails sanitizer)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "show", "../illegal"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    // Invalid patron name (fails sanitizer)
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "wake", "-illegal-patron"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    // Non-existent commission close
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "close", "ghost-comm"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}
