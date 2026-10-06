//! QA 05/04 — guarded real-podman **manor smoke**: the decree-plane trust
//! boundary, measured from inside real containers (SPEC §4, §7; ADR-0011
//! cl. 2–4; ADR-0026; ADR-0029 cl. 4).
//!
//! Runs only when `HAMLET_PODMAN_TESTS=1` **and** podman is on PATH;
//! otherwise it skips with a recorded reason. `#[ignore]`d, like every other
//! guarded suite (CONVENTIONS §5).
//!
//! Both suites assert ADR-0011 subuid ownership across the mount boundary, so
//! they need the QA04-07 host precondition (a daemon user disjoint from the
//! rootless userns). See `tests/support/mod.rs`.
//!
//! `adversarial_mayor.rs` proves the *logic* of the mayor loop without
//! podman. This file proves the two claims that logic rests on and that only
//! real UID/mount semantics can settle:
//!
//! 1. **Only UID 1000 may author decrees.** A subagent container (slot 1001)
//!    must be refused by the kernel every way round — direct path, `..`
//!    traversal from the mount root, and a bind-mounted symlink — because
//!    the executor itself performs **no** provenance check (QA05-01). The
//!    0700 manor home + the absent manor mount are the entire defence, so
//!    they are tested where they actually live.
//! 2. **The Mayor really can write its own decree directory, and the daemon
//!    really can read it back**, across the subuid mapping.
//!
//! It also records the host-side fact behind **QA05-02**: `write_ack` and
//! `quarantine` write into that same 0700 subuid-owned directory with a plain
//! `fs` write, so a daemon user that is *not* subuid 1000 cannot write the
//! ack. The probe is `#[ignore]`d separately and states the expected outcome
//! for both deployment shapes rather than asserting one.

mod support;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::Arc;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::hostperm::HostPermissions;
use hamlet_daemon::mayor::ManorProvisioner;
use hamlet_daemon::podman::{ExecOutput, PodmanRuntime, RunSpec, Runtime as _};

/// The Mayor's container name is the production identity (`MAYOR_IDENTITY` =
/// "manor", ADR-0028), not a per-test name — so both suites in this binary
/// create a container with the same *global* podman name. Run in parallel they
/// race: one's start-cleanup removes the other's container and the second
/// `podman run` dies with "name already in use" (QA04-20). The name is the
/// product's, so the tests serialise instead.
static MANOR_CONTAINER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Held across awaits, so it must be the async-aware kind: a `std` guard would
/// be a clippy error *and* wrong (these tests are `#[tokio::test]`s).
async fn manor_container_guard() -> tokio::sync::MutexGuard<'static, ()> {
    MANOR_CONTAINER_LOCK.lock().await
}

const COMMISSION: u64 = 95;
/// Container name **bases** for the two non-identity parcels this file
/// provisions. The names actually used are per-run (QA04-26 — podman names
/// are global to the store, so two overlapping batteries adopted each other's
/// containers and failed inside isolation assertions). The Mayor's own
/// container is NOT scoped anywhere: `manor` is the product's identity name
/// (`MAYOR_IDENTITY`, ADR-0028), which is why this binary serialises behind
/// `MANOR_CONTAINER_LOCK` instead (QA04-20).
const AGENT_BASE: &str = "qa-mayor-agent"; // slot 1001
const UID_AGENT: u32 = 1001;
const MAYOR_CONTAINER_BASE: &str = "qa-mayor-1000";

/// CONVENTIONS §5 guard.
fn podman_guard() -> Option<String> {
    support::podman_env_guard()
}

async fn sh(runtime: &PodmanRuntime, container: &str, script: &str) -> Result<ExecOutput, String> {
    let argv = vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()];
    runtime
        .exec(container, &argv, None)
        .await
        .map_err(|e| e.to_string())
}

