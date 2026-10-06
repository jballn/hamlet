//! QA 02/05 — guarded real-podman isolation & escape suite (Sprint 02 step 05).
//!
//! Runs only when `HAMLET_PODMAN_TESTS=1` **and** `podman` is on PATH;
//! otherwise it skips with a recorded reason. `#[ignore]`d so a plain
//! `cargo test` never touches podman; `scripts/ci.sh` step 5 and
//! `scripts/e2e-02-isolation.sh` run it via `--ignored podman` on capable
//! hosts (CONVENTIONS §5).
//!
//! Extends — never duplicates — `provision_guarded.rs` (baseline ro/cross-UID
//! probes) and `bootstrap_guarded.rs` (toolchain + home secret scan) with the
//! step-05 adversarial angles:
//!
//! * **escape suite** (fast: parent image only, no toolchain downloads) —
//!   symlink-into-ro and hardlink-from-ro tricks, redirection through a
//!   shell, unprivileged remount attempts, spool forgery (`mkdir` in the
//!   0755 mail root), freight writes, the full infrastructure-socket matrix
//!   (podman/docker/dbus/systemd), no container tooling or host endpoint
//!   env, capability/NoNewPrivs state, `/proc/self/uid_map` analysis (the
//!   daemon's host uid must NOT fall inside the container's mapping), and
//!   per-slot host-uid distinctness (ADR-0011: home/spool owners differ per
//!   slot and never equal the daemon user).
//! * **bootstrap integrity suite** — `.env` secret material (obvious fake,
//!   CONVENTIONS §6) must appear in NO provisioned home, NO pkgcache mirror
//!   entry, NO derived-image layer (`podman history`/`inspect`), and no
//!   `Containerfile` may exist anywhere under the village tree (ADR-0012:
//!   the build context is daemon-private and outside the tree).
//!
//! SPEC citations: §4 (isolation invariants, UID policy, freight ACLs),
//! §11.4 (secrets); ADR-0010 (invocation policy), ADR-0011 (host permission
//! model + subuid userns), ADR-0012 (derived images), ADR-0013 (mirror).
//!
//! The escape suite additionally requires the QA04-07 host precondition (a
//! daemon user disjoint from the rootless userns, which the `0 -> <podman
//! owner>` mapping makes impossible when the test user owns the store); it is
//! checked up front and recorded as a SKIP with the host remedy rather than
//! failing an assertion that cannot hold on such a host. The bootstrap
//! integrity suite asserts content rather than uid policy, and stays ungated
//! on that axis — but it provisions a real parcel, so it does carry the
//! QA04-08 filesystem-capacity precondition. See `tests/support/mod.rs`.
//!
//! **Two harness facts this file owns (both measured, both from the QA04-26 /
//! QA04-27 re-gate):**
//!
//! * **Container names are per-run** (QA04-26): podman names are global to
//!   the store, so `qa-escape-a` etc. are suffixed with this process's pid
//!   (`support::scoped`). Two overlapping batteries no longer destroy each
//!   other's containers. The Mayor identity name is *not* scoped anywhere —
//!   that is the product's name (ADR-0028) and `manor_guarded` serialises
//!   for it instead.
//! * **The two suites serialise** (QA04-27): the integrity suite performs a
//!   full cold bootstrap (~1 GB extracted, 350–600 s) and running it beside
//!   another provisioning killed its container's PID 1 at ~353 s
//!   (`podman events`: `died`, unprompted). The symptom reached the gate log
//!   as `podman: no such container …` inside a **SPEC §11.4 secrets
//!   assertion** — a host shape wearing a product mask. Both suites therefore
//!   hold `support::COLD_BOOTSTRAP_LOCK` (the async-aware mutex seam
//!   `manor_guarded` uses; `bootstrap_guarded`'s cold-bootstrap test shares
//!   it), and a parcel found not running is reported through
//!   `support::skip` with the remedy, never as an assertion failure.

mod support;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::census::Provisioner;
use hamlet_daemon::hostperm::HostPermissions;
use hamlet_daemon::podman::{ExecOutput, PodmanRuntime, RunSpec, Runtime as _};

/// Obvious fake (CONVENTIONS §6) — never a real secret.
const FAKE_SECRET: &str = "test-key-do-not-use-QA02-escape";

