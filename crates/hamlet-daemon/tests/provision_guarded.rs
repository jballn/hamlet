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
//!
//! The SPEC §4 per-slot uid policy needs the QA04-07 host precondition (a
//! daemon user disjoint from the rootless userns that can actually chown to a
//! subuid); it is checked up front and recorded as a SKIP with the host
//! remedy. See `tests/support/mod.rs`.

mod support;

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
    support::podman_env_guard()
}

/// Does `path` exist, asked through the daemon's own userns capability?
///
/// `podman unshare stat -c %F -- <path>` (the same typed probe the freight
/// promotion-read uses, ADR-0033): the parcel home is 0700 slot-owned, so a
/// host-side `Path::exists()` is EACCES-false on any host whose subuid
/// mapping actually works.
async fn unshare_exists(
    runtime: &PodmanRuntime,
    path: &std::path::Path,
) -> Result<bool, PodmanError> {
    let cmd = hamlet_daemon::podman::stat_cmd(path)?;
    // A non-zero exit means "no such path" (stat's own answer); anything else
    // is a real seam failure and stays an error.
    Ok(runtime
        .unshare_exec(&cmd)
        .await
        .map(|out| out.code == 0)
        .unwrap_or(false))
}

const COMMISSION: u64 = 91;
/// Container names are **per-run** (QA04-26): podman names are global to the
/// store, so two overlapping batteries provisioned each other's containers and
/// then failed inside an isolation assertion with text that reads like a
/// SPEC §4 defect. The constants are the stable *bases*; every container,
/// brief and cleanup below uses the pid-scoped name.
const AGENT_A_BASE: &str = "hamlet-guard-a"; // slot 1001
const AGENT_B_BASE: &str = "hamlet-guard-b"; // slot 1002
const UID_A: u32 = 1001;
const UID_B: u32 = 1002;

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_provisioned_parcels_enforce_the_spec4_isolation_invariants() {
    if let Some(reason) = podman_guard()
        .or_else(support::userns_precondition)
        .or_else(|| support::work_area_precondition(&std::env::temp_dir()))
    {
        support::skip(
            "podman_provisioned_parcels_enforce_the_spec4_isolation_invariants",
            &reason,
        );
        return;
    }

    // QA04-26: podman container names are global to the store, so two
    // overlapping batteries adopted (and `rm -f`ed) each other's parcels and
    // then failed inside isolation assertions with text that reads like a
    // SPEC §4 defect. Per-run names make concurrent runs coexist; the
    // battery-wide lock is the CI layer's half (`guarded-entry.sh`).
    let agent_a = support::scoped(AGENT_A_BASE);
    let agent_b = support::scoped(AGENT_B_BASE);

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");
    for agent in [&agent_a, &agent_b] {
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
        for name in [&agent_a, &agent_b] {
            let _ = StdCommand::new("podman").args(["rm", "-f", name]).output();
        }
    };
    cleanup();

    let result = async {
        // --- provision both parcels end to end (real podman) --------------
        provisioner
            .provision_parcel(&sanitize_name(&agent_a).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| PodmanError::invalid(e.to_string()))?;
        provisioner
            .provision_parcel(&sanitize_name(&agent_b).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| PodmanError::invalid(e.to_string()))?;

        // --- container A: identity + ro invariants ------------------------
        let out = run(&runtime, &agent_a, &["id", "-u"]).await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            UID_A.to_string(),
            "container user IS the slot uid (ADR-0011 subuid mapping)"
        );

        let denied = run(&runtime, &agent_a, &["touch", "/var/git/escape"]).await?;
        assert_ne!(denied.code, 0, "/var/git must be read-only inside");
        let denied = run(&runtime, &agent_a, &["touch", "/var/context/escape"]).await?;
        assert_ne!(denied.code, 0, "/var/context must be read-only inside");

        // --- home: rw, owned by the mapped UID -----------------------------
        let ok = run(&runtime, &agent_a, &["touch", "/home/1001/probe"]).await?;
        assert_eq!(
            ok.code,
            0,
            "home must be rw for its slot: {}",
            ok.stderr_lossy()
        );
        // Host-side confirmation that the bind mount IS the parcel home —
        // observed through the userns, because the host user cannot traverse a
        // 0700 slot-owned home (ADR-0011 cl. 2). The same wall `promotion_read`
        // hits (ADR-0033): the daemon reaches those trees with userns
        // capability, so its tests must too. A plain `Path::exists()` here is
        // EACCES-false on any host whose subuid mapping works, i.e. silently
        // vacuous exactly where it matters most.
        let seen = unshare_exists(runtime.as_ref(), &tree.parcel_home(UID_A).join("probe")).await?;
        assert!(
            seen,
            "the bind mount is the host parcel home (seen through the userns)"
        );
        let owner = run(&runtime, &agent_a, &["stat", "-c", "%u", "/home/1001"]).await?;
        assert_eq!(
            owner.stdout_lossy().trim(),
            UID_A.to_string(),
            "home is owned by the mapped UID inside the container"
        );

        // --- another parcel's home: not even visible -----------------------
        let hidden = run(&runtime, &agent_a, &["ls", "/home/1002"]).await?;
        assert_ne!(
            hidden.code, 0,
            "another parcel's home must not be reachable (not mounted; \
             0700 subuid-owned on host per ADR-0011)"
        );

        // --- mail: own spool rw, cross-UID read blocked by spool perms -----
        let own = run(&runtime, &agent_a, &["touch", "/var/mail/1001/probe"]).await?;
        assert_eq!(own.code, 0, "own spool must be rw: {}", own.stderr_lossy());
        let cross = run(&runtime, &agent_a, &["ls", "/var/mail/1002"]).await?;
        assert_ne!(
            cross.code, 0,
            "cross-UID spool read must be blocked by 0700 + per-UID \
             ownership (SPEC §4)"
        );

        // --- pkgcache: shared rw cache --------------------------------------
        let cache = run(&runtime, &agent_a, &["touch", "/var/pkgcache/probe-a"]).await?;
        assert_eq!(
            cache.code,
            0,
            "pkgcache is rw (cache semantics): {}",
            cache.stderr_lossy()
        );

        // --- freight: 01770 daemon-owned, other::--- — slot UIDs cannot
        // traverse until the Sprint 04 freight plane grants root ACLs
        // (ADR-0021 cl. 2); this suite never runs the plane.
        let freight = run(&runtime, &agent_a, &["ls", "/var/freight"]).await?;
        assert_ne!(
            freight.code, 0,
            "freight stays ACL-gated (01770, no slot grants) without the freight plane"
        );

        // --- mirror checks from container B ---------------------------------
        let out = run(&runtime, &agent_b, &["id", "-u"]).await?;
        assert_eq!(out.stdout_lossy().trim(), UID_B.to_string());
        let owner = run(&runtime, &agent_b, &["stat", "-c", "%u", "/home/1002"]).await?;
        assert_eq!(owner.stdout_lossy().trim(), UID_B.to_string());
        let cross = run(&runtime, &agent_b, &["ls", "/var/mail/1001"]).await?;
        assert_ne!(cross.code, 0, "cross-UID spool blocked in both directions");
        let hidden = run(&runtime, &agent_b, &["ls", "/home/1001"]).await?;
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
            let probe = run(&runtime, &agent_a, &["ls", sock]).await?;
            assert_ne!(
                probe.code, 0,
                "infrastructure socket path {sock} must not exist inside"
            );
        }

        // --- graceful teardown through the runtime seam ---------------------
        runtime.stop(&agent_a, Duration::from_secs(10)).await?;
        runtime.stop(&agent_b, Duration::from_secs(10)).await?;
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
    assert_eq!(host_mode(&tree.courier_freight()), 0o1770);
    assert_eq!(host_mode(&tree.commission_git(COMMISSION)), 0o755);
    assert_eq!(host_mode(&tree.market_pkgcache()), 0o1777);

    cleanup();
    result.expect("guarded provisioning: SPEC §4 isolation invariants");
}