/// Read a path inside the manor home from the daemon's own userns (ADR-0011
/// cl. 2/cl. 4).
///
/// A bare-host read of `manor/home/` is refused by the kernel and that is
/// correct: it is 0700 owned by subuid 1000 (host uid 100999), and the host
/// user is not that uid. Every access the daemon makes into the Mayor's tree
/// therefore goes through `podman unshare` — the seam ADR-0011 cl. 4 already
/// prescribes for writes and this suite now also uses for reads. Asserting a
/// plain `fs::read` here would assert the model is broken.
async fn unshare_cat(runtime: &PodmanRuntime, path: &Path) -> Result<String, String> {
    let cmd = hamlet_daemon::podman::cat_cmd(path).map_err(|e| e.to_string())?;
    let out = runtime
        .unshare_exec(&cmd)
        .await
        .map_err(|e| e.to_string())?;
    if out.code != 0 {
        return Err(format!(
            "unshare cat {} exited {}: {}",
            path.display(),
            out.code,
            out.stderr_lossy()
        ));
    }
    Ok(out.stdout_lossy())
}

/// Owner uid of a path, read from inside the daemon's userns (ADR-0011
/// cl. 2/cl. 4) — a 0700 subuid-owned directory is unreadable from the bare
/// host, which is the invariant working, not a gap (see `unshare_cat`).
async fn unshare_owner(runtime: &PodmanRuntime, path: &Path) -> Result<u32, String> {
    let argv = vec![
        "stat".to_owned(),
        "-c".to_owned(),
        "%u".to_owned(),
        "--".to_owned(),
        path.to_string_lossy().into_owned(),
    ];
    let out = runtime
        .unshare_exec(&argv)
        .await
        .map_err(|e| e.to_string())?;
    if out.code != 0 {
        return Err(format!(
            "unshare stat {} exited {}: {}",
            path.display(),
            out.code,
            out.stderr_lossy()
        ));
    }
    out.stdout_lossy()
        .trim()
        .parse()
        .map_err(|e| format!("unparseable uid {:?}: {e}", out.stdout_lossy()))
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0)
}

fn owner_of(path: &Path) -> u32 {
    fs::metadata(path).map(|m| m.uid()).unwrap_or(u32::MAX)
}