/// CONVENTIONS §5 guard: run only with `HAMLET_PODMAN_TESTS=1` and podman
/// on PATH; otherwise skip with a recorded reason.
fn podman_guard() -> Option<String> {
    support::podman_env_guard()
}

async fn run(
    runtime: &PodmanRuntime,
    container: &str,
    cmd: &[&str],
) -> Result<ExecOutput, Failure> {
    let argv: Vec<String> = cmd.iter().map(|s| (*s).to_owned()).collect();
    runtime
        .exec(container, &argv, None)
        .await
        .map_err(|e| Failure::exec(container, &e.to_string()))
}

/// `sh -c` probe — TEST-side convenience only: product code never renders a
/// shell (ADR-0010 cl. 1); the escape suite uses it to model an adversarial
/// agent that DOES have a shell inside its parcel.
async fn sh(runtime: &PodmanRuntime, container: &str, script: &str) -> Result<ExecOutput, Failure> {
    run(runtime, container, &["sh", "-c", script]).await
}

/// How a suite in this binary ended — because "did it fail?" and "is the
/// product broken?" are different questions (QA04-27).
///
/// The doctrine, restated: **a non-compliant host must be a recorded SKIP
/// with a remedy, never a RED that reads like a product defect.** The
/// assertions themselves are never weakened — a compliant host runs them
/// verbatim, and `assert!` panics (a plain RED) exactly as before.
enum Failure {
    /// A real assertion/operation failure: the gate must see it.
    Product(String),
    /// A host shape the suite cannot assert on, with its remedy.
    HostPrecondition(String),
}

impl From<String> for Failure {
    fn from(text: String) -> Self {
        Failure::Product(text)
    }
}

impl Failure {
    /// A podman failure against a container this suite provisioned: if the
    /// parcel is gone or not running, that is QA04-27's host precondition,
    /// not a §11.4/§4 claim. Everything else stays loud.
    fn exec(container: &str, err: &str) -> Self {
        match support::dead_parcel_precondition(container, err) {
            Some(reason) => Failure::HostPrecondition(reason),
            None => Failure::Product(err.to_owned()),
        }
    }

    fn report(self, test: &str) {
        match self {
            Failure::Product(msg) => panic!("{test}: {msg}"),
            Failure::HostPrecondition(reason) => support::skip(test, &reason),
        }
    }
}

/// **QA04-27** — is the parcel this suite just provisioned still running? A
/// cold bootstrap can lose PID 1 mid-extraction on a host without RAM
/// headroom; checking *before* the first assertion keeps the reported failure
/// from being about secrets.
fn liveness_precondition(container: &str) -> Option<String> {
    match support::container_running(container) {
        Some(true) => None,
        Some(false) => Some(support::parcel_not_running_reason(container)),
        // podman cannot answer: the guarded guard already proved podman is
        // runnable, so an unreadable inspect is itself a host shape.
        None => Some(format!(
            "QA04-27 host precondition unverifiable: `podman inspect` could not report the \
             state of `{container}`, so a dead parcel cannot be told apart from a live one. \
             Remedy: re-run the guarded suite on a host where podman inspect works for a \
             just-provisioned container."
        )),
    }
}

/// **QA04-27** — the integrity suite's cold bootstrap is the single most
/// memory-hungry thing in the guarded battery (~1 GB of pinned toolchains
/// extracted inside a `--memory`-capped parcel). Running two of them in
/// parallel kills one of the parcels' PID 1 on hosts without headroom for
/// both, which is how QA04-27 reached the gate log as a SPEC §11.4 failure.
///
/// The lock itself is shared test infrastructure
/// (`support::COLD_BOOTSTRAP_LOCK`, the seam `manor_guarded` established in
/// QA04-20); both suites in this binary take it.
use support::COLD_BOOTSTRAP_LOCK as ESCAPE_PROVISIONING_LOCK;

