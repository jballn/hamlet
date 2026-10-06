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
//! # Host preconditions
//!
//! Two Sprint-04 findings shape this suite. **QA04-08**: the cold run writes
//! ~200 MB of archives plus ~1 GB of extracted toolchains into the tempdir
//! village, so the tree's filesystem must have room (a RAM-backed `TMPDIR`
//! dies as `exit 137`). **QA04-09**: SPEC §4 mounts the parcel home and the
//! pkgcache mirror as two separate bind mounts; when they land on different
//! host devices the ADR-0013 cl. 3 hardlink publish is EXDEV — explicitly
//! non-fatal, "it is a cache" — so the mirror legitimately stays empty and
//! the zero-download claim does not apply. Both are checked up front: the
//! first skips with the host remedy, the second *branches* on the measured
//! device pair and asserts the claim each host shape actually supports.
//! See `tests/support/mod.rs`.
//!
//! # Cost
//!
//! The cold run downloads ~200 MB of pinned artifacts into the tempdir
//! village's mirror (rustc alone is ~100 MB compressed). That is the point of
//! the mirror: it happens once per village, and every later parcel is offline.

mod support;

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
use hamlet_daemon::podman::{ExecOutput, PodmanRuntime, Runtime};

const COMMISSION: u64 = 94;
/// Container names are **per-run** (QA04-26): podman names are global to the
/// store, so two overlapping batteries provisioned each other's containers and
/// then failed inside an isolation assertion with text that reads like a
/// SPEC §4 defect. The constants are the stable *bases*; every container,
/// brief and cleanup below uses the pid-scoped name.
const AGENT_A_BASE: &str = "hamlet-boot-a"; // slot 1001
const AGENT_B_BASE: &str = "hamlet-boot-b"; // slot 1002
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
    support::podman_env_guard()
}

/// One buffered `podman exec` against a guard container. Errors are Strings:
/// this suite mixes provisioning, bootstrap and podman failures in one flow.
/// Generic over the runtime so the publish-arm seam (ADR-0035) can wrap it.
async fn run<R: Runtime>(runtime: &R, container: &str, cmd: &[&str]) -> Result<ExecOutput, String> {
    let argv: Vec<String> = cmd.iter().map(|s| (*s).to_owned()).collect();
    runtime
        .exec(container, &argv, None)
        .await
        .map_err(|e| e.to_string())
}

