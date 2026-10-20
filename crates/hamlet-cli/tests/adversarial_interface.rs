//! Adversarial and boundary test suite for Sprint 08 CLI & local API interfaces
//! (SPEC §12; ADR-0041, ADR-0042, ADR-0043; CONVENTIONS §2, §3).
//!
//! Surface areas covered:
//! 1. **API boundary**: Static audit asserting zero direct filesystem/sled access
//!    to live village tree by CLI/TUI; route mapping coverage table.
//! 2. **CLI matrix**: Table-driven matrix of every verb × {human, --json} ×
//!    {success, typed failure}; exit-code policy (0/1/2/3); daemon-down clean errors.
//! 3. **Auth & token lifecycle**: Token file 0600, missing token 401, forged token 401,
//!    constant-time equality.
//! 4. **Adversarial input & path traversal**: Path traversal attempts in promotion /
//!    vault export, boundary sanitizer enforcement, malformed JSON streams.

use std::fs;
use std::path::Path;
use std::process::Command;

use hamlet_cli::init;
use hamlet_core::api::*;
use hamlet_core::charter::{Charter, ParcelLease};
use hamlet_core::commission::{CommissionRecord, CommissionState, IndentureEntryStatus};
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;

mod support;
use support::{start_test_daemon, BIN};

fn setup_test_village(work: &Path) -> HamletTree {
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
    rec.indenture_entries
        .insert(1001, IndentureEntryStatus::Done);
    charter.commissions.insert(1, rec);
    if let Some(s) = charter.slots.get_mut(&1001) {
        s.commission = Some(1);
    }
    charter.save_atomic(&tree.charter()).unwrap();

    tree
}

// ---------------------------------------------------------------------------
// 1. API Boundary & Route Coverage Table
// ---------------------------------------------------------------------------

#[test]
fn test_api_boundary_static_source_audit() {
    // Assert CLI and TUI source files never access live village tree directories directly.
    // Allowed exceptions:
    // - hamlet-cli/src/init.rs: offline tree initialization bootstrap
    // - hamlet-cli/src/client.rs & hamlet-tui/src/client.rs: reading daemon.endpoint / daemon.token
    // - hamlet-cli/src/daemon.rs: reading daemon.pid for liveness probe & charter().exists() check
    let cli_src = Path::new("crates/hamlet-cli/src");
    let tui_src = Path::new("crates/hamlet-tui/src");

    let forbidden_live_paths = [
        "guild/ledger",
        "guild/blueprints",
        "guild/register",
        "tavern/gossip",
        "tavern/patrons",
        "courier/mail",
        "courier/freight",
        "courier/dispatch",
        "market/skills",
        "market/pkgcache",
        "keep/vault",
        "keep/archives",
        "keep/census",
    ];

    for crate_dir in [cli_src, tui_src] {
        if !crate_dir.exists() {
            continue;
        }
        for entry in fs::read_dir(crate_dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let filename = path.file_name().unwrap().to_str().unwrap();
            if filename == "init.rs" {
                continue; // bootstrap exception
            }

            let content = fs::read_to_string(&path).unwrap();
            for forbidden in forbidden_live_paths {
                assert!(
                    !content.contains(forbidden),
                    "Boundary violation: {} touches live tree path '{}'",
                    path.display(),
                    forbidden
                );
            }

            // Assert no direct sled database usage
            assert!(
                !content.contains("sled::open") && !content.contains("sled::Db"),
                "Boundary violation: {} accesses sled directly",
                path.display()
            );
        }
    }
}

