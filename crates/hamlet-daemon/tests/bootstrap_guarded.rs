//! Guarded real-podman bootstrap suite (CONVENTIONS §5; SPEC §4 base image
//! and §5 blueprint execution; ADR-0012 image policy, ADR-0013 pkgcache
//! trust model).
//!
//! Runs only when `HAMLET_PODMAN_TESTS=1` **and** `podman` is on PATH;
//! otherwise it skips with a recorded reason. `#[ignore]`d so a plain
//! `cargo test` never touches podman; `scripts/ci.sh` step 5 runs it via
//! `cargo test --workspace -- --ignored podman` on capable hosts.
//!
//! # What this suite proves (step 04 verify contract)
//!
//! 1. **The derived image is slim and init-free** (SPEC §4): built from the
//!    blueprint's parent, carrying the apk tools + the bootstrap floor, with
//!    no ENTRYPOINT/CMD — the daemon supplies the keep-alive. GNU tar and xz
//!    are asserted present because extraction safety depends on them.
//! 2. **Provisioning from `defaults/village-alpine.toml` installs the three
//!    multiplexers into the parcel home**, and they RUN: `pixi --version`,
//!    `node --version` (through nvm's layout), `cargo --version` / `rustc
//!    --version` (through rustup's linked toolchain), `nvm --version`.
//!    Every check goes through `sh -lc`, i.e. the login-shell profile
//!    contract Sprint 03 must honour.
//! 3. **A second run of the pkgcache-dependent install performs ZERO network
//!    downloads**, asserted from the pkgcache hit instrumentation
//!    ([`BootstrapReport`]) in two independent ways: re-bootstrapping the
//!    same home is a verified no-op (`downloads == 0`, `steps_executed == 0`)
//!    and provisioning a SECOND parcel off the warm mirror downloads nothing
//!    (`cache_hits == 7`, `downloads == 0`) while still digest-verifying.
//! 4. **Isolation invariants survive bootstrap** (SPEC §4): `/var/git/` and
//!    `/var/context/` still ro, no cross-UID home reads, no infrastructure
//!    sockets, toolchain files owned by the slot UID (never root/daemon),
//!    and no secret from `.env` anywhere in a home (SPEC §11.4 — secrets are
//!    injected at session start, never baked in).
//!
//! # Cost
//!
//! The cold run downloads ~200 MB of pinned artifacts into the tempdir
//! village's mirror (rustc alone is ~100 MB compressed). That is the point of
//! the mirror: it happens once per village, and every later parcel is offline.

use std::fs;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
use hamlet_daemon::bootstrap::{BootstrapTarget, Bootstrapper, DEED_BASELINE_MARKER};
use hamlet_daemon::census::Provisioner;
use hamlet_daemon::podman::{ExecOutput, PodmanRuntime, Runtime as _};

const COMMISSION: u64 = 94;
const AGENT_A: &str = "hamlet-boot-a"; // slot 1001
const AGENT_B: &str = "hamlet-boot-b"; // slot 1002
const UID_A: u32 = 1001;
const UID_B: u32 = 1002;
/// Pinned versions from `defaults/toolchain-pins.toml` — the suite asserts
/// the *concrete* bytes the pin table froze, not a floating "latest".
const NODE_VERSION: &str = "v22.23.3";
const NVM_VERSION: &str = "0.40.8";
const RUST_VERSION: &str = "1.98.1";
const ARTIFACT_COUNT: usize = 7;
/// Obvious fake (CONVENTIONS §6): proves `.env` never reaches a home.
const FAKE_SECRET: &str = "test-key-do-not-use";

/// CONVENTIONS §5 guard: run only with `HAMLET_PODMAN_TESTS=1` and podman on
/// PATH; otherwise skip with a recorded reason.
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

/// One buffered `podman exec` against a guard container. Errors are Strings:
/// this suite mixes provisioning, bootstrap and podman failures in one flow.
async fn run(runtime: &PodmanRuntime, container: &str, cmd: &[&str]) -> Result<ExecOutput, String> {
    let argv: Vec<String> = cmd.iter().map(|s| (*s).to_owned()).collect();
    runtime
        .exec(container, &argv, None)
        .await
        .map_err(|e| e.to_string())
}

/// `podman exec <c> sh -lc '<script>'` — the login-shell path an agent session
/// takes, so the home profile is what makes the toolchain resolve.
async fn login(
    runtime: &PodmanRuntime,
    container: &str,
    script: &str,
) -> Result<ExecOutput, String> {
    run(runtime, container, &["sh", "-lc", script]).await
}