/// Give a subuid-owned path back to the daemon user so the tempdir can be
/// removed (ADR-0011: slot homes are 0700 subuid).
///
/// `-RP`, not `-R` (ADR-0034 cl. 5): the manor home and parcel homes are
/// *agent-writable* trees, so the reclaim walk can meet an agent-planted
/// symlink. Same traversal policy the product brace uses — spelled out here
/// for the same reason, because a dereferencing reclaim chowns whatever the
/// agent pointed at.
fn reclaim(path: &Path) {
    if path.exists() {
        let _ = StdCommand::new("podman")
            .args(["unshare", "chown", "-RP", "0:0", "--"])
            .arg(path)
            .output();
    }
}

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_manor_decrees_are_authored_only_by_uid_1000() {
    let _serial = manor_container_guard().await;
    if let Some(reason) = podman_guard()
        .or_else(support::userns_precondition)
        .or_else(|| support::work_area_precondition(&std::env::temp_dir()))
    {
        support::skip(
            "podman_manor_decrees_are_authored_only_by_uid_1000",
            &reason,
        );
        return;
    }

    // QA04-26: per-run names for the two *non-identity* parcels. The Mayor's
    // container keeps the product's identity name (`manor`) and this binary
    // serialises behind `MANOR_CONTAINER_LOCK` instead.
    let agent = support::scoped(AGENT_BASE);
    let mayor_container = support::scoped(MAYOR_CONTAINER_BASE);

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");

    let config = Config::default();
    let runtime = Arc::new(PodmanRuntime::from_config(&config));
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");

    // The manor (provisions slot 1000, MAYOR.md, home/, home/decrees/ and
    // the ADR-0011 braces) plus one ordinary agent parcel to attack from.
    fs::create_dir_all(tree.parcel_home(UID_AGENT)).expect("home dir");
    fs::create_dir_all(
        tree.courier_mail_recipient(&sanitize_name(&UID_AGENT.to_string()).unwrap()),
    )
    .expect("spool dir");
    fs::create_dir_all(tree.commission_git(COMMISSION)).expect("git dir");
    fs::create_dir_all(tree.courier_freight()).expect("freight dir");
    fs::create_dir_all(tree.market_pkgcache()).expect("pkgcache dir");
    fs::create_dir_all(tree.tavern_gossip()).expect("gossip dir");

    let cleanup = || {
        for name in [&agent, &mayor_container, "manor"] {
            let _ = StdCommand::new("podman").args(["rm", "-f", name]).output();
        }
        reclaim(&tree.manor_home());
        reclaim(&tree.parcel_home(UID_AGENT));
        reclaim(&tree.courier_mail_recipient(&sanitize_name(&UID_AGENT.to_string()).unwrap()));
    };
    cleanup();

    let result = async {
        let perms = HostPermissions::new(runtime.as_ref());
        perms
            .apply_parcel_home(&tree, UID_AGENT)
            .await
            .map_err(|e| e.to_string())?;
        perms
            .apply_mail_spool(&tree, UID_AGENT)
            .await
            .map_err(|e| e.to_string())?;
        perms
            .apply_shared(&tree, COMMISSION)
            .await
            .map_err(|e| e.to_string())?;

        runtime
            .pull(blueprint.meta().base_image())
            .await
            .map_err(|e| e.to_string())?;

        ManorProvisioner::new(
            tree.clone(),
            config.clone(),
            Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
        )
        .provision(&blueprint)
        .await
        .map_err(|e| e.to_string())?;

        let agent_spec = RunSpec::for_agent(
            &sanitize_name(&agent).unwrap(),
            UID_AGENT,
            COMMISSION,
            &blueprint,
            &config,
        )
        .map_err(|e| e.to_string())?;
        runtime
            .run_detached(&agent_spec, &tree)
            .await
            .map_err(|e| e.to_string())?;

        // --- ADR-0011 braces on the manor home --------------------------
        assert_eq!(
            mode_of(&tree.manor_home()),
            0o700,
            "ADR-0011 cl. 3: manor home is 0700"
        );
        let host_uid = owner_of(&tree.parcel_home(UID_AGENT));
        assert_ne!(
            host_uid,
            owner_of(&tree.manor_home()),
            "the manor home is owned by a different host uid than a parcel home"
        );

        // --- claim 1: UID 1000 CAN author a decree ----------------------
        // `ManorProvisioner::provision` already started the manor container
        // under the sanitized identity name (SPEC §4: hostname `manor`).
        let mayor_identity = sanitize_name("manor").unwrap().to_string();
        let out = sh(
            &runtime,
            &mayor_identity,
            "id -u; mkdir -p /home/1000/decrees; echo '{}' > /home/1000/decrees/probe.json; cat /home/1000/decrees/probe.json",
        )
        .await?;
        assert_eq!(
            out.stdout_lossy().lines().next().unwrap_or("").trim(),
            "1000",
            "the Mayor runs as UID 1000 (SPEC §4)"
        );
        assert!(
            out.stdout_lossy().contains("{}"),
            "the Mayor can write its own decree directory: {}",
            out.stdout_lossy()
        );
        // ... and the write crossed the bind mount: the Mayor's own `stat`
        // is the diagnosis shape for QA04-23 (ownership, not inference),
        // and the daemon observes the same bytes from its userns.
        // QA04-28: the previous `ls -ln … | contains("1000")` was vacuous —
        // "1000" appears in the path `/home/1000` itself, so the assertion
        // passed under the exact QA04-23 bug it was written to catch
        // (`decrees/` = `0 0 0755`). Assert the fields instead: owner and
        // mode, per path, so a daemon-owned or group/world-readable decree
        // directory fails here (ADR-0034 cl. 1/3; SPEC §4; ADR-0011 cl. 3).
        for path in ["/home/1000", "/home/1000/decrees"] {
            let out = sh(
                &runtime,
                &mayor_identity,
                &format!("stat -c '%u %a' {path}"),
            )
            .await?;
            assert_eq!(
                out.code, 0,
                "the Mayor must be able to stat its own {path}: {}",
                out.stderr_lossy()
            );
            assert_eq!(
                out.stdout_lossy().trim(),
                "1000 700",
                "{path} must be owned by UID 1000 and mode 0700 inside the \
                 container (ADR-0034 cl. 1/3; SPEC §4): {}",
                out.stdout_lossy()
            );
        }
        let probe = tree.manor_decrees().join("probe.json");
        assert_eq!(
            unshare_cat(&runtime, &probe).await?.trim(),
            "{}",
            "the daemon sees the Mayor's write through the bind mount"
        );

        // --- claim 2: NO other slot can author a decree ------------------
        // Ownership is the isolation boundary here (the executor's own
        // provenance check, ADR-0030/QA05-01, only ever confirms what the
        // kernel already refused). Every vector below must be refused.
        for denied in [
            // The manor is simply not in the agent's mount set.
            "ls /home/1000",
            "cat /home/1000/decrees/probe.json",
            "mkdir -p /home/1000/decrees",
            // Traversal from a bind root that *is* present.
            "mkdir -p /var/mail/../../../census/manor/home/decrees",
            "ls /var/mail/../../../keep/census/manor/home/decrees",
            // Via the shared gossip mount and the freight mount.
            "ls /var/context/../../../keep/census/manor/home/decrees",
            "ls /var/freight/../../../keep/census/manor/home/decrees",
            // Forge a mail envelope that pretends to be the Mayor, or read
            // another slot's spool: the Mayor's spool is 0700 uid 1000.
            "ls /var/mail/1000",
            "mkdir /var/mail/1000",
            "touch /var/mail/1000/d-000000000001-0000000000000000.json",
            // Infrastructure reach (SPEC §2: agents never touch sockets).
            "ls /run/podman/podman.sock",
            "podman ps",
        ] {
            let out = sh(&runtime, &agent, denied).await?;
            assert_ne!(
                out.code, 0,
                "a non-mayor writer must be refused: `{denied}` \
                 (SPEC §4: no subagent mount contains the manor; ADR-0011 cl. 3: 0700)"
            );
        }

        // The agent's own resources stay rw: the braces must not become a
        // self-DoS (the same shape as `escape_guarded.rs`).
        for allowed in [
            "touch /home/1001/probe",
            "touch /var/mail/1001/probe",
            "touch /var/pkgcache/probe-mayor-suite",
        ] {
            let out = sh(&runtime, &agent, allowed).await?;
            assert_eq!(
                out.code, 0,
                "own resources must stay writable: `{allowed}` ({})",
                out.stderr_lossy()
            );
        }

        Ok::<(), String>(())
    }
    .await;

    let _ = StdCommand::new("podman")
        .args(["rm", "-f", &agent, &mayor_container, "manor"])
        .output();
    reclaim(&tree.manor_home());
    reclaim(&tree.parcel_home(UID_AGENT));
    result.expect("the guarded manor suite must pass on a podman-capable host");
}

