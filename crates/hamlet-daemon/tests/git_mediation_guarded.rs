//! Guarded real-podman git bundle mediation end-to-end test
//! (`CONVENTIONS §5`, `SPEC §14`, `ADR-0037`).
//!
//! Runs only when `HAMLET_PODMAN_TESTS=1` **and** `podman` is on `PATH`;
//! otherwise it skips with a recorded reason. `#[ignore]`d so a plain
//! `cargo test` never touches podman; `scripts/ci.sh` step 5 runs it via
//! `cargo test --workspace -- --ignored podman` on capable hosts.
//!
//! # Coverage
//!
//! 1. Seeds a commission bare git repository on the host via
//!    [`Ledger::create_commission`], verifying world-readable normalization so
//!    the userns-mapped container UID (`1001`) can read `/var/git` (`:ro`).
//! 2. Launches a real detached agent container (`--userns=keep-id:uid=1001,gid=1001`)
//!    from the derived village image and runs [`seed_agent_git_home`] inside it.
//! 3. Inside the container:
//!    - Confirms `/var/git` is read-only (`git push origin` fails).
//!    - Clones `/var/git` into `/home/1001/work`, checks out `agent/1001`,
//!      commits using the seeded `/home/1001/.gitconfig` (`1001@hamlet.local`),
//!      and invokes `/home/1001/.pixi/bin/hamlet-git-push container-push`.
//! 4. On the host:
//!    - Runs [`Ledger::reconcile_git_spools`] (exercising the `podman unshare`
//!      spool helper against the `0700` subuid-owned `/home/1001/git-spool`).
//!    - Asserts `refs/heads/agent/1001` is updated in `commission<n>/git/`,
//!      `keep/census/archives/git/<n>/1001/container-push.bundle` is archived,
//!      `GitDiffReady` woke the Mayor with the unified diff, and
//!      `/home/1001/git-spool/container-push.bundle.ack` is readable by UID
//!      `1001` inside the container.
//!    - Verifies the container can `git fetch /var/git refs/heads/agent/1001`
//!      to observe its mediated branch tip in `/var/git`.

mod support;

use std::fs;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::packet::WakeReason;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::bootstrap::Bootstrapper;
use hamlet_daemon::hostperm::HostPermissions;
use hamlet_daemon::ledger::{
    seed_agent_git_home, GitBundleAck, GitBundleAckStatus, GitBundleOutcome, Ledger,
    RecordingCommissionWake,
};
use hamlet_daemon::podman::{ContainerState, ExecOutput, PodmanRuntime, RunSpec, Runtime};

const TEST_UID: u32 = 1001;
const CONTAINER_BASE: &str = "hamlet-gitmed";

fn podman_guard() -> Option<String> {
    support::podman_env_guard()
}