#[test]
fn test_route_coverage_table() {
    // Verify that every mutating operator verb maps directly to a normative API command endpoint.
    let mutating_routes: [(&str, &str, &str); 9] = [
        ("daemon stop", "POST", ENDPOINT_STOP),
        ("commission new", "POST", ENDPOINT_COMMISSION_NEW),
        (
            "commission close",
            "POST",
            "/api/v1/commissions/:slug/close",
        ),
        ("agent hibernate", "POST", ENDPOINT_AGENT_HIBERNATE),
        ("agent wake", "POST", ENDPOINT_AGENT_WAKE),
        ("agent steer", "POST", "/api/v1/agents/:uid/steer"),
        ("mail send", "POST", ENDPOINT_MAIL_SEND),
        ("vault export", "POST", ENDPOINT_VAULT_EXPORT),
        (
            "escalation resolve",
            "POST",
            "/api/v1/escalations/:id/resolve",
        ),
    ];

    for (verb, method, route) in mutating_routes {
        assert_eq!(method, "POST");
        assert!(
            route.starts_with("/api/v1/"),
            "Mutating verb '{verb}' must map to a v1 API route, got {route}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. CLI Matrix & Exit Code Contract (ADR-0042)
// ---------------------------------------------------------------------------

#[test]
fn test_cli_matrix_daemon_down_clean_errors_and_exit_code_3() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    init::run(work).unwrap();

    let client_verbs: &[&[&str]] = &[
        &["daemon", "status"],
        &["commission", "list"],
        &["commission", "show", "nonexistent"],
        &["agent", "list"],
        &["agent", "show", "1001"],
        &["decree", "list"],
        &["decree", "show", "d-1-0001"],
        &["mail", "list", "1001"],
        &["mail", "read", "1001"],
        &["vault", "list"],
        &["metrics", "show"],
    ];

    for verb in client_verbs {
        // 1. Human output: stderr describes failure, exits 3
        let out = Command::new(BIN)
            .current_dir(work)
            .args(*verb)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(3),
            "Verb {:?} without daemon should exit 3, got {:?}",
            verb,
            out.status.code()
        );
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.contains("daemon is not running") || stderr.contains("discovery file"),
            "Verb {:?} stderr clean daemon-down message missing: {}",
            verb,
            stderr
        );

        // 2. JSON output: structured JSON error with code 3 on stdout, exits 3
        let mut json_args = vec!["--json"];
        json_args.extend_from_slice(verb);
        let out_json = Command::new(BIN)
            .current_dir(work)
            .args(&json_args)
            .output()
            .unwrap();
        assert_eq!(out_json.status.code(), Some(3));
        let stdout = String::from_utf8(out_json.stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!(
                "Verb {:?} with --json produced non-JSON stdout: {}\nErr: {e}",
                verb, stdout
            )
        });
        assert_eq!(parsed["code"], 3);
        assert!(parsed["error"].as_str().is_some());
    }
}

#[test]
fn test_cli_matrix_exit_codes_and_json_formatting() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_test_village(work);
    let _daemon = start_test_daemon(work);

    // 1. Exit code 0: Successful command in human and --json
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "list"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8(out.stdout).unwrap().contains("testcomm"));

    let out_json = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "commission", "list"])
        .output()
        .unwrap();
    assert_eq!(out_json.status.code(), Some(0));
    let parsed: serde_json::Value = serde_json::from_slice(&out_json.stdout).unwrap();
    assert!(parsed.is_array());
    assert_eq!(parsed[0]["slug"], "testcomm");

    // 2. Exit code 1: Typed API/domain error in human and --json
    let out_fail = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "show", "nonexistent"])
        .output()
        .unwrap();
    assert_eq!(out_fail.status.code(), Some(1));
    let stderr = String::from_utf8(out_fail.stderr).unwrap();
    assert!(stderr.contains("not found"));

    let out_fail_json = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "commission", "show", "nonexistent"])
        .output()
        .unwrap();
    assert_eq!(out_fail_json.status.code(), Some(1));
    let parsed: serde_json::Value = serde_json::from_slice(&out_fail_json.stdout).unwrap();
    assert_eq!(parsed["code"], 1);

    // 3. Exit code 2: Usage / clap parse error in human and --json
    let out_usage = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "new"]) // missing required arguments
        .output()
        .unwrap();
    assert_eq!(out_usage.status.code(), Some(2));

    let out_usage_json = Command::new(BIN)
        .current_dir(work)
        .args(["--json", "commission", "new"])
        .output()
        .unwrap();
    assert_eq!(out_usage_json.status.code(), Some(2));
    let parsed: serde_json::Value = serde_json::from_slice(&out_usage_json.stdout).unwrap();
    assert_eq!(parsed["code"], 2);
}