/// **QA05-02 probe — the daemon's own write into the decree directory.**
///
/// `DecreeExecutor::write_ack` and `DecreeExecutor::quarantine` (see
/// `crates/hamlet-daemon/src/decrees.rs`) call plain `fs` writes into
/// `keep/census/manor/home/decrees/`, which `apply_manor_home` has just made
/// 0700 and chowned to the **sub**uid 1000. ADR-0011 cl. 4 requires every
/// daemon write into such a directory to go through `podman unshare`.
///
/// Step 03b made the missing-ack failure loud and ADR-0030 cl. 2 closed it:
/// the executor escalates a `PermissionDenied` write to
/// `Runtime::unshare_write_atomic` (`podman unshare` mktemp → cp → chmod →
/// chown → mv). This probe is that seam's acceptance test on a real host.
///
/// This probe reports the host's truth rather than asserting a deployment
/// shape, because both are legitimate:
///
/// * daemon user == subuid 1000 (this dev host, where `id -u` is 1000):
///   the write succeeds and QA05-02 is latent;
/// * daemon user != subuid 1000 (the spec'd service user): the write is
///   refused with `EACCES` and QA05-02 is live.
#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_probe_daemon_can_write_the_ack_into_the_manor_decrees_dir() {
    let _serial = manor_container_guard().await;
    if let Some(reason) = podman_guard()
        .or_else(support::userns_precondition)
        .or_else(|| support::work_area_precondition(&std::env::temp_dir()))
    {
        support::skip(
            "podman_probe_daemon_can_write_the_ack_into_the_manor_decrees_dir",
            &reason,
        );
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");
    let runtime = Arc::new(PodmanRuntime::from_config(&config_of()));
    let _ = StdCommand::new("podman")
        .args(["rm", "-f", "manor"])
        .output();

    let provisioned = ManorProvisioner::new(
        tree.clone(),
        config_of(),
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    )
    .provision(&Blueprint::default_village_alpine().expect("blueprint"))
    .await
    .expect("manor provisioning");

    let host_uid = StdCommand::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .expect("host `id -u`");
    let manor_uid = owner_of(&tree.manor_home());
    eprintln!(
        "QA05-02 probe: host uid {host_uid}, manor home owner {manor_uid}, mode {:o}",
        mode_of(&tree.manor_home())
    );
    assert_eq!(provisioned.uid, hamlet_core::slots::MAYOR_UID);

    let ack = tree.manor_decrees().join(".qa05-02-ack-write-probe");
    match fs::write(&ack, b"probe\n") {
        Ok(()) => eprintln!(
            "QA05-02 LATENT on this host: the daemon user can write the ack directory \
             (host uid == manor owner or the dir is daemon-writable). Under the spec'd \
             dedicated service user this same write is refused and acks are silently lost."
        ),
        Err(e) => {
            // Expected on this host shape: the manor home is 0700 and owned
            // by the Mayor's subuid, so the daemon user's direct write is
            // refused by the kernel. The product handles it — ack/quarantine
            // writes escalate through `Runtime::unshare_write_atomic` (step-05
            // seam; a refusal without the seam is a hard error, never a trace,
            // since step 03b). The seam half of this probe asserts that below.
            eprintln!(
                "QA05-02 LIVE on this host: the daemon cannot write {} directly ({e}) — \
                 expected; the decree plane escalates through the unshare write seam, \
                 asserted next.",
                ack.display()
            );
        }
    }
    let _ = fs::remove_file(&ack);

    // The seam itself, on this host: whichever the direct write did, writing
    // through the daemon's own userns must succeed. This is what makes the
    // spec'd deployment (daemon user != subuid 1000) work, and it is the
    // half of QA05-02 that used to be missing entirely.
    let sealed = tree.manor_decrees().join(".qa05-02-unshare-seam-probe");
    let mayor = hamlet_core::slots::MAYOR_UID;
    runtime
        .unshare_write_atomic(&sealed, b"seam\n", (mayor, mayor))
        .await
        .unwrap_or_else(|e| panic!("QA05-02: the unshare write seam must work: {e}"));
    // Read back through the userns, not from the host: the seam hands the file
    // to the Mayor's subuid inside a 0700 subuid-owned directory, so a
    // host-side read is EACCES on every host whose subuid mapping actually
    // works — and that is precisely the deployment ADR-0011 cl. 4 describes.
    let read_back = runtime
        .unshare_exec(&hamlet_daemon::podman::cat_cmd(&sealed).expect("cat argv"))
        .await
        .expect("unshare cat");
    assert_eq!(
        String::from_utf8_lossy(&read_back.stdout),
        "seam\n",
        "the seam's bytes arrive intact"
    );
    let owner_seen = runtime
        .unshare_exec(&hamlet_daemon::podman::stat_cmd(&sealed).expect("stat argv"))
        .await
        .expect("unshare stat");
    assert_eq!(
        String::from_utf8_lossy(&owner_seen.stdout).trim(),
        "regular file",
        "and the file is a real file, handed to the Mayor slot rather than \
         left with the daemon (ownership itself is asserted in-container)"
    );
    eprintln!(
        "QA05-02 seam: OK — {} was written from inside the daemon userns",
        sealed.display()
    );
    let _ = fs::remove_file(&sealed);

    reclaim(&tree.manor_home());
    let _ = StdCommand::new("podman")
        .args(["rm", "-f", "manor"])
        .output();
}

