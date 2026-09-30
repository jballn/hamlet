//! Guarded real-podman smoke test (CONVENTIONS §5, ADR-0010).
//!
//! Runs only when `HAMLET_PODMAN_TESTS=1` **and** `podman` is on PATH;
//! otherwise it skips with a recorded reason. The test is also `#[ignore]`d
//! so a plain `cargo test` never touches podman; `scripts/ci.sh` step 5 runs
//! it via `cargo test --workspace -- --ignored podman` on capable hosts.
//!
//! Coverage: one real `run_detached` + `state` + `exec` (userns-mapped UID,
//! ro `/var/git/` invariant) + `stop`, per the Sprint 02 step 02 verify
//! contract. Subuid/gid runtime verification proper is step 03's addendum.

use std::fs;
use std::process::Command as StdCommand;
use std::time::Duration;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::config::Config;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::podman::{ContainerState, PodmanRuntime, RunSpec, Runtime as _};

/// CONVENTIONS §5 guard: run only with `HAMLET_PODMAN_TESTS=1` and podman
/// on PATH; otherwise skip with a recorded reason.
fn podman_guard() -> Option<String> {
    if std::env::var("HAMLET_PODMAN_TESTS").unwrap_or_default() != "1" {
        return Some("HAMLET_PODMAN_TESTS != 1".to_owned());
    }
    let found = StdCommand::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !found {
        return Some("podman not on PATH".to_owned());
    }
    None
}

const TEST_UID: u32 = 1001;
const TEST_COMMISSION: u64 = 99;
const CONTAINER: &str = "hamlet-podman-smoke";

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_smoke_run_state_exec_stop() {
    if let Some(reason) = podman_guard() {
        eprintln!("SKIP podman_smoke_run_state_exec_stop: {reason}");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    // Bind-mount source dirs must exist before `podman run -v`.
    for dir in [
        tree.parcel_home(TEST_UID),
        tree.commission_git(TEST_COMMISSION),
        tree.courier_mail(),
        tree.courier_freight(),
        tree.market_pkgcache(),
        tree.tavern_gossip(),
    ] {
        fs::create_dir_all(&dir).expect("mount source dir");
    }

    let config = Config::default();
    let runtime = PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(2));
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");
    let image = blueprint.meta().base_image().to_owned();

    let spec = RunSpec::for_agent(
        &sanitize_name(CONTAINER).unwrap(),
        TEST_UID,
        TEST_COMMISSION,
        &blueprint,
        &config,
    )
    .expect("valid agent spec");

    // Cleanup helper: whatever happens, the smoke container must not leak.
    let cleanup = || {
        let _ = StdCommand::new("podman")
            .args(["rm", "-f", CONTAINER])
            .output();
    };
    cleanup();

    let result = async {
        runtime.pull(&image).await?;
        let id = runtime.run_detached(&spec, &tree).await?;
        assert!(!id.is_empty(), "podman run -d must print a container id");

        assert_eq!(runtime.state(CONTAINER).await?, ContainerState::Running);

        // Userns mapping: the container user IS the charter UID slot.
        let out = runtime
            .exec(CONTAINER, &["id".to_owned(), "-u".to_owned()], None)
            .await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            TEST_UID.to_string(),
            "container uid must equal the charter slot"
        );

        // SPEC §4 isolation invariant: /var/git/ is ro.
        let denied = runtime
            .exec(
                CONTAINER,
                &["touch".to_owned(), "/var/git/escape".to_owned()],
                None,
            )
            .await?;
        assert_ne!(denied.code, 0, "/var/git must be read-only inside");

        runtime.stop(CONTAINER, Duration::from_secs(10)).await?;
        assert_eq!(runtime.state(CONTAINER).await?, ContainerState::Exited);
        Ok::<(), hamlet_daemon::podman::PodmanError>(())
    }
    .await;

    cleanup();
    result.expect("smoke: run_detached + state + exec + stop");
}
