//! Guarded real-podman provisioning suite (CONVENTIONS §5; SPEC §4
//! isolation invariants; ADR-0011 permission model).
//!
//! Runs only when `HAMLET_PODMAN_TESTS=1` **and** `podman` is on PATH;
//! otherwise it skips with a recorded reason. `#[ignore]`d so a plain
//! `cargo test` never touches podman; `scripts/ci.sh` step 5 runs it via
//! `cargo test --workspace -- --ignored podman` on capable hosts.
//!
//! Coverage (step 03 verify contract): provision TWO real parcels end to
//! end (`provision_parcel` → real `podman run`), then assert FROM INSIDE a
//! container: `/var/git/` and `/var/context/` are ro (writes fail), home is
//! rw and owned by the mapped slot UID, another parcel's home is not even
//! visible, and cross-UID mail spool reads are blocked by spool perms
//! (0700 + subuid ownership — the invariant keep-id could never provide,
//! ADR-0011 context). Host-side braces (modes, subuid ownership) are
//! asserted after teardown.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::Provisioner;
use hamlet_daemon::podman::{ExecOutput, PodmanError, PodmanRuntime, Runtime as _};

/// One buffered `podman exec` against a guard container.
async fn run(
    runtime: &PodmanRuntime,
    container: &str,
    cmd: &[&str],
) -> Result<ExecOutput, PodmanError> {
    let argv: Vec<String> = cmd.iter().map(|s| (*s).to_owned()).collect();
    runtime.exec(container, &argv, None).await
}

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