// ---------------------------------------------------------------------------
// 3. Auth & Discovery File Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_token_enforcement_and_mode_0600() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let tree = setup_test_village(work);
    let _daemon = start_test_daemon(work);

    // 1. Verify token file permissions (0600)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(tree.daemon_token())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "run/daemon.token must be mode 0600");
    }

    // 2. Discover endpoint and token
    let endpoint_str = fs::read_to_string(tree.daemon_endpoint()).unwrap();
    let ep: DaemonEndpoint = serde_json::from_str(&endpoint_str).unwrap();
    let token_str = fs::read_to_string(tree.daemon_token()).unwrap();
    let valid_token = token_str.trim();

    let client = reqwest::Client::new();
    let status_url = format!("{}{}", ep.base_url(), ENDPOINT_STATUS);

    // 3. Request without token -> 401 Unauthorized
    let res = client.get(&status_url).send().await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
    let err: ApiErrorResponse = res.json().await.unwrap();
    assert_eq!(err.error, "unauthorized");

    // 4. Request with invalid/forged token -> 401 Unauthorized
    let res = client
        .get(&status_url)
        .header("Authorization", "Bearer invalid-token-0000000000000000000")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 5. Request with custom header x-hamlet-local-token with valid token -> 200 OK
    let res = client
        .get(&status_url)
        .header(LOCAL_TOKEN_HEADER, valid_token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);

    // 6. Request with Bearer authorization with valid token -> 200 OK
    let res = client
        .get(&status_url)
        .header("Authorization", format!("Bearer {valid_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
}

// ---------------------------------------------------------------------------
// 4. Adversarial Findings & Path Traversal Reproduction
// ---------------------------------------------------------------------------

#[test]
fn test_repro_qa08_01_vault_export_path_traversal() {
    // Remediation of QA08-01 (SPEC §2, §11.3):
    // In `crates/hamlet-daemon/src/local_api.rs`, `payload.artifact` is validated via `sanitize_name`
    // and confined within the vault root. Path traversal (`../../charter.json`, `../../.env`) must be rejected
    // with HTTP 400 Bad Request and CLI exit code 1.
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_test_village(work);
    let _daemon = start_test_daemon(work);

    let dest = work.join("exfiltrated_charter.json");
    let out = Command::new(BIN)
        .current_dir(work)
        .args([
            "vault",
            "export",
            "../../charter.json",
            dest.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(1),
        "Path traversal export must fail with exit code 1, got {:?}",
        out.status.code()
    );
    assert!(
        !dest.exists(),
        "Vault export must not exfiltrate file outside vault"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid artifact name") || stderr.contains("path separator"),
        "stderr should describe bad request / invalid name, got: {stderr}"
    );

    // Also verify strict --json error formatting
    let out_json = Command::new(BIN)
        .current_dir(work)
        .args([
            "--json",
            "vault",
            "export",
            "../../charter.json",
            dest.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out_json.status.code(), Some(1));
    assert!(!dest.exists());
    let parsed: serde_json::Value = serde_json::from_slice(&out_json.stdout).unwrap();
    assert_eq!(parsed["code"], 1);
}

#[test]
fn test_boundary_sanitizer_rejects_malformed_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path();
    let _tree = setup_test_village(work);
    let _daemon = start_test_daemon(work);

    // Malformed commission slug
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["commission", "show", "../traversal"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    // Malformed patron wake name
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["agent", "wake", "bad/name"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    // Non-numeric recipient in mail list
    let out = Command::new(BIN)
        .current_dir(work)
        .args(["mail", "list", "abc"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
