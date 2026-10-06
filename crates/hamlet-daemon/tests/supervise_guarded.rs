//! Guarded live session smoke test (Sprint 03 step 02; CONVENTIONS §5).
//!
//! Runs only when `HAMLET_PODMAN_TESTS=1` **and** `podman` is on PATH;
//! otherwise it skips with a recorded reason. Also `#[ignore]`d so a plain
//! `cargo test` never touches podman; `scripts/ci.sh` runs it via
//! `cargo test --workspace -- --ignored podman` on capable hosts.
//!
//! Coverage (the step-02 verify contract): one REAL session over
//! `podman exec` stdio — spawn → readiness → (message) → graceful stop —
//! through the production [`PodmanSpawner`] path. The session agent is:
//! - **real pi** (`pi --mode rpc`) when the container image provides it;
//! - otherwise a **`sh` echo session**: a minimal pi-dialect JSONL script
//!   (`get_state` probe + `prompt` → run events) written into the parcel
//!   home via `tee`. The default village blueprint's base image is plain
//!   alpine (pi lands via nvm only after a full bootstrap), so the echo
//!   fallback is the expected path in CI; which one ran is printed.
//!
//! The smoke fixture creates the parcel home as the daemon user, so it also
//! needs the QA04-07 host precondition (a daemon user disjoint from the
//! rootless userns); otherwise uid-1001 writes hit EACCES for host-shape
//! reasons. See `tests/support/mod.rs`.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::{Charter, CommissionStatus};
use hamlet_core::config::{Config, Secrets};
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::session::{SessionId, SessionStatus};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::hostperm::HostPermissions;
use hamlet_daemon::podman::{ContainerState, PodmanRuntime, RunSpec, Runtime as _};
use hamlet_daemon::supervise::{
    pi_session_cmd, ParcelRef, PodmanSpawner, SessionSpawner, Supervisor,
};
use hamlet_daemon::telemetry::Telemetry;

fn podman_guard() -> Option<String> {
    support::podman_env_guard()
}

fn name(s: &str) -> SafeName {
    sanitize_name(s).unwrap()
}

const TEST_UID: u32 = 1001;
const TEST_COMMISSION: u64 = 99;
/// Container name base; the name actually used is **per-run** (QA04-26):
/// podman names are global to the store, so a second overlapping run of this
/// suite would adopt (and then `rm -f`) this run's container and fail inside
/// an assertion. `support::scoped` suffixes it with this process's pid.
const CONTAINER_BASE: &str = "hamlet-supervise-smoke";

/// Minimal pi-dialect echo session (JSONL): answers the readiness probe
/// and one prompt with the canonical run-event sequence, exits on stdin
/// EOF (graceful stop). Written to the parcel home, run as the slot uid.
const ECHO_SESSION_SH: &str = r#"#!/bin/sh
# hamlet guarded smoke: minimal pi-dialect echo session
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"get_state"'*)
      printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false}}\n' "$id" ;;
    *'"prompt"'*)
      printf '{"type":"response","id":"%s","command":"prompt","success":true,"data":{"disposition":"started"}}\n' "$id"
      printf '{"type":"agent_start"}\n'
      printf '{"type":"agent_end","willRetry":false}\n'
      printf '{"type":"agent_settled"}\n' ;;
  esac
done
"#;