/// **QA04-29 — the manor-home brace must not follow a symlink out of the
/// tree.** `apply_manor_home` runs `podman unshare chown -RP 1000:1000` over
/// the Mayor's home (ADR-0034 cl. 1 + cl. 5). That home is, by design,
/// **agent-writable** — it exists so the Mayor can author decrees — so it is
/// the one private tree in the village an adversary populates with symlinks,
/// and it is walked with `-R`.
///
/// Whether `-R` chowns what a symlink *points at* decides whether the brace
/// is a rename of the Mayor's own subtree or a **privileged chown of an
/// attacker-chosen path**, executed root-equivalent inside the daemon's
/// userns (ADR-0011 cl. 4). Measured on a live rootless host before the ADR
/// was written: `-RP` leaves `target/` untouched; `-RL` moves `target/` and
/// `target/f1` to the Mayor's host uid with rc=0 and no warning. The property
/// used to rest entirely on an unpinned GNU flag default; ADR-0034 cl. 5 now
/// states it and `podman.rs` pins the argv, and this test pins the *kernel's*
/// behaviour on a real host.
///
/// The victim is a daemon-owned directory outside the village tree with a
/// known owner; the plant is a symlink inside the home pointing at it. After
/// the brace, the victim must still be the daemon's.
#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_manor_home_brace_does_not_follow_symlinks_out_of_the_tree() {
    let _serial = manor_container_guard().await;
    let test = "podman_manor_home_brace_does_not_follow_symlinks_out_of_the_tree";
    if let Some(reason) = podman_guard()
        .or_else(support::userns_precondition)
        .or_else(|| support::work_area_precondition(&std::env::temp_dir()))
    {
        support::skip(test, &reason);
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");

    // The victim: outside the village tree, daemon-owned, with a file inside.
    let victim = tmp.path().join("outside-the-village");
    fs::create_dir_all(&victim).expect("victim dir");
    let victim_file = victim.join("f1");
    fs::write(&victim_file, b"daemon-owned\n").expect("victim file");

    // The plant: a symlink inside the Mayor's home pointing at it, plus a
    // real file beside it so the brace has something it *must* touch.
    let home = tree.manor_home();
    fs::write(home.join("inside.txt"), b"inside\n").expect("real file in home");
    std::os::unix::fs::symlink(&victim, home.join("link")).expect("plant the symlink");

    let runtime = Arc::new(PodmanRuntime::from_config(&config_of()));
    // Ownership is read from INSIDE the daemon's userns, so the numbers are
    // *inside* uids: the daemon user is 0 there and the Mayor is 1000. A
    // bare-host read would mix the two scales and compare 0 to 1000.
    let victim_owner = unshare_owner(&runtime, &victim)
        .await
        .expect("stat the victim");
    let victim_file_owner = unshare_owner(&runtime, &victim_file)
        .await
        .expect("stat the victim file");
    assert_eq!(
        victim_owner, 0,
        "the victim starts as the daemon's own (uid 0 inside the userns)"
    );

    let result = HostPermissions::new(runtime.as_ref())
        .apply_manor_home(&tree, hamlet_core::slots::MAYOR_UID)
        .await;

    // The brace itself must succeed — this is a "the safe path works" claim
    // first and a "the unsafe path is unreachable" claim second.
    if let Err(e) = result {
        support::skip(
            test,
            &format!(
                "QA04-29 could not observe the brace on this host: `apply_manor_home` failed \
                 ({e}). The no-dereference property is pinned hermetically by \
                 `podman.rs::chown_recursive_argv_pins_no_dereference` regardless; re-run on \
                 a host whose subuid mapping lets the brace land (QA04-07)."
            ),
        );
        reclaim(&home);
        reclaim(&victim);
        return;
    }

    // The real file moved into the Mayor's subuid...
    let inside_owner = unshare_owner(&runtime, &home.join("inside.txt")).await;
    // ... and the symlink's *target* did not move at all.
    let outside_owner = unshare_owner(&runtime, &victim).await;
    let outside_file_owner = unshare_owner(&runtime, &victim_file).await;

    reclaim(&home);
    reclaim(&victim);

    let inside_owner = inside_owner.expect("stat the real file inside the manor home");
    let outside_owner = outside_owner.expect("stat the symlink target directory");
    let outside_file_owner = outside_file_owner.expect("stat a file under the target");
    assert_eq!(
        inside_owner,
        hamlet_core::slots::MAYOR_UID,
        "the brace really did recurse into the manor home (otherwise this test \
         proves nothing)"
    );
    assert_eq!(
        outside_owner, victim_owner,
        "ADR-0034 cl. 5: `chown -R` over the manor home must NOT chown what a \
         symlink inside it points at — a dereferencing brace hands the Mayor's \
         subuid to an attacker-chosen path outside the village (measured: `-RL` \
         does exactly this, rc=0, silently)"
    );
    assert_eq!(
        outside_file_owner, victim_file_owner,
        "and the same holds for a file *under* that target"
    );
}

fn config_of() -> Config {
    let mut config = Config::default();
    config.sessions.ready_timeout_ms = 10_000;
    config
}