fn host_uid() -> u32 {
    StdCommand::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .expect("host `id -u`")
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

/// Recursively give a subuid-owned path back to the daemon user so the
/// tempdir can be deleted (ADR-0011: slot homes/spools are 0700 subuid).
///
/// `-RP`, not `-R`: the tree being reclaimed is a *parcel home*, i.e. an
/// agent-writable directory that may contain agent-planted symlinks. The
/// traversal policy is the same one ADR-0034 cl. 5 pins for the product
/// brace, and it is spelled out here for the same reason — a dereferencing
/// reclaim chowns whatever the agent pointed at.
fn reclaim(path: &Path) {
    if path.exists() {
        let _ = StdCommand::new("podman")
            .args(["unshare", "chown", "-RP", "0:0", "--"])
            .arg(path)
            .output();
    }
}

const COMMISSION: u64 = 95;
/// Container/agent names are **per-run** (QA04-26): podman names are global
/// to the store, so two overlapping batteries used to provision each other's
/// containers and then fail with "container state improper" inside an
/// isolation assertion. These are the stable bases; `agent_a()` etc. are the
/// names actually used.
const AGENT_A_BASE: &str = "qa-escape-a"; // slot 1001
const AGENT_B_BASE: &str = "qa-escape-b"; // slot 1002
/// The bootstrap-integrity suite provisions its own parcel; it must NOT reuse
/// AGENT_A's container (QA04-20). Distinct base *and* a per-run suffix.
const INTEGRITY_AGENT_BASE: &str = "qa-escape-integrity"; // slot 1001
const UID_A: u32 = 1001;
const UID_B: u32 = 1002;

// ---------------------------------------------------------------------------
// 1. Escape suite: ro mounts, symlink/hardlink tricks, cross-UID, sockets,
//    capabilities, userns mapping (task items 1–4).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_escape_suite_ro_symlinks_cross_uid_no_infra_and_userns() {
    // QA04-27: one cold bootstrap at a time per binary. The escape arm is
    // cheap (parent image, no toolchain download), but it shares the store
    // and the host with the integrity arm and must not sit beside it.
    let _serial = ESCAPE_PROVISIONING_LOCK.lock().await;
    if let Some(reason) = podman_guard().or_else(support::userns_precondition) {
        support::skip(
            "podman_escape_suite_ro_symlinks_cross_uid_no_infra_and_userns",
            &reason,
        );
        return;
    }

    let agent_a = support::scoped(AGENT_A_BASE);
    let agent_b = support::scoped(AGENT_B_BASE);

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");

    let config = Config::default();
    let runtime = Arc::new(PodmanRuntime::from_config(&config));
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");

    // Mount sources + per-UID dirs, then the ADR-0011 permission braces.
    // The escape suite deliberately runs the PARENT image (no toolchain
    // bootstrap): isolation invariants must not depend on ~200 MB of tools.
    for uid in [UID_A, UID_B] {
        fs::create_dir_all(tree.parcel_home(uid)).expect("home dir");
        let spool = tree.courier_mail_recipient(&sanitize_name(&uid.to_string()).unwrap());
        fs::create_dir_all(&spool).expect("spool dir");
    }
    fs::create_dir_all(tree.commission_git(COMMISSION)).expect("git dir");
    fs::create_dir_all(tree.courier_freight()).expect("freight dir");
    fs::create_dir_all(tree.market_pkgcache()).expect("pkgcache dir");
    fs::create_dir_all(tree.tavern_gossip()).expect("gossip dir");
    // Read-only seed content: targets for the hardlink/symlink escape tries.
    fs::write(tree.commission_git(COMMISSION).join("seed.txt"), "ro\n").expect("git seed");
    fs::write(tree.tavern_gossip().join("seed.txt"), "ro\n").expect("gossip seed");

    let cleanup = || {
        for name in [&agent_a, &agent_b] {
            let _ = StdCommand::new("podman").args(["rm", "-f", name]).output();
        }
        for uid in [UID_A, UID_B] {
            reclaim(&tree.parcel_home(uid));
            reclaim(&tree.courier_mail_recipient(&sanitize_name(&uid.to_string()).unwrap()));
        }
    };
    cleanup();

    let result: Result<(), Failure> = async {
        let perms = HostPermissions::new(runtime.as_ref());
        for uid in [UID_A, UID_B] {
            perms
                .apply_parcel_home(&tree, uid)
                .await
                .map_err(|e| e.to_string())?;
            perms
                .apply_mail_spool(&tree, uid)
                .await
                .map_err(|e| e.to_string())?;
        }
        perms
            .apply_shared(&tree, COMMISSION)
            .await
            .map_err(|e| e.to_string())?;

        runtime
            .pull(blueprint.meta().base_image())
            .await
            .map_err(|e| e.to_string())?;
        for (agent, uid) in [(&agent_a, UID_A), (&agent_b, UID_B)] {
            let spec = RunSpec::for_agent(
                &sanitize_name(agent).unwrap(),
                uid,
                COMMISSION,
                &blueprint,
                &config,
            )
            .map_err(|e| e.to_string())?;
            runtime
                .run_detached(&spec, &tree)
                .await
                .map_err(|e| e.to_string())?;
        }
        // QA04-27: the isolation arms must never be reported against a
        // parcel that is not running — that failure text ("own resources
        // must stay writable") is how QA04-26's interference read as a
        // SPEC §4 defect.
        for name in [&agent_a, &agent_b] {
            if let Some(reason) = liveness_precondition(name) {
                return Err(Failure::HostPrecondition(reason));
            }
        }

        // --- task 4: userns mapping — inside uid == charter slot ----------
        let out = run(&runtime, &agent_a, &["id", "-u"]).await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            UID_A.to_string(),
            "A runs as its slot"
        );
        let out = run(&runtime, &agent_b, &["id", "-u"]).await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            UID_B.to_string(),
            "B runs as its slot"
        );

        // --- task 1: ro mounts, direct + every indirection we can model ---
        for denied in [
            "touch /var/git/escape",
            "mkdir /var/git/sub",
            "rm /var/git/seed.txt",
            "echo x > /var/git/redir",     // shell redirection
            "echo x > /var/context/redir", // the other ro mount
            "touch /var/context/escape",
            "rm /var/context/seed.txt",
            "ln /var/git/seed.txt /home/1001/hardlink", // hardlink OUT of ro
            "ln /var/context/seed.txt /home/1001/hardlink2",
            "ln -s /var/git /home/1001/gitlink && touch /home/1001/gitlink/escape", // symlink INTO ro
            "ln -s /var/context /home/1001/ctxlink && touch /home/1001/ctxlink/escape",
            "ln -s /var/git/seed.txt /home/1001/filelink && echo x > /home/1001/filelink",
            "mount -o remount,rw /var/git", // unprivileged remount
            "mount -o remount,rw /var/context",
            "chmod 777 /var/git",
            "chown 1001 /var/git/seed.txt",
        ] {
            let out = sh(&runtime, &agent_a, denied).await?;
            assert_ne!(
                out.code, 0,
                "ro escape attempt must fail: `{denied}` (SPEC §4: agents cannot write \
                 /var/git/ or /var/context/)"
            );
        }
        // The symlink itself may be created in the rw home — but writing
        // THROUGH it must have failed and left nothing on the ro side.
        let out = sh(&runtime, &agent_a, "ls /var/git /var/context").await?;
        assert!(
            !out.stdout_lossy().contains("escape"),
            "nothing landed in the ro mounts: {}",
            out.stdout_lossy()
        );

        // --- task 2: cross-UID home / spool / freight ----------------------
        for denied in [
            "ls /home/1002", // B's home: not mounted in A
            "cat /home/1002/anything",
            "ls /var/mail/1002", // B's spool: 0700 slot-owned
            "touch /var/mail/1002/forged",
            "cat /var/mail/1002/* 2>/dev/null; ls /var/mail/1002",
            "mkdir /var/mail/9999", // spool forgery in the 0755 root
            "touch /var/mail/rootfile",
            "ls /var/freight", // 01770 daemon, other::---: no slot root-ACL grant in this suite
            "mkdir /var/freight/sandbox",
            "touch /var/freight/x",
        ] {
            let out = sh(&runtime, &agent_a, denied).await?;
            assert_ne!(
                out.code, 0,
                "cross-UID/infra write must fail: `{denied}` (SPEC §4)"
            );
        }
        // Own resources stay rw (isolation must not become self-DoS).
        for allowed in [
            "touch /home/1001/probe",
            "touch /var/mail/1001/probe",
            "touch /var/pkgcache/probe-a",
        ] {
            let out = sh(&runtime, &agent_a, allowed).await?;
            assert_eq!(
                out.code,
                0,
                "own resource must stay rw: `{allowed}`: {}",
                out.stderr_lossy()
            );
        }
        // Mirror from B.
        for denied in [
            "ls /home/1001",
            "ls /var/mail/1001",
            "touch /var/mail/1001/forged",
        ] {
            let out = sh(&runtime, &agent_b, denied).await?;
            assert_ne!(out.code, 0, "B→A must fail: `{denied}`");
        }

        // --- task 3: no infrastructure inside ------------------------------
        for sock in [
            "/run/podman/podman.sock",
            "/run/podman",
            "/run/docker.sock",
            "/var/run/docker.sock",
            "/run/dbus/system_bus_socket",
            "/var/run/dbus/system_bus_socket",
            "/run/dbus",
            "/run/systemd/private",
        ] {
            let out = run(&runtime, &agent_a, &["ls", sock]).await?;
            assert_ne!(
                out.code, 0,
                "host infrastructure {sock} must not exist inside (SPEC §4)"
            );
        }
        let out = sh(&runtime, &agent_a, "command -v podman docker nerdctl ctl").await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            "",
            "no container tooling inside the parcel"
        );
        let out = sh(
            &runtime,
            &agent_a,
            "printenv DOCKER_HOST CONTAINER_HOST DOCKER_SOCKET",
        )
        .await?;
        assert_eq!(out.stdout_lossy().trim(), "", "no host endpoint env inside");

        // --- task 3/4: privilege state of the session process --------------
        let out = sh(
            &runtime,
            &agent_a,
            "grep -E '^(CapEff|CapPrm|NoNewPrivs|Seccomp):' /proc/self/status",
        )
        .await?;
        let status = out.stdout_lossy();
        assert!(
            status.contains("CapEff:\t0000000000000000"),
            "cap-drop=ALL must leave zero effective caps: {status}"
        );
        assert!(
            status.contains("CapPrm:\t0000000000000000"),
            "no permitted caps either: {status}"
        );
        assert!(
            status.contains("NoNewPrivs:\t1"),
            "no-new-privileges must be active: {status}"
        );

        // --- task 4: the private userns must not contain the daemon user ---
        let daemon_uid = host_uid();
        let out = sh(&runtime, &agent_a, "cat /proc/self/uid_map").await?;
        let map = out.stdout_lossy();
        let mut covers_slot: Option<(u64, u64)> = None;
        for line in map.lines() {
            let f: Vec<u64> = line
                .split_whitespace()
                .map(|t| t.parse().unwrap_or(u64::MAX))
                .collect();
            assert_eq!(f.len(), 3, "uid_map extent: {line}");
            let (inside, host, len) = (f[0], f[1], f[2]);
            assert_ne!(
                host, 0,
                "rootful identity mapping is forbidden (ADR-0011): {map}"
            );
            if inside <= u64::from(UID_A) && u64::from(UID_A) < inside + len {
                covers_slot = Some((inside, host));
            }
            // The daemon's own host uid must not fall inside the *subuid*
            // mapping — policy IS the disjoint subuid range.
            //
            // The single-uid owner row (`0 -> <invoking uid> 1`) is exempt and
            // not negotiable: rootless podman maps container root onto the
            // invoking user in every container it ever starts, so demanding
            // disjointness from that row made the assertion unsatisfiable for
            // ANY rootless host (QA04-07 restated). What ADR-0011 cl. 2
            // actually forbids is a *slot* mapping back onto the daemon, and
            // that is what the row below plus the host-ownership braces
            // assert.
            if inside == 0 && len == 1 {
                assert_eq!(
                    host,
                    u64::from(daemon_uid),
                    "the owner row maps container root onto the invoking user \
                     (rootless podman, ADR-0010): {map}"
                );
                continue;
            }
            assert!(
                u64::from(daemon_uid) < host || u64::from(daemon_uid) >= host + len,
                "daemon uid {daemon_uid} collides with the container's subuid mapping: {map}"
            );
        }
        let (slot_inside, slot_host) = covers_slot.expect("slot uid is mapped");
        assert_ne!(
            slot_host + u64::from(UID_A) - slot_inside,
            u64::from(daemon_uid),
            "the slot's host uid must not be the daemon user"
        );

        // Host-side: per-slot uids are DISTINCT and never the daemon user;
        // daemon-owned shared dirs stay daemon-owned (the "braces").
        let home_a = owner_of(&tree.parcel_home(UID_A));
        let home_b = owner_of(&tree.parcel_home(UID_B));
        let spool_a = owner_of(&tree.courier_mail_recipient(&sanitize_name("1001").unwrap()));
        let spool_b = owner_of(&tree.courier_mail_recipient(&sanitize_name("1002").unwrap()));
        assert_ne!(
            home_a, daemon_uid,
            "home A is subuid-owned, not daemon-owned"
        );
        assert_ne!(
            home_b, daemon_uid,
            "home B is subuid-owned, not daemon-owned"
        );
        assert_ne!(home_a, home_b, "slots never share a host uid (ADR-0011)");
        assert_eq!(
            home_a, spool_a,
            "slot A's home and spool share its host uid"
        );
        assert_eq!(
            home_b, spool_b,
            "slot B's home and spool share its host uid"
        );
        assert_eq!(
            u64::from(home_a),
            slot_host + u64::from(UID_A) - slot_inside,
            "host owner == uid_map(slot) — the mapping really is the subuid range"
        );
        for shared in [
            tree.commission_git(COMMISSION),
            tree.tavern_gossip(),
            tree.courier_freight(),
            tree.courier_mail(),
            tree.market_pkgcache(),
        ] {
            assert_eq!(
                owner_of(&shared),
                daemon_uid,
                "{} stays daemon-owned",
                shared.display()
            );
        }
        // The mapped UID cannot write the ro host dirs directly either
        // (task 1 "perms braces"): 0755 daemon-owned ⇒ "other" has no w.
        assert_eq!(mode_of(&tree.commission_git(COMMISSION)), 0o755);
        assert_eq!(mode_of(&tree.tavern_gossip()), 0o755);
        assert_eq!(mode_of(&tree.courier_freight()), 0o1770);
        assert_eq!(mode_of(&tree.courier_mail()), 0o755);
        assert_eq!(mode_of(&tree.parcel_home(UID_A)), 0o700);
        assert_eq!(
            mode_of(&tree.courier_mail_recipient(&sanitize_name("1001").unwrap())),
            0o700
        );

        runtime
            .stop(&agent_a, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        runtime
            .stop(&agent_b, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), Failure>(())
    }
    .await;

    cleanup();
    match result {
        Ok(()) => {}
        Err(failure) => {
            failure.report("podman_escape_suite_ro_symlinks_cross_uid_no_infra_and_userns")
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Bootstrap integrity: no .env secret material in home, mirror, or image
//    layers (task item 7).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_bootstrap_integrity_no_secrets_in_home_mirror_or_image_layers() {
    // QA04-27: this arm is the battery's one cold bootstrap. Serialised with
    // its sibling so a peer provisioning run cannot take PID 1 out from under
    // it — the failure mode that made a dead container read as a SPEC §11.4
    // secrets violation.
    let _serial = ESCAPE_PROVISIONING_LOCK.lock().await;
    // This suite provisions a real parcel, so it carries the QA04-08 capacity
    // precondition: extracting the pinned rustc into a RAM-backed TMPDIR dies
    // with exit 137, which is an OOM wearing a tar costume — not a defect.
    if let Some(reason) =
        podman_guard().or_else(|| support::work_area_precondition(&std::env::temp_dir()))
    {
        support::skip(
            "podman_bootstrap_integrity_no_secrets_in_home_mirror_or_image_layers",
            &reason,
        );
        return;
    }

    let agent = support::scoped(INTEGRITY_AGENT_BASE);
    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");
    fs::write(
        tree.register_brief(&sanitize_name(&agent).unwrap()),
        format!("# {agent}\n\nQA integrity brief.\n"),
    )
    .expect("brief");
    fs::write(
        tree.env_file(),
        format!("HAMLET_QA_SECRET_KEY={FAKE_SECRET}\n"),
    )
    .expect(".env fixture");

    let config = Config::default();
    let runtime =
        Arc::new(PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(3)));
    let provisioner = Provisioner::new(
        tree.clone(),
        config,
        Arc::clone(&runtime) as Arc<dyn hamlet_daemon::podman::Runtime>,
    );
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");

    let cleanup = || {
        let _ = StdCommand::new("podman")
            .args(["rm", "-f", &agent])
            .output();
        reclaim(&tree.parcel_home(UID_A));
        reclaim(&tree.courier_mail_recipient(&sanitize_name("1001").unwrap()));
    };
    cleanup();

    let result: Result<(), Failure> = async {
        let provisioned = provisioner
            .provision_parcel_with_report(&sanitize_name(&agent).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| e.to_string())?;
        let tag = provisioned.image.tag.clone();

        // --- QA04-27: the parcel must be alive before any §11.4 claim -----
        // A cold bootstrap can lose PID 1 mid-extraction on a host without
        // RAM headroom (measured: at ~353 s, unprompted). Probing *here* —
        // between provisioning and the first secret assertion — is what keeps
        // a dead container from being reported as a secrets leak.
        if let Some(reason) = liveness_precondition(&agent) {
            return Err(Failure::HostPrecondition(reason));
        }

        // --- inside the parcel: home + shared mirror carry no secret -------
        let out = sh(
            &runtime,
            &agent,
            &format!("grep -rIl '{FAKE_SECRET}' /home/{UID_A} /var/pkgcache 2>/dev/null"),
        )
        .await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            "",
            "secret material found in the home or the mirror (SPEC §11.4)"
        );
        let out = sh(
            &runtime,
            &agent,
            &format!("find /home/{UID_A} /var/pkgcache -name '*.env*'"),
        )
        .await?;
        assert_eq!(
            out.stdout_lossy().trim(),
            "",
            "no .env-shaped file in home/mirror"
        );

        // --- image layers: history + config env carry no secret ------------
        let podman_out = |args: &[&str]| -> String {
            let o = StdCommand::new("podman")
                .args(args)
                .output()
                .expect("podman");
            assert!(
                o.status.success(),
                "podman {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        };
        let history = podman_out(&["history", "--no-trunc", "--format", "{{.CreatedBy}}", &tag]);
        assert!(
            !history.contains(FAKE_SECRET),
            "secret baked into an image layer (SPEC §11.4): {history}"
        );
        let inspect = podman_out(&["inspect", "--format", "{{json .Config}}", &tag]);
        assert!(
            !inspect.contains(FAKE_SECRET),
            "secret in image config: {inspect}"
        );
        assert!(
            !inspect.contains(".env"),
            "image config references .env: {inspect}"
        );

        // --- host side: no Containerfile under the village tree ------------
        // ADR-0011 cl. 2: parcel homes and mail spools are 0700 **subuid**-
        // owned, so the daemon's host user cannot list them — that is the
        // isolation invariant, not a gap in this scan. Those subtrees are
        // agent-private and are never a build context (ADR-0012 keeps the
        // Containerfile outside the tree entirely), so the "no Containerfile
        // in the village tree" claim is about the daemon-visible parts, which
        // are all daemon-owned and fully scanned. EACCES is therefore the one
        // error this walk tolerates; anything else is still loud.
        fn find_containerfile(dir: &Path, hits: &mut Vec<String>) {
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
                Err(e) => panic!("read {}: {e}", dir.display()),
            };
            for entry in entries {
                let entry = entry.unwrap();
                let path = entry.path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    find_containerfile(&path, hits);
                } else if path
                    .file_name()
                    .map(|n| n == "Containerfile")
                    .unwrap_or(false)
                {
                    hits.push(path.display().to_string());
                }
            }
        }
        let mut hits = Vec::new();
        find_containerfile(tree.root(), &mut hits);
        assert!(
            hits.is_empty(),
            "a Containerfile inside the village tree is root-fs-as-code for \
             any agent (ADR-0012: private 0700 context OUTSIDE the tree): {hits:?}"
        );

        // --- persisted village files carry no secret (charter/DEED/…) ------
        fn scan(dir: &Path, hits: &mut Vec<String>) {
            // Same tolerance as find_containerfile: an agent-private 0700
            // subuid-owned subtree is unreadable by design; the SPEC §11.4
            // claim is about files the daemon persists outside `.env`.
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
                Err(e) => panic!("read {}: {e}", dir.display()),
            };
            for entry in entries {
                let entry = entry.unwrap();
                let path = entry.path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    scan(&path, hits);
                } else if meta.is_file() && path.file_name().unwrap() != ".env" {
                    if let Ok(text) = fs::read_to_string(&path) {
                        if text.contains(FAKE_SECRET) {
                            hits.push(path.display().to_string());
                        }
                    }
                }
            }
        }
        let mut secret_hits = Vec::new();
        scan(tree.root(), &mut secret_hits);
        assert!(
            secret_hits.is_empty(),
            "secret persisted outside .env: {secret_hits:?} (SPEC §11.4)"
        );

        runtime
            .stop(&agent, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), Failure>(())
    }
    .await;

    cleanup();
    match result {
        Ok(()) => {}
        Err(failure) => {
            failure.report("podman_bootstrap_integrity_no_secrets_in_home_mirror_or_image_layers")
        }
    }
}