async fn exec_sh<R: Runtime>(
    runtime: &R,
    container: &str,
    script: &str,
) -> Result<ExecOutput, String> {
    let argv = vec!["sh".to_owned(), "-lc".to_owned(), script.to_owned()];
    runtime
        .exec(container, &argv, None)
        .await
        .map_err(|e| e.to_string())
}

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_git_bundle_mediation_round_trip_and_readonly_var_git() {
    if let Some(reason) = podman_guard() {
        eprintln!("SKIP podman_git_bundle_mediation_round_trip_and_readonly_var_git: {reason}");
        return;
    }
    let container = support::scoped(CONTAINER_BASE);
    let ident = sanitize_name(&container).expect("valid container name");

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("save charter");

    // Seed commission 1 with an initial commit on `main`.
    let seed_dir = tmp.path().join("seed-src");
    fs::create_dir_all(&seed_dir).expect("create seed dir");
    fs::write(seed_dir.join("README.md"), "# Seeded Village Repo\n").expect("write readme");

    let wake = Arc::new(RecordingCommissionWake::new());
    let ledger = Arc::new(Ledger::new(tree.clone()).with_wake(Arc::clone(&wake) as _));
    let created = ledger
        .create_commission("podman-med", "# Podman Mediation\n", Some(&seed_dir))
        .expect("create commission");

    // Allocate slot 1001 in charter and assign to commission.
    let mut charter = Charter::load(&tree.charter()).expect("load charter");
    let allocated_uid = charter
        .allocate_slot(ident.clone(), Some(created.id))
        .expect("allocate slot 1001");
    assert_eq!(allocated_uid, TEST_UID);
    charter.save_atomic(&tree.charter()).expect("save charter");
    ledger
        .assign_uid(created.id, TEST_UID)
        .expect("assign uid to commission");

    // Ensure mount source directories exist before `podman run`.
    let spool_name = sanitize_name(&TEST_UID.to_string()).expect("spool name");
    for dir in [
        tree.parcel_home(TEST_UID),
        tree.commission_git(created.id),
        tree.courier_mail(),
        tree.courier_mail_recipient(&spool_name),
        tree.courier_freight(),
        tree.market_pkgcache(),
        tree.tavern_gossip(),
    ] {
        fs::create_dir_all(&dir).expect("create mount source dir");
    }

    let config = Config::default();
    let runtime = PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(3));
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");
    let bootstrapper = Bootstrapper::new(&tree, &runtime).expect("bootstrapper");

    let home_dir = tree.parcel_home(TEST_UID);
    let mail_dir = tree.courier_mail_recipient(&spool_name);
    let cleanup = || {
        let _ = StdCommand::new("podman")
            .args(["rm", "-f", &container])
            .output();
        let _ = StdCommand::new("podman")
            .args([
                "unshare",
                "chown",
                "-R",
                "0:0",
                &home_dir.to_string_lossy(),
                &mail_dir.to_string_lossy(),
            ])
            .output();
    };
    cleanup();

    let result: Result<(), String> = async {
        // Apply ADR-0011 host permissions so `/home/1001` is `0700` owned by
        // subuid 1001 inside the userns before the container starts.
        let perms = HostPermissions::new(&runtime);
        perms
            .apply_parcel_home(&tree, TEST_UID)
            .await
            .map_err(|e| format!("apply_parcel_home: {e}"))?;
        perms
            .apply_mail_spool(&tree, TEST_UID)
            .await
            .map_err(|e| format!("apply_mail_spool: {e}"))?;
        perms
            .apply_shared(&tree, created.id)
            .await
            .map_err(|e| format!("apply_shared: {e}"))?;

        let image = bootstrapper
            .ensure_image(&blueprint)
            .await
            .map_err(|e| format!("ensure_image: {e}"))?;

        let spec = RunSpec::for_agent(&ident, TEST_UID, created.id, &blueprint, &config)
            .map_err(|e| format!("for_agent: {e}"))?
            .with_image(image.tag);

        runtime
            .run_detached(&spec, &tree)
            .await
            .map_err(|e| format!("run_detached: {e}"))?;
        let state = runtime
            .state(&container)
            .await
            .map_err(|e| format!("state: {e}"))?;
        assert_eq!(state, ContainerState::Running);

        // Seed `/home/1001/git-spool`, `/home/1001/.gitconfig`, and
        // `/home/1001/.pixi/bin/hamlet-git-push` inside the container.
        seed_agent_git_home(&runtime, &ident, TEST_UID)
            .await
            .map_err(|e| format!("seed_agent_git_home: {e}"))?;

        // Inside the container: clone `/var/git`, verify direct `git push` to
        // `/var/git` is rejected (read-only mount), then commit on `agent/1001`
        // and stage `container-push.bundle` via `hamlet-git-push`.
        let clone_and_push = exec_sh(
            &runtime,
            &container,
            "set -eu; git clone /var/git /home/1001/work; cd /home/1001/work; git checkout -b agent/1001; echo 'from container 1001' > container.txt; git add container.txt; git commit -m 'feat: commit inside container'; if git push origin agent/1001 2>/dev/null; then echo 'ERROR: direct git push to ro /var/git succeeded' >&2; exit 1; fi; /home/1001/.pixi/bin/hamlet-git-push container-push; git rev-parse HEAD",
        )
        .await?;
        assert_eq!(
            clone_and_push.code,
            0,
            "in-container clone + hamlet-git-push failed: stdout={} stderr={}",
            clone_and_push.stdout_lossy(),
            clone_and_push.stderr_lossy()
        );
        let commit_oid = clone_and_push
            .stdout_lossy()
            .lines()
            .last()
            .unwrap_or_default()
            .trim()
            .to_owned();
        assert_eq!(commit_oid.len(), 40, "expected 40-hex commit oid, got {commit_oid:?}");

        // Host-side reconciliation mediates the bundle out of the `0700`
        // subuid-owned `/home/1001/git-spool`.
        let outcomes = ledger
            .reconcile_git_spools()
            .map_err(|e| format!("reconcile_git_spools: {e}"))?;
        assert_eq!(outcomes.len(), 1, "expected 1 mediated bundle, got {outcomes:?}");
        assert!(
            matches!(
                &outcomes[0],
                GitBundleOutcome::Accepted { head_oid, .. } if head_oid == &commit_oid
            ),
            "expected Accepted with head {commit_oid}, got {:?}",
            outcomes[0]
        );

        // Verify `.ack` is readable by UID 1001 inside the container AND that
        // `/var/git` now exposes `refs/heads/agent/1001` to the container.
        let ack_out = exec_sh(
            &runtime,
            &container,
            "cat /home/1001/git-spool/container-push.bundle.ack",
        )
        .await?;
        assert_eq!(
            ack_out.code,
            0,
            "in-container ack check failed: {}",
            ack_out.stderr_lossy()
        );
        let ack = GitBundleAck::parse(&ack_out.stdout_lossy()).expect("parse ack from container output");
        assert_eq!(ack.status, GitBundleAckStatus::Accepted);

        let fetch_out = exec_sh(
            &runtime,
            &container,
            "set -eu; cd /home/1001/work; git fetch origin refs/heads/agent/1001; git rev-parse FETCH_HEAD",
        )
        .await?;
        assert_eq!(
            fetch_out.code,
            0,
            "in-container fetch check failed: {}",
            fetch_out.stderr_lossy()
        );
        let fetched_oid = fetch_out.stdout_lossy().lines().last().unwrap_or_default().trim().to_owned();
        assert_eq!(fetched_oid, commit_oid);

        // Verify archive copy on host and Mayor `GitDiffReady` wake.
        assert!(
            tree.census_git_archive(created.id, TEST_UID)
                .join("container-push.bundle")
                .is_file(),
            "archived bundle must exist in keep/census/archives/git/"
        );
        let wakes = wake.events();
        assert!(
            wakes
                .iter()
                .any(|(_, w)| w.kind().wake_reason() == WakeReason::GitDiffReady
                    && w.body().contains("+from container 1001")),
            "expected GitDiffReady wake with unified diff, got {wakes:?}"
        );

        runtime
            .stop(&container, Duration::from_secs(10))
            .await
            .map_err(|e| format!("stop: {e}"))?;
        Ok(())
    }
    .await;

    cleanup();
    result.expect("podman git bundle mediation round-trip");
}