/// `podman exec <c> sh -lc '<script>'` — the login-shell path an agent session
/// takes, so the home profile is what makes the toolchain resolve.
async fn login<R: Runtime>(
    runtime: &R,
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
        tag.starts_with("localhost/hamlet/village-alpine:v1-f2-c"),
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
// 1b. QA04-05/06 regressions, verified against a REAL built image
// ---------------------------------------------------------------------------

/// QA04-05: the pinned musl toolchain's shared libraries are really in the
/// image, are root-owned and unwritable by a slot UID, and the loader
/// resolves them.
///
/// These two `.so`s are the whole difference between "every parcel's node
/// probe dies at the dynamic loader with exit 127" and a working toolchain,
/// and they are now *in every parcel's* root filesystem — so the security
/// question is not "are they present" but "can an agent tamper with them".
#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_musl_runtime_libs_present_root_owned_and_not_agent_writable() {
    if let Some(reason) = podman_guard() {
        eprintln!(
            "SKIP podman_musl_runtime_libs_present_root_owned_and_not_agent_writable: {reason}"
        );
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");
    let config = Config::default();
    let runtime = PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(3));
    let tag = Bootstrapper::new(&tree, &runtime)
        .expect("committed pin table parses")
        .ensure_image(&blueprint)
        .await
        .expect("derived image resolves")
        .tag;

    // Present AND resolvable by the loader. `readelf` is absent in alpine, so
    // resolve through the loader itself: ldd of a linked binary is the real
    // question, and `node`'s DT_NEEDED set is {libstdc++, libgcc_s, musl}.
    let libs = podman(&[
        "run",
        "--rm",
        &tag,
        "sh",
        "-c",
        "set -e; for l in /usr/lib/libstdc++.so.6 /usr/lib/libgcc_s.so.1; do \
         p=$(readlink -f $l); [ -r $p ] || { echo \"unreadable:$p\"; exit 1; }; \
         echo \"$p\"; done; echo libs-ok",
    ]);
    assert!(
        libs.contains("libs-ok"),
        "the musl runtime libraries resolve inside the image (QA04-05): {libs:?}"
    );

    // Root-owned, and NOT writable by anyone but root: the container user is
    // the slot UID and `--cap-drop=ALL` means it cannot chown them back.
    let stats = podman(&[
        "run",
        "--rm",
        "--user",
        "1001:1001",
        &tag,
        "sh",
        "-c",
        "for l in /usr/lib/libstdc++.so.6 /usr/lib/libgcc_s.so.1; do \
         stat -c '%u %a' $(readlink -f $l); done",
    ]);
    let mut seen = 0;
    for line in stats.lines().filter(|l| !l.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let owner: u32 = fields.next().expect("uid").parse().expect("numeric uid");
        let mode = fields.next().expect("mode");
        assert_eq!(
            owner, 0,
            "runtime library is root-owned inside the image: {line}"
        );
        assert!(
            !mode.ends_with('7')
                && !mode.ends_with('6')
                && !mode.ends_with('3')
                && !mode.ends_with('2'),
            "no group/other write bit on a runtime library: {mode}"
        );
        seen += 1;
    }
    assert_eq!(seen, 2, "both libraries inspected: {stats:?}");

    // ADVERSARIAL: a slot UID cannot replace, shadow or rewrite them. Any of
    // these succeeding would be a privilege escalation via the toolchain.
    for attempt in [
        "rm -f /usr/lib/libstdc++.so.6",
        "ln -sf /tmp/evil.so /usr/lib/libstdc++.so.6",
        "cp /bin/busybox /usr/lib/libgcc_s.so.1",
        "mkdir -p /usr/lib && chmod 0777 /usr/lib",
    ] {
        let out = StdCommand::new("podman")
            .args([
                "run",
                "--rm",
                "--user",
                "1001:1001",
                &tag,
                "sh",
                "-c",
                attempt,
            ])
            .output()
            .expect("podman must be runnable (guarded)");
        assert!(
            !out.status.success(),
            "slot uid must not be able to `{attempt}` a runtime library: \
             stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // …and the originals are intact afterwards (a failed rm must not have
    // unlinked them on the way out).
    let intact = StdCommand::new("podman")
        .args([
            "run",
            "--rm",
            &tag,
            "sh",
            "-c",
            "test -r /usr/lib/libstdc++.so.6",
        ])
        .status()
        .expect("podman must be runnable (guarded)");
    assert!(intact.success(), "libstdc++ survives the attack attempts");
}

/// QA04-06: the derived image inherits **no body** from its parent.
///
/// `alpine:3.24` upstream declares `CMD ["/bin/sh"]`, and the generated
/// Containerfile used to merely omit CMD — so every real parcel image
/// inherited an interactive root shell. The daemon never passes one (it always
/// appends an explicit command), which is why nothing broke; but the pinned
/// invariant "the derived image declares no entrypoint/cmd" was false for every
/// image built since Sprint 02.
#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_derived_image_inherits_no_body_from_its_parent() {
    if let Some(reason) = podman_guard() {
        eprintln!("SKIP podman_derived_image_inherits_no_body_from_its_parent: {reason}");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");
    let blueprint = Blueprint::default_village_alpine().expect("default blueprint");
    let config = Config::default();
    let runtime = PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(3));
    let tag = Bootstrapper::new(&tree, &runtime)
        .expect("committed pin table parses")
        .ensure_image(&blueprint)
        .await
        .expect("derived image resolves")
        .tag;

    // Premise: the parent really does declare a body. If upstream ever drops
    // it, this test keeps passing vacuously — so pin the premise explicitly.
    let parent_cmd = podman(&[
        "image",
        "inspect",
        "--format",
        "{{json .Config.Cmd}}",
        blueprint.meta().base_image(),
    ]);
    assert!(
        !parent_cmd.trim().is_empty() && parent_cmd.trim() != "[]" && parent_cmd.trim() != "null",
        "the parent declares a CMD — that is exactly what must not be inherited: {parent_cmd:?}"
    );

    for field in ["{{.Config.Cmd}}", "{{.Config.Entrypoint}}"] {
        let value = podman(&["image", "inspect", "--format", field, &tag]);
        let trimmed = value.trim();
        assert!(
            trimmed.is_empty() || trimmed == "[]" || trimmed == "<nil>" || trimmed == "<no value>",
            "{field} of the derived image must be empty, not inherited: {trimmed:?}"
        );
    }

    // FAIL-CLOSED, not merely quiet: running the image with NO command must
    // ERROR rather than silently start the inherited `/bin/sh`. This is the
    // direction the clearing was chosen for.
    let bare = StdCommand::new("podman")
        .args(["run", "--rm", &tag])
        .output()
        .expect("podman must be runnable (guarded)");
    assert!(
        !bare.status.success(),
        "a commandless run must fail instead of inheriting a shell body: \
         stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&bare.stdout),
        String::from_utf8_lossy(&bare.stderr)
    );
    let stderr = String::from_utf8_lossy(&bare.stderr);
    assert!(
        !stderr.contains("interactive"),
        "the failure is the missing command, not a started shell: {stderr:?}"
    );

    // …and the daemon's own path still works: an explicit command runs fine,
    // including the keep-alive shape bootstrap actually uses.
    let keep_alive = StdCommand::new("podman")
        .args([
            "run",
            "--rm",
            "--name",
            "hamlet-cmd-probe",
            &tag,
            "sleep",
            "0.1",
        ])
        .status()
        .expect("podman must be runnable (guarded)");
    assert!(keep_alive.success(), "an explicit command still runs");
    let _ = StdCommand::new("podman")
        .args(["rm", "-f", "hamlet-cmd-probe"])
        .status();
    let detached = StdCommand::new("podman")
        .args([
            "run",
            "-d",
            "--name",
            "hamlet-keepalive-probe",
            &tag,
            "sleep",
            "infinity",
        ])
        .status()
        .expect("podman must be runnable (guarded)");
    assert!(
        detached.success(),
        "the SPEC §4 keep-alive body (sleep infinity) still starts"
    );
    let _ = StdCommand::new("podman")
        .args(["rm", "-f", "hamlet-keepalive-probe"])
        .status();
}

// ---------------------------------------------------------------------------
// 2. The full contract: toolchains install, run, and re-provision offline
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_bootstrap_installs_toolchains_and_reprovisions_with_zero_downloads() {
    // QA04-27: the guarded battery's single most memory-hungry step (~1 GB
    // of pinned toolchains extracted inside a `--memory`-capped parcel).
    // Running it beside another provisioning killed a parcel's PID 1 on hosts
    // without headroom for both, and the failure reached the gate log wearing
    // an isolation assertion. Serialised per binary with `escape_guarded`;
    // across batteries it is the CI layer's flock.
    let _serial = support::COLD_BOOTSTRAP_LOCK.lock().await;
    if let Some(reason) = podman_guard()
        .or_else(support::userns_precondition)
        .or_else(|| support::work_area_precondition(&std::env::temp_dir()))
    {
        support::skip(
            "podman_bootstrap_installs_toolchains_and_reprovisions_with_zero_downloads",
            &reason,
        );
        return;
    }

    // QA04-26: podman container names are global to the store, so a second
    // battery running this suite on the same host would otherwise adopt (and
    // then `rm -f`) this run's parcels. Per-run names make concurrent runs
    // coexist; the battery-wide lock is the CI layer's half (guarded-entry).
    let agent_a = support::scoped(AGENT_A_BASE);
    let agent_b = support::scoped(AGENT_B_BASE);

    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");

    // QA04-09 is decided from the observed publish outcome (see the
    // `mirror_warms` binding next to the publish assertion). Hoisted into an
    // atomic because the mirror-entry assertions live in the host-side
    // records, after the async block completes. A host-side
    // `same_device` probe cannot predict it: btrfs gives each subvolume its own
    // st_dev and runc's bind-mount result is not derivable from the host at
    // all (a host that reported "same device" still produced EXDEV inside
    // every container).
    let mirror_shape = Arc::new(std::sync::Mutex::new(None::<bool>));
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");
    for agent in [&agent_a, &agent_b] {
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
        for name in [&agent_a, &agent_b] {
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
            .provision_parcel_with_report(&sanitize_name(&agent_a).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| e.to_string())?;
        assert!(
            a.image
                .tag
                .starts_with("localhost/hamlet/village-alpine:v1-f2-c"),
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
        // QA04-09 ground truth: did the mirror actually warm? Taken from the
        // OBSERVED publish outcome, never predicted — every failure is
        // classified so a digest or permission error cannot hide behind the
        // EXDEV excuse, and the strong "parcel B installs offline" claim is
        // asserted only in the shape that supports it.
        let mirror_warms = a.bootstrap.publish_failures.is_empty();
        *mirror_shape.lock().unwrap() = Some(mirror_warms);
        if !mirror_warms {
            // ADR-0013 cl. 3: EXDEV publish is explicitly non-fatal, so the
            // mirror legitimately stays empty — but only EXDEV may say so.
            for failure in &a.bootstrap.publish_failures {
                assert!(
                    support::is_exdev(failure),
                    "publish failure must be EXDEV and nothing else: {failure}"
                );
            }
        }

        // --- 2. the toolchains RUN inside, through the login profile -------
        for (script, expect) in [
            ("pixi --version", "pixi 0.34.0"),
            ("node --version", NODE_VERSION),
            ("nvm --version", NVM_VERSION),
            ("cargo --version", RUST_VERSION),
            ("rustc --version", RUST_VERSION),
        ] {
            let out = login(&*runtime, &agent_a, script).await?;
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
        let via_nvm = login(&*runtime, &agent_a, "nvm which 22").await?;
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
            let out = login(&*runtime, &agent_a, script).await?;
            assert_eq!(out.stdout_lossy().trim(), expect, "{script}");
        }

        // --- 4. SPEC §11.4: no secret was baked into the home -------------
        let leaked = run(
            &*runtime,
            &agent_a,
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
            &*runtime,
            &agent_a,
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
            let owner = run(&*runtime, &agent_a, &["stat", "-c", "%u", &path]).await?;
            assert_eq!(
                owner.stdout_lossy().trim(),
                UID_A.to_string(),
                "{path} is owned by the slot uid inside the container"
            );
        }
        // …and the private staging dir is gone (~200 MB reclaimed per parcel).
        let staged = run(&*runtime, &agent_a, &["ls", "/home/1001/.hamlet/stage"]).await?;
        assert_ne!(staged.code, 0, "staging is dropped after extraction");

        // --- 6. SPEC §4: no systemd-class init, just the daemon's keep-alive
        let pid1 = run(&*runtime, &agent_a, &["cat", "/proc/1/cmdline"]).await?;
        assert_eq!(pid1.code, 0);
        assert!(
            pid1.stdout_lossy().contains("sleep"),
            "PID 1 is the slim keep-alive the daemon controls: {:?}",
            pid1.stdout_lossy()
        );

        // --- 7. isolation invariants still hold after bootstrap -----------
        for denied in ["touch /var/git/escape", "touch /var/context/escape"] {
            let out = login(&*runtime, &agent_a, denied).await?;
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
            let out = run(&*runtime, &agent_a, &["ls", path]).await?;
            assert_ne!(out.code, 0, "{path} must not be reachable");
        }
        let freight = run(&*runtime, &agent_a, &["ls", "/var/freight"]).await?;
        assert_ne!(
            freight.code, 0,
            "freight stays ACL-gated (01770, other::---) without the freight plane"
        );

        // --- 8. the mirror holds every pinned artifact --------------------
        let mirrored = login(&*runtime, &agent_a, "ls -1 /var/pkgcache/dist | wc -l").await?;
        assert_eq!(
            mirrored.stdout_lossy().trim(),
            if mirror_warms {
                ARTIFACT_COUNT.to_string()
            } else {
                // Cross-device publish: the cache stays empty by design. The
                // guarantee that survives is not "the mirror is warm" but
                // "the cold parcel installed and was digest-verified".
                "0".to_owned()
            },
            "the mirror holds every pinned artifact exactly when home and \
             pkgcache share a device (ADR-0013 cl. 3; SPEC §4 mounts)"
        );

        // --- 9. RE-RUN on the same home: verified no-op, zero downloads ----
        let bootstrapper = Bootstrapper::new(&tree, runtime.as_ref()).map_err(|e| e.to_string())?;
        let target_a = BootstrapTarget::new(sanitize_name(&agent_a).unwrap(), UID_A).unwrap();
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
            .provision_parcel_with_report(&sanitize_name(&agent_b).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| e.to_string())?;
        assert!(!b.bootstrap.no_op, "a fresh home still installs");
        if mirror_warms {
            assert_eq!(
                b.bootstrap.downloads, 0,
                "pkgcache hit instrumentation: the second parcel downloads NOTHING"
            );
            assert_eq!(b.bootstrap.cache_hits, ARTIFACT_COUNT);
            assert_eq!(b.bootstrap.cache_misses, 0);
        } else {
            // QA04-09 on a cross-device host: with an empty mirror the second
            // parcel MUST re-download — that is the cache behaving correctly,
            // not a degradation. The zero-download claim is a property of the
            // warm-mirror shape only, so it is stated only there.
            assert_eq!(
                b.bootstrap.downloads, ARTIFACT_COUNT,
                "an empty (EXDEV-unwritable) mirror makes every artifact a miss"
            );
            assert_eq!(b.bootstrap.cache_hits, 0);
            assert_eq!(b.bootstrap.cache_misses, ARTIFACT_COUNT);
        }
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
        let out = login(&*runtime, &agent_b, "cargo --version").await?;
        assert!(
            out.stdout_lossy().contains(RUST_VERSION),
            "parcel B installed Rust from the mirror: {}",
            out.stdout_lossy()
        );
        let out = login(&*runtime, &agent_b, "node --version").await?;
        assert!(out.stdout_lossy().contains(NODE_VERSION));
        let owner = run(
            &*runtime,
            &agent_b,
            &["stat", "-c", "%u", "/home/1002/.cargo/bin/cargo"],
        )
        .await?;
        assert_eq!(owner.stdout_lossy().trim(), UID_B.to_string());
        // …and neither parcel can reach the other's toolchain.
        let cross = run(&*runtime, &agent_b, &["ls", "/home/1001"]).await?;
        assert_ne!(cross.code, 0, "cross-UID home read blocked");
        let cross = run(&*runtime, &agent_a, &["ls", "/home/1002"]).await?;
        assert_ne!(cross.code, 0, "cross-UID home read blocked both ways");
        // The mirror is shared by design — but only as a cache: B reads A's
        // published artifact, and a tampered copy would fail its digest pin.
        let shared = run(&*runtime, &agent_b, &["ls", "/var/pkgcache/dist"]).await?;
        assert_eq!(shared.code, 0, "the mirror is readable by every slot");
        eprintln!(
            "guarded bootstrap host shape: home/pkgcache {} device -> mirror {}",
            if mirror_warms { "SHARE" } else { "SPLIT" },
            if mirror_warms {
                "warms"
            } else {
                "stays empty (EXDEV publish)"
            }
        );

        // --- 11. graceful teardown through the runtime seam ---------------
        runtime
            .stop(&agent_a, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        runtime
            .stop(&agent_b, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }
    .await;

    // The container-side failure (if any) is reported FIRST and in full: the
    // host-side records below are asserted even when it failed, and an
    // empty DEED.md used to hide the real cause behind a confusing assert.
    if let Err(err) = &result {
        eprintln!("guarded bootstrap container-side failure: {err}");
    }

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
    let mirror_warms = mirror_shape
        .lock()
        .unwrap()
        .expect("the run reached the publish step");
    assert_eq!(
        entries.len(),
        if mirror_warms { ARTIFACT_COUNT } else { 0 },
        "one file per pinned artifact when the mirror can be hardlink-published \\
         (ADR-0013 cl. 3), and none when the publish is EXDEV — the cache is a \\
         cache, so an unwritable mirror is empty, not partial"
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

// ---------------------------------------------------------------------------
// QA04-32: force the warm mirror arm (ADR-0035)
// ---------------------------------------------------------------------------
// This test forces the publish-arm seam to `cp --` so the mirror warms even
// on cross-device hosts. It exercises the *downstream accounting* that the
// product's `ln` would produce on a shared-device host: mirror file count,
// cache_hits/downloads, digest re-verification, and parcel B offline
// toolchain operation. It does NOT exercise ADR-0013 cl. 3's symlink-refusal
// or inode-identity (those are product `ln` properties, separately pinned).
// The seam is honest: it simulates a shared-device topology, it does not
// fake success.

#[tokio::test]
#[ignore = "guarded: needs HAMLET_PODMAN_TESTS=1 + podman on PATH (CONVENTIONS §5)"]
async fn podman_bootstrap_forced_warm_mirror_makes_second_parcel_offline() {
    // QA04-27: serialised with the other cold bootstrap.
    let _serial = support::COLD_BOOTSTRAP_LOCK.lock().await;
    if let Some(reason) = podman_guard()
        .or_else(support::userns_precondition)
        .or_else(|| support::work_area_precondition(&std::env::temp_dir()))
    {
        support::skip(
            "podman_bootstrap_forced_warm_mirror_makes_second_parcel_offline",
            &reason,
        );
        return;
    }

    let agent = support::scoped(AGENT_A_BASE); // slot 1001
    let agent_b = support::scoped(AGENT_B_BASE); // slot 1002
    let tmp = tempfile::tempdir().expect("tempdir");
    let tree = HamletTree::ensure(tmp.path().join(".hamlet")).expect("village tree");

    // SPEC §5: provisioning seeds PROPRIETOR.md from the guild register, so
    // both parcels need a brief before their provision call (sibling-suite
    // pattern) — and the charter must exist for the provisioner to save into.
    Charter::empty()
        .save_atomic(&tree.charter())
        .expect("seed charter");
    for name in [&agent, &agent_b] {
        fs::write(
            tree.register_brief(&sanitize_name(name).unwrap()),
            format!("# {name}\n\nGuarded-bootstrap brief.\n"),
        )
        .expect("brief");
    }

    let config = Config::default();
    let inner_runtime =
        Arc::new(PodmanRuntime::from_config(&config).with_pull_retry(3, Duration::from_secs(3)));
    // Force the warm arm: rewrite publish argv from `ln` to `cp --`.
    let runtime = support::PublishSeamRuntime::new(inner_runtime, support::PublishArm::ForceWarm);

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
        let home = tree.parcel_home(UID_A);
        if home.exists() {
            let _ = StdCommand::new("podman")
                .args(["unshare", "chown", "0:0", "--"])
                .arg(&home)
                .output();
        }
    };
    cleanup();

    let result = async {
        // Provision parcel A with the forced warm mirror.
        let a = provisioner
            .provision_parcel_with_report(&sanitize_name(&agent).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| e.to_string())?;

        // The seam must have rewired at least one publish.
        assert!(
            runtime.rewrites() > 0,
            "the publish-arm seam must have rewritten at least one argv"
        );
        // With ForceWarm, publish_failures must be empty (cp crosses devices).
        assert!(
            a.bootstrap.publish_failures.is_empty(),
            "ForceWarm arm: publish must succeed (cp crosses devices; no EXDEV)"
        );
        // Mirror must hold all artifacts.
        let dist = tree.market_pkgcache_dist();
        let entries: Vec<_> = fs::read_dir(&dist)
            .expect("mirror dir exists")
            .map(|e| e.expect("entry"))
            .collect();
        assert_eq!(
            entries.len(),
            ARTIFACT_COUNT,
            "mirror holds all pinned artifacts when warm"
        );

        // Provision parcel B: must download ZERO, all cache hits.
        let b = provisioner
            .provision_parcel_with_report(&sanitize_name(&agent_b).unwrap(), COMMISSION, &blueprint)
            .await
            .map_err(|e| e.to_string())?;

        assert_eq!(
            b.bootstrap.downloads, 0,
            "parcel B downloads NOTHING when mirror is warm"
        );
        assert_eq!(b.bootstrap.cache_hits, ARTIFACT_COUNT);
        assert_eq!(b.bootstrap.cache_misses, 0);
        assert_eq!(b.bootstrap.verified_artifacts.len(), ARTIFACT_COUNT);

        // B's toolchain works and is B's own.
        let out = login(&*runtime, &agent_b, "cargo --version").await?;
        assert!(
            out.stdout_lossy().contains(RUST_VERSION),
            "parcel B installed Rust from the mirror: {}",
            out.stdout_lossy()
        );
        let out = login(&*runtime, &agent_b, "node --version").await?;
        assert!(out.stdout_lossy().contains(NODE_VERSION));
        let owner = run(
            &*runtime,
            &agent_b,
            &["stat", "-c", "%u", "/home/1002/.cargo/bin/cargo"],
        )
        .await?;
        assert_eq!(owner.stdout_lossy().trim(), UID_B.to_string());

        // Isolation still holds.
        let cross = run(&*runtime, &agent_b, &["ls", "/home/1001"]).await?;
        assert_ne!(cross.code, 0, "cross-UID home read blocked");
        let cross = run(&*runtime, &agent, &["ls", "/home/1002"]).await?;
        assert_ne!(cross.code, 0, "cross-UID home read blocked both ways");

        // Teardown.
        runtime
            .stop(&agent, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        runtime
            .stop(&agent_b, Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }
    .await;

    cleanup();
    result.expect("forced warm mirror: parcel B is offline-fast (ADR-0035)");
}