async fn wait_for_status(supervisor: &Supervisor, want: &[SessionStatus]) -> SessionStatus {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let record = supervisor
            .session_record(TEST_UID)
            .unwrap()
            .expect("charter session record");
        if want.contains(&record.status) {
            return record.status;
        }
        assert!(
            Instant::now() < deadline,
            "timeout waiting for {:?}, last status {}",
            want,
            record.status
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_live_session_spawn_message_stop() {
    if let Some(reason) = podman_guard()
        .or_else(support::userns_precondition)
        .or_else(|| support::work_area_precondition(&std::env::temp_dir()))
    {
        support::skip("podman_live_session_spawn_message_stop", &reason);
        return;
    }
    // QA04-26: per-run container name (see `CONTAINER_BASE`).
    let container = support::scoped(CONTAINER_BASE);

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
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
    let mut charter = Charter::empty();
    charter
        .commissions
        .insert(TEST_COMMISSION, CommissionStatus::Open);
    let uid = charter
        .allocate_slot(name("smoke"), Some(TEST_COMMISSION))
        .expect("slot");
    assert_eq!(uid, TEST_UID);
    charter.save_atomic(&tree.charter()).expect("charter");

    let config = Config::default();
    let runtime =
        Arc::new(PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(2)));
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");
    let image = blueprint.meta().base_image().to_owned();
    let spec = RunSpec::for_agent(
        &name(&container),
        TEST_UID,
        TEST_COMMISSION,
        &blueprint,
        &config,
    )
    .expect("valid agent spec");

    // The parcel home is 0700 **slot-owned** (ADR-0011 cl. 2), applied through
    // the same seam provisioning uses. A daemon-owned home is only "working"
    // on a host where the subuid mapping silently failed (QA04-07c): inside
    // the container it would appear owned by uid 0, which lets container-root
    // writes pass — and the live session, which podman execs as the slot uid,
    // then fails with EACCES. Build the fixture the way the product does.
    let perms = HostPermissions::new(runtime.as_ref());
    perms
        .apply_parcel_home(&tree, TEST_UID)
        .await
        .expect("slot-owned parcel home (ADR-0011 cl. 2)");

    let cleanup = || {
        let _ = StdCommand::new("podman")
            .args(["rm", "-f", &container])
            .output();
    };
    cleanup();

    let result = async {
        runtime.pull(&image).await?;
        runtime.run_detached(&spec, &tree).await?;
        assert_eq!(runtime.state(&container).await?, ContainerState::Running);

        // Which agent does the container provide? Real pi if present
        // (bootstrap-derived images install it via nvm); otherwise the
        // echo-session fallback below.
        let pi_probe = runtime
            .exec(
                &container,
                &["sh".to_owned(), "-c".to_owned(), "command -v pi".to_owned()],
                None,
            )
            .await?;
        let have_pi = pi_probe.code == 0 && !pi_probe.stdout_lossy().trim().is_empty();

        let (cmd, brief, want) = if have_pi {
            eprintln!("NOTE live smoke: using REAL pi (--mode rpc)");
            (
                pi_session_cmd(&config.sessions),
                // No tasking against real pi without provider credentials:
                // readiness + graceful stop is the smoke contract.
                "",
                vec![SessionStatus::Running, SessionStatus::Idle],
            )
        } else {
            eprintln!("NOTE live smoke: pi unavailable in {image}; using sh echo session");
            let write = runtime
                .exec(
                    &container,
                    &["tee".to_owned(), "/home/1001/echo.sh".to_owned()],
                    Some(ECHO_SESSION_SH.as_bytes()),
                )
                .await?;
            assert_eq!(write.code, 0, "echo.sh write: {}", write.stderr_lossy());
            (
                vec!["sh".to_owned(), "/home/1001/echo.sh".to_owned()],
                "smoke ping",
                vec![SessionStatus::Idle],
            )
        };

        let telemetry =
            Arc::new(Telemetry::open(tmp.path().join(".hamlet-data")).expect("telemetry"));
        let spawner: Arc<dyn SessionSpawner> = Arc::new(PodmanSpawner::new(
            Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>
        ));
        let supervisor = Supervisor::new(
            tree.clone(),
            config.clone(),
            spawner,
            Arc::clone(&telemetry),
        );

        // Secrets ride the exec env (-e), never files in the home.
        let secrets = Secrets::from_vars([("HAMLET_API_KEY_ANTHROPIC", "test-key-do-not-use")]);
        let parcel =
            ParcelRef::new(TEST_UID, name(&container), TEST_COMMISSION).with_env(BTreeMap::new());
        let session = supervisor
            .start_session_with_cmd(&parcel, brief, &secrets, cmd)
            .await
            .expect("start_session over real podman exec");
        assert_eq!(session, SessionId::new(TEST_UID, 0).unwrap());

        // spawn → readiness → (message settles at the task boundary)
        let reached = wait_for_status(&supervisor, &want).await;
        eprintln!("NOTE live smoke: session reached {reached}");

        // Graceful stop: stdin EOF ends the exec; the container lives on
        // (session lifecycle is independent of container lifecycle).
        let final_status = supervisor.stop_session(TEST_UID).await.expect("stop");
        assert_eq!(final_status, SessionStatus::Stopped);
        assert_eq!(runtime.state(&container).await?, ContainerState::Running);

        // No crash telemetry on the happy path.
        let traces = telemetry
            .recent_traces(
                hamlet_daemon::telemetry::AgentKey {
                    commission: TEST_COMMISSION,
                    uid: TEST_UID,
                },
                usize::MAX,
            )
            .expect("traces");
        assert!(
            !traces.iter().any(|t| t.event == "session-crashed"),
            "unexpected crash: {traces:?}"
        );
        assert!(traces.iter().any(|t| t.event == "session-ready"));
        assert!(traces.iter().any(|t| t.event == "session-stopped"));

        supervisor.shutdown().await;
        Ok::<(), hamlet_daemon::podman::PodmanError>(())
    }
    .await;

    cleanup();
    result.expect("smoke: live session spawn → message → stop");
}