fn podman(args: &[&str]) -> String {
    let out = StdCommand::new("podman")
        .args(args)
        .output()
        .expect("podman must be runnable (guarded)");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "podman {args:?} failed (exit {:?}): {text}",
        out.status.code()
    );
    text
}

// ---------------------------------------------------------------------------
// 1. Derived image: slim, init-free, floor present (cheap — no provisioning)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_derived_image_is_slim_init_free_and_has_the_floor() {
    if let Some(reason) = podman_guard() {
        eprintln!("SKIP podman_derived_image_is_slim_init_free_and_has_the_floor: {reason}");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");
    let config = Config::default();
    let runtime = PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(3));
    let bootstrapper = Bootstrapper::new(&tree, &runtime).expect("committed pin table parses");

    let outcome = bootstrapper
        .ensure_image(&blueprint)
        .await
        .expect("derived image resolves");
    let tag = outcome.tag.clone();
    assert!(
        tag.starts_with("localhost/hamlet/village-alpine:v1-f1-c"),
        "localhost-scoped, versioned, floored, fingerprinted: {tag}"
    );

    // Idempotent: a second resolve is a probe, not a rebuild.
    let again = bootstrapper
        .ensure_image(&blueprint)
        .await
        .expect("second resolve");
    assert_eq!(again.tag, tag);
    assert!(!again.built, "the derived image is built at most once");

    // --- no init system, no entrypoint: the daemon owns the keep-alive ----
    let config_json = podman(&["image", "inspect", "--format", "{{json .Config}}", &tag]);
    for forbidden in ["systemd", "openrc", "runit", "s6-svscan", "/sbin/init"] {
        assert!(
            !config_json.contains(forbidden),
            "no init system in the image config (SPEC §4): {config_json}"
        );
    }
    let entrypoint = podman(&[
        "image",
        "inspect",
        "--format",
        "{{.Config.Entrypoint}}",
        &tag,
    ]);
    let cmd = podman(&["image", "inspect", "--format", "{{.Config.Cmd}}", &tag]);
    for value in [&entrypoint, &cmd] {
        let trimmed = value.trim();
        assert!(
            trimmed.is_empty() || trimmed == "[]" || trimmed == "<nil>" || trimmed == "<no value>",
            "the derived image declares no entrypoint/cmd (the daemon supplies \
             the keep-alive): {trimmed:?}"
        );
    }

    // --- the apk floor really is there (extraction + verification depend on it)
    let tar = podman(&["run", "--rm", &tag, "tar", "--version"]);
    assert!(
        tar.contains("GNU tar"),
        "the floor installs GNU tar: busybox tar's member handling is not what \
         bootstrap's traversal safety argument rests on — got {tar:?}"
    );
    for probe in [
        ["xz", "--version"],
        ["curl", "--version"],
        ["gzip", "--version"],
        ["setfacl", "--version"],
        ["git", "--version"],
        ["uutils", "--version"],
    ] {
        let mut argv = vec!["run", "--rm", tag.as_str()];
        argv.extend(probe);
        let out = podman(&argv);
        assert!(
            !out.trim().is_empty(),
            "{} present in the derived image",
            probe[0]
        );
    }
    // The digest tool is the trust anchor of the whole pkgcache model
    // (ADR-0013): check it FUNCTIONALLY, not by --version (busybox applets
    // have no --version). Alpine ships sha256sum as a busybox applet.
    let digest = podman(&["run", "--rm", &tag, "sha256sum", "/etc/alpine-release"]);
    let first = digest.split_whitespace().next().unwrap_or_default();
    assert_eq!(first.len(), 64, "sha256sum answers a digest: {digest:?}");
    assert!(
        first
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "lowercase hex, as the pin table records: {digest:?}"
    );
    // …and every other tool bootstrap's in-container argv names. (This one
    // probe uses `sh -c` for the loop; it is test-side image introspection,
    // never daemon code — the daemon's argv is shell-free by ADR-0010 cl. 1.)
    let out = podman(&[
        "run",
        "--rm",
        &tag,
        "sh",
        "-c",
        "for t in cp ln tee mkdir chmod rm cat stat env curl tar xz sha256sum; do          command -v $t >/dev/null || { echo missing:$t; exit 1; }; done; echo all-present",
    ]);
    assert!(
        out.contains("all-present"),
        "every tool bootstrap's argv names exists in the derived image: {out:?}"
    );

    // The parent image is a build input, present locally after ensure_image.
    assert_eq!(
        podman(&["image", "exists", blueprint.meta().base_image(),]).trim(),
        "",
        "`podman image exists` prints nothing and exits 0 when present"
    );
}