const COMMISSION: u64 = 91;
const AGENT_A: &str = "hamlet-guard-a"; // slot 1001
const AGENT_B: &str = "hamlet-guard-b"; // slot 1002
const UID_A: u32 = 1001;
const UID_B: u32 = 1002;

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_provisioned_parcels_enforce_the_spec4_isolation_invariants() {
    if let Some(reason) = podman_guard() {
        eprintln!(
            "SKIP podman_provisioned_parcels_enforce_the_spec4_isolation_invariants: {reason}"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");
    for agent in [AGENT_A, AGENT_B] {
        let brief = tree.register_brief(&sanitize_name(agent).unwrap());
        fs::write(brief, format!("# {agent}\n\nGuarded-test brief.\n")).expect("brief");
    }

    let config = Config::default();
    let runtime =
        Arc::new(PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(2)));
    let provisioner = Provisioner::new(
        tree.clone(),
        config,
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");

    // Whatever happens, the guard containers must not leak.
    let cleanup = || {
        for name in [AGENT_A, AGENT_B] {
            let _ = StdCommand::new("podman").args(["rm", "-f", name]).output();
        }
    };
    cleanup();

    let result = async {
        // --- provision both parcels end to end (real podman) --------------
        provisioner
            .provision_parcel(&sanitize_name(AGENT_A).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| PodmanError::invalid(e.to_string()))?;
        provisioner
            .provision_parcel(&sanitize_name(AGENT_B).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| PodmanError::invalid(e.to_string()))?;

        // --- container A: identity + ro invariants ------------------------
        let out = run(&runtime, AGENT_A, &["id", "-u"]).await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            UID_A.to_string(),
            "container user IS the slot uid (ADR-0011 subuid mapping)"
        );

        let denied = run(&runtime, AGENT_A, &["touch", "/var/git/escape"]).await?;
        assert_ne!(denied.code, 0, "/var/git must be read-only inside");
        let denied = run(&runtime, AGENT_A, &["touch", "/var/context/escape"]).await?;
        assert_ne!(denied.code, 0, "/var/context must be read-only inside");

        // --- home: rw, owned by the mapped UID -----------------------------
        let ok = run(&runtime, AGENT_A, &["touch", "/home/1001/probe"]).await?;
        assert_eq!(
            ok.code,
            0,
            "home must be rw for its slot: {}",
            ok.stderr_lossy()
        );
        assert!(
            tree.parcel_home(UID_A).join("probe").exists(),
            "the bind mount is the host parcel home"
        );
        let owner = run(&runtime, AGENT_A, &["stat", "-c", "%u", "/home/1001"]).await?;
        assert_eq!(
            owner.stdout_lossy().trim(),
            UID_A.to_string(),
            "home is owned by the mapped UID inside the container"
        );

        // --- another parcel's home: not even visible -----------------------
        let hidden = run(&runtime, AGENT_A, &["ls", "/home/1002"]).await?;
        assert_ne!(
            hidden.code, 0,
            "another parcel's home must not be reachable (not mounted; \
             0700 subuid-owned on host per ADR-0011)"
        );

        // --- mail: own spool rw, cross-UID read blocked by spool perms -----
        let own = run(&runtime, AGENT_A, &["touch", "/var/mail/1001/probe"]).await?;
        assert_eq!(own.code, 0, "own spool must be rw: {}", own.stderr_lossy());
        let cross = run(&runtime, AGENT_A, &["ls", "/var/mail/1002"]).await?;
        assert_ne!(
            cross.code, 0,
            "cross-UID spool read must be blocked by 0700 + per-UID \
             ownership (SPEC §4)"
        );

        // --- pkgcache: shared rw cache --------------------------------------
        let cache = run(&runtime, AGENT_A, &["touch", "/var/pkgcache/probe-a"]).await?;
        assert_eq!(
            cache.code,
            0,
            "pkgcache is rw (cache semantics): {}",
            cache.stderr_lossy()
        );

        // --- freight: 0710 daemon-owned — slot UIDs cannot traverse until
        // Sprint 04 layers the POSIX ACL sandboxes on top (ADR-0011 cl. 3).
        let freight = run(&runtime, AGENT_A, &["ls", "/var/freight"]).await?;
        assert_ne!(
            freight.code, 0,
            "freight is ACL-gated (0710) until Sprint 04 setfacl sandboxes"
        );

        // --- mirror checks from container B ---------------------------------
        let out = run(&runtime, AGENT_B, &["id", "-u"]).await?;
        assert_eq!(out.stdout_lossy().trim(), UID_B.to_string());
        let owner = run(&runtime, AGENT_B, &["stat", "-c", "%u", "/home/1002"]).await?;
        assert_eq!(owner.stdout_lossy().trim(), UID_B.to_string());
        let cross = run(&runtime, AGENT_B, &["ls", "/var/mail/1001"]).await?;
        assert_ne!(cross.code, 0, "cross-UID spool blocked in both directions");
        let hidden = run(&runtime, AGENT_B, &["ls", "/home/1001"]).await?;
        assert_ne!(hidden.code, 0);

        // --- no infrastructure sockets reachable (SPEC §4) ------------------
        // QA fix (Sprint 02 step 05, HANDOFF F-02-02): the old probe listed
        // the `/var/run` DIRECTORY — on Alpine `/var/run` is a symlink to
        // `/run`, which exists in every image, so `ls /var/run` exits 0 and
        // the probe would false-fail the first real podman run. The SPEC §4
        // invariant is that no host infrastructure SOCKET is reachable.
        for sock in [
            "/run/podman/podman.sock",
            "/run/docker.sock",
            "/var/run/docker.sock",
            "/run/dbus/system_bus_socket",
            "/var/run/dbus/system_bus_socket",
        ] {
            let probe = run(&runtime, AGENT_A, &["ls", sock]).await?;
            assert_ne!(
                probe.code, 0,
                "infrastructure socket path {sock} must not exist inside"
            );
        }

        // --- graceful teardown through the runtime seam ---------------------
        runtime.stop(AGENT_A, Duration::from_secs(10)).await?;
        runtime.stop(AGENT_B, Duration::from_secs(10)).await?;
        Ok::<(), PodmanError>(())
    }
    .await;

    // --- host-side braces (ADR-0011 cl. 3) — asserted even on failure -----
    let host_mode = |p: &std::path::Path| {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0)
    };
    assert_eq!(host_mode(&tree.parcel_home(UID_A)), 0o700);
    let host_owner = fs::metadata(tree.parcel_home(UID_A))
        .map(|m| m.uid())
        .unwrap_or(u32::MAX);
    let daemon_euid = StdCommand::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(u32::MAX);
    assert_ne!(
        host_owner, daemon_euid,
        "home must be subuid-owned on the host (podman unshare chown ran), \
         not daemon-owned — cross-container ownership isolation depends on it"
    );
    assert_eq!(host_mode(&tree.courier_freight()), 0o710);
    assert_eq!(host_mode(&tree.commission_git(COMMISSION)), 0o755);
    assert_eq!(host_mode(&tree.market_pkgcache()), 0o1777);

    cleanup();
    result.expect("guarded provisioning: SPEC §4 isolation invariants");
}