// ---------------------------------------------------------------------------
// 2. The full contract: toolchains install, run, and re-provision offline
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_bootstrap_installs_toolchains_and_reprovisions_with_zero_downloads() {
    if let Some(reason) = podman_guard() {
        eprintln!("SKIP podman_bootstrap_installs_toolchains_and_reprovisions_with_zero_downloads: {reason}");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");
    for agent in [AGENT_A, AGENT_B] {
        fs::write(
            tree.register_brief(&sanitize_name(agent).unwrap()),
            format!("# {agent}\n\nGuarded-bootstrap brief.\n"),
        )
        .expect("brief");
    }
    // A `.env` with an obvious fake: bootstrap must never read it, let alone
    // write it into a home (SPEC §11.4 — secrets are injected at session start).
    fs::write(
        tree.env_file(),
        format!("HAMLET_TEST_SECRET_KEY={FAKE_SECRET}\n"),
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

    // Whatever happens, the guard containers must not leak.
    let cleanup = || {
        for name in [AGENT_A, AGENT_B] {
            let _ = StdCommand::new("podman").args(["rm", "-f", name]).output();
        }
        // Give the tempdir back to the daemon user: parcel homes are 0700
        // subuid-owned, so an ordinary recursive delete would fail.
        for uid in [UID_A, UID_B] {
            let home = tree.parcel_home(uid);
            if home.exists() {
                let _ = StdCommand::new("podman")
                    .args(["unshare", "chown", "0:0", "--"])
                    .arg(&home)
                    .output();
            }
        }
    };
    cleanup();

    let result = async {
        // --- 1. provision parcel A: cold mirror, full pinned download ------
        let a = provisioner
            .provision_parcel_with_report(&sanitize_name(AGENT_A).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| e.to_string())?;
        assert!(
            a.image
                .tag
                .starts_with("localhost/hamlet/village-alpine:v1-f1-c"),
            "the parcel runs the DERIVED image, not the raw parent: {}",
            a.image.tag
        );
        assert!(!a.bootstrap.no_op, "an empty home installs");
        assert_eq!(a.bootstrap.cache_misses, ARTIFACT_COUNT, "cold mirror");
        assert_eq!(a.bootstrap.downloads, ARTIFACT_COUNT);
        assert_eq!(a.bootstrap.verified_artifacts.len(), ARTIFACT_COUNT);
        assert_eq!(
            a.bootstrap.probes.len(),
            3,
            "pixi, node and rust all probed: {:?}",
            a.bootstrap.probes
        );
        assert_eq!(a.bootstrap.manifest_sha256.len(), 64);
        assert!(
            a.bootstrap.publish_failures.is_empty(),
            "the first parcel populates the mirror: {:?}",
            a.bootstrap.publish_failures
        );

        // --- 2. the toolchains RUN inside, through the login profile -------
        for (script, expect) in [
            ("pixi --version", "pixi 0.34.0"),
            ("node --version", NODE_VERSION),
            ("nvm --version", NVM_VERSION),
            ("cargo --version", RUST_VERSION),
            ("rustc --version", RUST_VERSION),
        ] {
            let out = login(&runtime, AGENT_A, script).await?;
            assert_eq!(
                out.code,
                0,
                "`{script}` must work in a login shell (exit {}): {}",
                out.code,
                out.stderr_lossy()
            );
            assert!(
                out.stdout_lossy().contains(expect),
                "`{script}` printed {:?}, expected it to contain {expect:?}",
                out.stdout_lossy()
            );
        }
        // nvm resolves the pinned node from its own versions/ layout — i.e.
        // the runtime was materialized where the multiplexer expects it.
        let via_nvm = login(&runtime, AGENT_A, "nvm which 22").await?;
        assert_eq!(
            via_nvm.code,
            0,
            "nvm can resolve 22: {}",
            via_nvm.stderr_lossy()
        );
        assert!(
            via_nvm.stdout_lossy().contains(NODE_VERSION),
            "nvm points at the pinned runtime: {}",
            via_nvm.stdout_lossy()
        );

        // --- 3. the env layer is per-UID; caches stay shared --------------
        for (script, expect) in [
            ("echo $HOME", format!("/home/{UID_A}")),
            ("echo $CARGO_HOME", format!("/home/{UID_A}/.cargo")),
            ("echo $RUSTUP_HOME", format!("/home/{UID_A}/.rustup")),
            ("echo $PIXI_HOME", format!("/home/{UID_A}/.pixi")),
            ("echo $NVM_DIR", format!("/home/{UID_A}/.nvm")),
            ("echo $PIXI_CACHE_DIR", "/var/pkgcache/pixi".to_owned()),
            ("echo $HAMLET_BLUEPRINT", "village-alpine".to_owned()),
            ("echo $HAMLET_BLUEPRINT_VERSION", "1".to_owned()),
        ] {
            let out = login(&runtime, AGENT_A, script).await?;
            assert_eq!(out.stdout_lossy().trim(), expect, "{script}");
        }

        // --- 4. SPEC §11.4: no secret was baked into the home -------------
        let leaked = run(
            &runtime,
            AGENT_A,
            &["grep", "-rIl", FAKE_SECRET, "/home/1001"],
        )
        .await?;
        assert_ne!(
            leaked.code,
            0,
            "a `.env` secret must never reach a parcel home (it is injected at \
             session start, Sprint 03): {}",
            leaked.stdout_lossy()
        );
        let secret_exports = login(
            &runtime,
            AGENT_A,
            "grep -Ei '^export [A-Za-z_]*(KEY|SECRET|TOKEN|PASSW|CREDENTIAL)' /home/1001/.profile",
        )
        .await?;
        assert_ne!(
            secret_exports.code,
            0,
            "no secret-shaped export in the profile: {}",
            secret_exports.stdout_lossy()
        );

        // --- 5. the toolchain belongs to the slot UID, not root/daemon ----
        for path in [
            format!("/home/{UID_A}/.pixi/bin/pixi"),
            format!("/home/{UID_A}/.cargo/bin/cargo"),
            format!("/home/{UID_A}/.nvm/versions/node/{NODE_VERSION}/bin/node"),
            format!("/home/{UID_A}/.profile"),
            format!("/home/{UID_A}/.hamlet/toolchain.json"),
        ] {
            let owner = run(&runtime, AGENT_A, &["stat", "-c", "%u", &path]).await?;
            assert_eq!(
                owner.stdout_lossy().trim(),
                UID_A.to_string(),
                "{path} is owned by the slot uid inside the container"
            );
        }
        // …and the private staging dir is gone (~200 MB reclaimed per parcel).
        let staged = run(&runtime, AGENT_A, &["ls", "/home/1001/.hamlet/stage"]).await?;
        assert_ne!(staged.code, 0, "staging is dropped after extraction");

        // --- 6. SPEC §4: no systemd-class init, just the daemon's keep-alive
        let pid1 = run(&runtime, AGENT_A, &["cat", "/proc/1/cmdline"]).await?;
        assert_eq!(pid1.code, 0);
        assert!(
            pid1.stdout_lossy().contains("sleep"),
            "PID 1 is the slim keep-alive the daemon controls: {:?}",
            pid1.stdout_lossy()
        );

        // --- 7. isolation invariants still hold after bootstrap -----------
        for denied in ["touch /var/git/escape", "touch /var/context/escape"] {
            let out = login(&runtime, AGENT_A, denied).await?;
            assert_ne!(out.code, 0, "{denied} must fail (ro mount)");
        }
        // QA fix (F-02-02): probe SOCKET files, not `/var/run` (an Alpine
        // symlink to the always-present `/run` — see provision_guarded.rs).
        for path in [
            "/home/1002",
            "/run/podman/podman.sock",
            "/run/docker.sock",
            "/var/run/docker.sock",
            "/run/dbus/system_bus_socket",
        ] {
            let out = run(&runtime, AGENT_A, &["ls", path]).await?;
            assert_ne!(out.code, 0, "{path} must not be reachable");
        }
        let freight = run(&runtime, AGENT_A, &["ls", "/var/freight"]).await?;
        assert_ne!(
            freight.code, 0,
            "freight stays ACL-gated (0710) until Sprint 04"
        );

        // --- 8. the mirror holds every pinned artifact --------------------
        let mirrored = login(&runtime, AGENT_A, "ls -1 /var/pkgcache/dist | wc -l").await?;
        assert_eq!(
            mirrored.stdout_lossy().trim(),
            ARTIFACT_COUNT.to_string(),
            "every pinned artifact was published for the next parcel"
        );

        // --- 9. RE-RUN on the same home: verified no-op, zero downloads ----
        let bootstrapper = Bootstrapper::new(&tree, runtime.as_ref()).map_err(|e| e.to_string())?;
        let target_a = BootstrapTarget::new(sanitize_name(AGENT_A).unwrap(), UID_A).unwrap();
        let again = bootstrapper
            .bootstrap_home(&target_a, &blueprint)
            .await
            .map_err(|e| e.to_string())?;
        assert!(again.no_op, "a matching home is a verified no-op");
        assert_eq!(again.downloads, 0, "zero network on the no-op path");
        assert_eq!(again.steps_executed, 0, "no install step ran");
        assert_eq!(again.manifest_sha256, a.bootstrap.manifest_sha256);
        assert!(
            !again.deed_recorded,
            "the DEED baseline is recorded once, not per run"
        );
        assert!(again.probes["pixi"].contains("pixi"));
        assert!(again.probes["node"].contains(NODE_VERSION));
        assert!(again.probes["rust"].contains(RUST_VERSION));

        // --- 10. SECOND parcel off the warm mirror: zero downloads ---------
        let b = provisioner
            .provision_parcel_with_report(&sanitize_name(AGENT_B).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| e.to_string())?;
        assert!(!b.bootstrap.no_op, "a fresh home still installs");
        assert_eq!(
            b.bootstrap.downloads, 0,
            "pkgcache hit instrumentation: the second parcel downloads NOTHING"
        );
        assert_eq!(b.bootstrap.cache_hits, ARTIFACT_COUNT);
        assert_eq!(b.bootstrap.cache_misses, 0);
        assert_eq!(
            b.bootstrap.verified_artifacts.len(),
            ARTIFACT_COUNT,
            "cache hits are still digest-verified (the mirror is world-writable)"
        );
        assert_eq!(b.bootstrap.manifest_sha256.len(), 64);
        assert_ne!(
            b.bootstrap.manifest_sha256, a.bootstrap.manifest_sha256,
            "the manifest is per-UID (homes are per-slot)"
        );

        // B's toolchain works and is B's own.
        let out = login(&runtime, AGENT_B, "cargo --version").await?;
        assert!(
            out.stdout_lossy().contains(RUST_VERSION),
            "parcel B installed Rust from the mirror: {}",
            out.stdout_lossy()
        );
        let out = login(&runtime, AGENT_B, "node --version").await?;
        assert!(out.stdout_lossy().contains(NODE_VERSION));
        let owner = run(
            &runtime,
            AGENT_B,
            &["stat", "-c", "%u", "/home/1002/.cargo/bin/cargo"],
        )
        .await?;
        assert_eq!(owner.stdout_lossy().trim(), UID_B.to_string());
        // …and neither parcel can reach the other's toolchain.
        let cross = run(&runtime, AGENT_B, &["ls", "/home/1001"]).await?;
        assert_ne!(cross.code, 0, "cross-UID home read blocked");
        let cross = run(&runtime, AGENT_A, &["ls", "/home/1002"]).await?;
        assert_ne!(cross.code, 0, "cross-UID home read blocked both ways");
        // The mirror is shared by design — but only as a cache: B reads A's
        // published artifact, and a tampered copy would fail its digest pin.
        let shared = run(&runtime, AGENT_B, &["ls", "/var/pkgcache/dist"]).await?;
        assert_eq!(shared.code, 0, "the mirror is readable by every slot");

        // --- 11. graceful teardown through the runtime seam ---------------
        runtime
            .stop(AGENT_A, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        runtime
            .stop(AGENT_B, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }
    .await;

    // --- host-side records (asserted even when the container side failed) --
    for uid in [UID_A, UID_B] {
        let deed = fs::read_to_string(tree.parcel_deed(uid)).unwrap_or_default();
        assert!(
            deed.contains(DEED_BASELINE_MARKER),
            "parcel{uid} DEED.md carries the toolchain baseline (SPEC §9.4)"
        );
        assert_eq!(
            deed.matches(DEED_BASELINE_MARKER).count(),
            1,
            "exactly one baseline entry: {deed}"
        );
        assert!(
            !deed.contains(FAKE_SECRET),
            "DEED.md records env KEYS only, never values (SPEC §11.4)"
        );
        assert!(
            deed.contains(&format!(
                "- manifest: /home/{uid}/.hamlet/toolchain.json · sha256 "
            )),
            "the baseline identifies itself by manifest digest: {deed}"
        );
    }
    // The mirror is daemon-visible (01777 parent) and holds regular files.
    let dist = tree.market_pkgcache_dist();
    let entries: Vec<_> = fs::read_dir(&dist)
        .expect("mirror dir exists")
        .map(|e| e.expect("entry"))
        .collect();
    assert_eq!(
        entries.len(),
        ARTIFACT_COUNT,
        "one file per pinned artifact"
    );
    for entry in &entries {
        let meta = fs::symlink_metadata(entry.path()).unwrap();
        assert!(
            meta.file_type().is_file(),
            "{} is a regular file, never a symlink",
            entry.path().display()
        );
        assert!(
            meta.len() > 1024,
            "{} is not a stub",
            entry.path().display()
        );
    }

    cleanup();
    result.expect("guarded bootstrap: SPEC §4/§5 toolchain contract");
}
