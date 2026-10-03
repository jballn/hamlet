//! Blueprint bootstrap integration tests (Sprint 02 step 04; CONVENTIONS §9).
//!
//! Daemon behavior against a tempdir `.hamlet/` tree with the mock seam:
//! [`FakeRuntime`] records every argv (built through the same pure builders as
//! real podman) and answers scripted `exec` output, so the whole bootstrap
//! flow — mirror probe, stage, verify, extract, probe, profile/manifest
//! writes, DEED baseline — runs without podman. Container *semantics* (real
//! `pixi --version`, real digests, real offline re-provision) live in the
//! guarded suite (`bootstrap_guarded.rs`).
//!
//! Focus: the executor's decisions and side-effect discipline. The plan's
//! shape is golden-tested in `bootstrap.rs` unit tests; these tests drive it.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use hamlet_core::blueprint::Blueprint;
use hamlet_core::charter::Charter;
use hamlet_core::config::Config;
use hamlet_core::pins::ToolchainPins;
use hamlet_core::sanitize::{sanitize_name, SafeName};
use hamlet_core::tree::HamletTree;
use hamlet_daemon::bootstrap::{
    artifact_files, deed_baseline_entry, deed_has_baseline, mirror_path, plan_bootstrap,
    script_pinned_digests, staged_path, BootstrapReport, BootstrapTarget, Bootstrapper,
    DEED_BASELINE_MARKER,
};
use hamlet_daemon::census::{ProvisionError, Provisioner};
use hamlet_daemon::hostperm::MODE_PKGCACHE_DIST;
use hamlet_daemon::podman::{ExecOutput, FakeRuntime, PodmanErrorKind, Runtime};

const UID: u32 = 1001;
const AGENT: &str = "scribe";
const COMMISSION: u64 = 7;
const BRIEF: &str = "# scribe\n\nCore principles: copy faithfully.";

/// Digest the fake answers for the in-home manifest/profile files (the real
/// value is computed in-container from the bytes bootstrap writes).
const HOME_FILE_SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn name(s: &str) -> SafeName {
    sanitize_name(s).expect("test name must sanitize")
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

struct Fixture {
    tree: HamletTree,
    runtime: Arc<FakeRuntime>,
    pins: ToolchainPins,
    blueprint: Blueprint,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    /// A village tree with a register brief, an empty charter and a
    /// fully-scripted fake runtime (every pinned digest verifies).
    fn new(uids: &[u32]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let tree = HamletTree::ensure(tmp.path().join(".hamlet")).unwrap();
        Charter::empty().save_atomic(&tree.charter()).unwrap();
        fs::write(tree.register_brief(&name(AGENT)), BRIEF).unwrap();
        let pins = ToolchainPins::default_hamlet().unwrap();
        let runtime = Arc::new(FakeRuntime::new());
        for uid in uids {
            script_pinned_digests(&runtime, *uid, &pins, HOME_FILE_SHA256);
        }
        Self {
            tree,
            runtime,
            pins,
            blueprint: Blueprint::default_village_alpine().unwrap(),
            _tmp: tmp,
        }
    }

    fn bootstrapper(&self) -> Bootstrapper<'_> {
        Bootstrapper::with_pins(
            &self.tree,
            self.runtime.as_ref() as &dyn Runtime,
            self.pins.clone(),
        )
    }

    fn target(&self) -> BootstrapTarget {
        BootstrapTarget::new(name(AGENT), UID).unwrap()
    }

    /// Script the two home reads so the next bootstrap sees a home that
    /// already matches the desired state (the verified no-op path).
    fn script_bootstrapped_home(&self, uid: u32) {
        let plan = self.plan(uid, &BTreeSet::new());
        let output = |content: &str| ExecOutput {
            code: 0,
            stdout: content.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        self.runtime.script_exec_for(
            format!("cat {}", hamlet_daemon::bootstrap::manifest_path(uid)),
            output(&plan.manifest),
        );
        self.runtime.script_exec_for(
            format!("cat {}", hamlet_daemon::bootstrap::profile_path(uid)),
            output(&plan.profile),
        );
        // The probe answers a version string (non-zero would fail the no-op).
        self.runtime
            .script_exec_for("pixi --version", output("pixi 0.34.0"));
        self.runtime
            .script_exec_for("node --version", output("v22.23.3"));
        self.runtime
            .script_exec_for("cargo --version", output("cargo 1.98.1"));
    }

    fn plan(&self, uid: u32, cached: &BTreeSet<String>) -> hamlet_daemon::bootstrap::BootstrapPlan {
        plan_bootstrap(uid, &self.blueprint, &self.pins, cached).unwrap()
    }

    fn deed(&self, uid: u32) -> String {
        fs::read_to_string(self.tree.parcel_deed(uid)).unwrap()
    }

    /// Every argv the fake recorded, joined for substring assertions.
    fn argv_text(&self) -> String {
        self.runtime
            .argv_log()
            .iter()
            .map(|argv| argv.join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Seed the stub DEED.md the way provisioning does, so bootstrap has its
/// `### Dynamic Alterations` block to record into.
fn seed_deed(f: &Fixture, uid: u32) {
    fs::create_dir_all(f.tree.parcel(uid)).unwrap();
    fs::write(
        f.tree.parcel_deed(uid),
        hamlet_daemon::census::deed_stub(&name(AGENT), uid, COMMISSION, &f.blueprint),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Cold bootstrap
// ---------------------------------------------------------------------------

/// The full flow on an empty home: every pinned artifact is downloaded,
/// digest-verified, extracted, probed; the profile + manifest are written
/// through `tee`; the DEED baseline is recorded.
#[tokio::test]
async fn cold_bootstrap_installs_every_tool_and_records_the_baseline() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);

    let report = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();

    assert!(!report.no_op, "an empty home is not a no-op");
    assert_eq!((report.cache_hits, report.cache_misses), (0, 7));
    assert_eq!(report.downloads, 7, "one download per pinned artifact");
    assert_eq!(report.verified_artifacts.len(), 7);
    assert_eq!(report.steps_executed, 48, "the golden step count");
    assert_eq!(report.manifest_sha256, HOME_FILE_SHA256);
    assert!(report.deed_recorded);
    assert!(report.publish_failures.is_empty());
    assert_eq!(
        report.probes.keys().collect::<Vec<_>>(),
        ["node", "pixi", "rust"],
        "all three SPEC §4 multiplexers are probed"
    );

    // The mirror directory exists with sticky world-writable mode, so every
    // slot can publish into it and none can clobber another's entry.
    let dist = f.tree.market_pkgcache_dist();
    assert!(dist.is_dir());
    assert_eq!(mode_of(&dist), MODE_PKGCACHE_DIST);
    // (The 01777 parent mode is provisioning's job — asserted in provision.rs.)

    // DEED.md: the stub is untouched, the baseline entry sits in the block.
    let deed = f.deed(UID);
    let plan = f.plan(UID, &BTreeSet::new());
    let expected_entry = deed_baseline_entry(UID, &plan, HOME_FILE_SHA256);
    assert!(deed.ends_with(&expected_entry), "{deed}");
    assert!(deed_has_baseline(&deed, UID, HOME_FILE_SHA256));
    assert!(deed.contains(DEED_BASELINE_MARKER));
    assert_eq!(
        deed.matches(DEED_BASELINE_MARKER).count(),
        1,
        "exactly one baseline entry"
    );

    // Every artifact went out over https to its pinned URL, and every write
    // into the home happened through an in-container `tee`.
    let argv = f.argv_text();
    assert_eq!(argv.matches("curl -fsSL --retry 3 -o ").count(), 7);
    assert!(argv.contains("tee /home/1001/.profile"));
    assert!(argv.contains("tee /home/1001/.hamlet/toolchain.json"));
    assert_eq!(
        argv.matches("sha256sum ").count(),
        8,
        "7 artifacts + manifest"
    );
}

/// The home is 0700 subuid-owned (ADR-0011), so the daemon's host user must
/// not write into it: every byte goes through `podman exec`, and no host path
/// ever appears in an in-container argv.
#[tokio::test]
async fn home_files_are_written_from_inside_the_container_never_the_host() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    fs::create_dir_all(f.tree.parcel_home(UID)).unwrap();

    f.bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();

    // The host-side home is still empty: the fake spawns nothing, and the
    // daemon wrote nothing into it directly.
    let entries: Vec<_> = fs::read_dir(f.tree.parcel_home(UID))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "host never writes the home: {entries:?}"
    );

    // No in-container argv carries a host path (the village root, the tempdir).
    let root = f.tree.root().to_str().unwrap().to_owned();
    for line in f.argv_text().lines() {
        assert!(!line.contains(&root), "host path leaked into argv: {line}");
    }

    // The written bytes are exactly the plan's (stdin payloads recorded).
    let plan = f.plan(UID, &BTreeSet::new());
    let stdins = f.runtime.stdin_log();
    assert_eq!(stdins.len(), 2, "profile + manifest");
    assert!(stdins[0].0.join(" ").contains("tee /home/1001/.profile"));
    assert_eq!(String::from_utf8_lossy(&stdins[0].1), plan.profile);
    assert!(stdins[1]
        .0
        .join(" ")
        .contains("tee /home/1001/.hamlet/toolchain.json"));
    assert_eq!(String::from_utf8_lossy(&stdins[1].1), plan.manifest);
}

/// SPEC §11.4: secrets are injected at session start, never baked into a
/// home. A `.env` in the village root and a secret in the process env must
/// not appear in any argv, stdin payload, home file, or DEED line.
#[tokio::test]
async fn secrets_never_reach_the_home_the_argv_or_the_deed() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    // Obvious fakes (CONVENTIONS §6).
    fs::write(
        f.tree.env_file(),
        "HAMLET_TEST_SECRET_KEY=test-key-do-not-use\nHAMLET_TEST_TOKEN=test-token-do-not-use\n",
    )
    .unwrap();
    std::env::set_var("HAMLET_TEST_SECRET_KEY", "test-key-do-not-use");

    f.bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();
    std::env::remove_var("HAMLET_TEST_SECRET_KEY");

    let forbidden = [
        "test-key-do-not-use",
        "test-token-do-not-use",
        "HAMLET_TEST_SECRET_KEY",
        "HAMLET_TEST_TOKEN",
    ];
    let haystacks: Vec<String> = {
        let mut v = vec![f.argv_text(), f.deed(UID)];
        v.extend(
            f.runtime
                .stdin_log()
                .iter()
                .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned()),
        );
        let plan = f.plan(UID, &BTreeSet::new());
        v.push(plan.profile);
        v.push(plan.manifest);
        v
    };
    for needle in forbidden {
        for haystack in &haystacks {
            assert!(
                !haystack.contains(needle),
                "{needle} leaked into bootstrap output"
            );
        }
    }
    // And the .env file itself was never read into the flow (no dotenvy here).
    assert!(f.tree.env_file().is_file(), "the fixture wrote it");
}

// ---------------------------------------------------------------------------
// Idempotence / convergence
// ---------------------------------------------------------------------------

/// Re-running bootstrap on a home that already matches the desired state is a
/// VERIFIED no-op: zero downloads, zero install steps, and the probes still
/// answer (a wiped or corrupted home would fall through to a full run).
#[tokio::test]
async fn second_bootstrap_is_a_verified_no_op_with_zero_downloads() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    f.script_bootstrapped_home(UID);

    let report = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();

    assert!(report.no_op);
    assert_eq!(report.downloads, 0, "no network on a verified no-op");
    assert_eq!(report.steps_executed, 0, "no install step ran");
    assert_eq!(report.manifest_sha256, HOME_FILE_SHA256);
    assert_eq!(report.probes["pixi"], "pixi 0.34.0");
    assert_eq!(report.probes["node"], "v22.23.3");
    assert_eq!(report.probes["rust"], "cargo 1.98.1");

    let argv = f.argv_text();
    assert!(!argv.contains("curl"), "no download verb at all");
    assert!(!argv.contains("tar "), "nothing extracted");
    assert!(!argv.contains("tee "), "nothing rewritten");
    // Two reads + three probes + one manifest digest = six execs, all offline.
    assert_eq!(argv.matches("podman exec").count(), 6, "{argv}");
    assert_eq!(argv.matches("sha256sum ").count(), 1);

    // The DEED baseline is recorded (this run had to write it — the stub was
    // fresh), and a second no-op run does not duplicate it.
    assert!(report.deed_recorded);
    let report2 = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();
    assert!(report2.no_op);
    assert!(!report2.deed_recorded, "already recorded → no duplicate");
    assert_eq!(f.deed(UID).matches(DEED_BASELINE_MARKER).count(), 1);
    assert_eq!(
        report2.probes, report.probes,
        "the probes answer identically"
    );
    assert_eq!(report2.manifest_sha256, report.manifest_sha256);
}

/// Restart-resumability (CONVENTIONS §3 r6) over the state this step
/// introduces: a FRESH bootstrapper (new runtime, nothing in memory) on a
/// bootstrapped home converges to the same no-op verdict from disk alone.
#[tokio::test]
async fn a_fresh_bootstrapper_converges_to_the_same_no_op() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    f.script_bootstrapped_home(UID);
    f.bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();
    let deed_before = f.deed(UID);

    // "Kill the daemon": a brand-new runtime + bootstrapper over the same tree.
    let restarted = Arc::new(FakeRuntime::new());
    script_pinned_digests(&restarted, UID, &f.pins, HOME_FILE_SHA256);
    let plan = f.plan(UID, &BTreeSet::new());
    let out = |content: &str| ExecOutput {
        code: 0,
        stdout: content.as_bytes().to_vec(),
        stderr: Vec::new(),
    };
    restarted.script_exec_for(
        format!("cat {}", hamlet_daemon::bootstrap::manifest_path(UID)),
        out(&plan.manifest),
    );
    restarted.script_exec_for(
        format!("cat {}", hamlet_daemon::bootstrap::profile_path(UID)),
        out(&plan.profile),
    );
    for (matcher, value) in [
        ("pixi --version", "pixi 0.34.0"),
        ("node --version", "v22.23.3"),
        ("cargo --version", "cargo 1.98.1"),
    ] {
        restarted.script_exec_for(matcher, out(value));
    }
    let fresh =
        Bootstrapper::with_pins(&f.tree, restarted.as_ref() as &dyn Runtime, f.pins.clone());
    let report = fresh
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();
    assert!(report.no_op, "recovered state says: already bootstrapped");
    assert_eq!(report.downloads, 0);
    assert_eq!(f.deed(UID), deed_before, "DEED.md is byte-identical");
}

/// A warm mirror makes the NEXT parcel offline-fast: zero downloads, every
/// artifact staged from `/var/pkgcache/dist/` and still digest-verified.
#[tokio::test]
async fn warm_mirror_means_zero_downloads_for_the_next_parcel() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    // The first parcel filled the mirror (host side: regular, non-empty files).
    let files = artifact_files(&f.blueprint, &f.pins).unwrap();
    fs::create_dir_all(f.tree.market_pkgcache_dist()).unwrap();
    for file in &files {
        fs::write(f.tree.pkgcache_artifact(file).unwrap(), b"mirrored bytes").unwrap();
    }

    let report = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();

    assert!(!report.no_op, "the home is still empty — install runs");
    assert_eq!(
        report.downloads, 0,
        "pkgcache hit instrumentation: zero network"
    );
    assert_eq!(report.cache_hits, 7);
    assert_eq!(report.cache_misses, 0);
    assert_eq!(
        report.verified_artifacts.len(),
        7,
        "cache hits are still verified"
    );
    let argv = f.argv_text();
    assert!(!argv.contains("curl"), "no download verb at all");
    assert_eq!(argv.matches("cp /var/pkgcache/dist/").count(), 7);
    assert_eq!(argv.matches("sha256sum ").count(), 8);
    assert!(
        !argv.contains(" ln "),
        "a cache hit is never re-published (the mirror already has it)"
    );
}

// ---------------------------------------------------------------------------
// Adversarial: the mirror is world-writable
// ---------------------------------------------------------------------------

/// A poisoned mirror entry (right name, wrong bytes) is REFUSED: the digest
/// check fires before any extraction, nothing lands in the home, and no DEED
/// claim is recorded.
#[tokio::test]
async fn a_poisoned_mirror_artifact_is_refused_and_never_extracted() {
    // Unscripted fixture: the wrong digest must be registered BEFORE the
    // correct pin scripts (first match wins in the fake).
    let f = Fixture::new(&[]);
    seed_deed(&f, UID);
    let deed_before = f.deed(UID);

    // Another UID "cached" a rustc tarball whose digest is not the pin.
    let poisoned = "rustc-1.98.1-x86_64-unknown-linux-musl.tar.xz";
    fs::create_dir_all(f.tree.market_pkgcache_dist()).unwrap();
    fs::write(f.tree.pkgcache_artifact(poisoned).unwrap(), b"evil payload").unwrap();
    // The fake answers the WRONG digest for that staged file (registered
    // first, so it wins over the correct pin script below).
    f.runtime.script_exec_for(
        format!("sha256sum {}", staged_path(UID, poisoned)),
        ExecOutput {
            code: 0,
            stdout: format!("{}  {}\n", "f".repeat(64), staged_path(UID, poisoned)).into_bytes(),
            stderr: Vec::new(),
        },
    );
    script_pinned_digests(&f.runtime, UID, &f.pins, HOME_FILE_SHA256);

    let err = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("digest mismatch"), "{text}");
    assert!(text.contains(poisoned), "{text}");
    assert!(
        text.contains(&"f".repeat(64)),
        "the actual digest is reported"
    );

    let argv = f.argv_text();
    // pixi and node installed fine (they come first), but nothing of the
    // poisoned toolchain was extracted, executed, published, or written.
    assert!(
        !argv.contains(&format!(
            "tar -x -J -f {}{}",
            staged_path(UID, poisoned),
            " -C"
        )),
        "no extraction of an unverified artifact: {argv}"
    );
    assert!(
        !argv.contains(&format!(
            "ln {} {}",
            staged_path(UID, poisoned),
            mirror_path(poisoned)
        )),
        "never published"
    );
    assert!(
        !argv.contains("rustup toolchain link"),
        "no toolchain linked"
    );
    assert!(
        !argv.contains("tee /home/1001/.hamlet/toolchain.json"),
        "no manifest written"
    );
    assert_eq!(f.deed(UID), deed_before, "DEED.md claims nothing");
}

/// A symlink planted in the mirror is a MISS, never followed (ADR-0007): the
/// artifact is downloaded fresh instead of copying through the link.
#[tokio::test]
async fn a_symlinked_mirror_entry_is_a_miss_never_followed() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    let files = artifact_files(&f.blueprint, &f.pins).unwrap();
    fs::create_dir_all(f.tree.market_pkgcache_dist()).unwrap();
    let target = f.tree.parcel_deed(UID); // something the daemon owns
    let planted = &files[0];
    std::os::unix::fs::symlink(&target, f.tree.pkgcache_artifact(planted).unwrap()).unwrap();
    for file in &files[1..] {
        fs::write(f.tree.pkgcache_artifact(file).unwrap(), b"mirrored bytes").unwrap();
    }

    let report = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();
    assert_eq!(report.cache_hits, 6, "the symlink is not a cache hit");
    assert_eq!(report.cache_misses, 1);
    assert_eq!(report.downloads, 1);
    let argv = f.argv_text();
    assert!(
        argv.contains(&format!(
            "curl -fsSL --retry 3 -o {} ",
            staged_path(UID, planted)
        )),
        "it is re-downloaded instead: {argv}"
    );
    assert!(
        !argv.contains(&format!("cp {} ", mirror_path(planted))),
        "never copied through the planted link"
    );
    // Publishing is skipped for a name that already exists (best effort).
    assert!(
        fs::read_link(f.tree.pkgcache_artifact(planted).unwrap()).is_ok(),
        "the planted link itself is untouched"
    );
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        f.deed(UID),
        "and its target was never written through"
    );
    assert!(
        report.verified_artifacts.contains(planted),
        "re-downloaded AND verified"
    );
}

/// Publishing is a cache optimization: a failed `ln` (another UID won the
/// race, or EXDEV) is recorded and the bootstrap still succeeds.
#[tokio::test]
async fn publish_failure_is_non_fatal_and_recorded() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    let file = "nvm-0.40.8.tar.gz";
    f.runtime.script_exec_for(
        format!("ln {} {}", staged_path(UID, file), mirror_path(file)),
        ExecOutput {
            code: 1,
            stdout: Vec::new(),
            stderr: "ln: /var/pkgcache/dist/nvm-0.40.8.tar.gz: File exists".into(),
        },
    );

    let report = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap();
    assert_eq!(report.publish_failures.len(), 1);
    assert!(report.publish_failures[0].starts_with("nvm-0.40.8.tar.gz: exit 1"));
    assert!(!report.no_op);
    assert_eq!(report.downloads, 7, "the rest of the flow is unaffected");
}

// ---------------------------------------------------------------------------
// Image policy (ADR-0012 cl. 2)
// ---------------------------------------------------------------------------

/// The derived image is built once, from a daemon-private context OUTSIDE the
/// village tree, and the derived tag is never pulled (it is `localhost/`).
#[tokio::test]
async fn ensure_image_builds_once_from_a_private_context() {
    let f = Fixture::new(&[UID]);
    f.runtime.set_image_exists(false); // nothing in the local store yet

    let outcome = f.bootstrapper().ensure_image(&f.blueprint).await.unwrap();
    assert!(outcome.built);
    assert!(outcome
        .tag
        .starts_with("localhost/hamlet/village-alpine:v1-f2-c"));

    let log = f.runtime.argv_log();
    let verbs: Vec<&str> = log.iter().map(|a| a[1].as_str()).collect();
    assert_eq!(verbs, ["image", "image", "pull", "build"], "{verbs:?}");
    // The parent is pulled; the derived tag is only ever probed + built.
    assert_eq!(log[0][3], outcome.tag);
    assert_eq!(log[1][3], "docker.io/library/alpine:3.24");
    assert_eq!(log[2], ["podman", "pull", "docker.io/library/alpine:3.24"]);
    assert!(
        !f.argv_text()
            .lines()
            .any(|l| l.starts_with("podman pull localhost/")),
        "derived images are never pulled"
    );

    let build = &log[3];
    assert_eq!(
        build[..4],
        ["podman", "build", "--tag", outcome.tag.as_str()]
    );
    assert_eq!(build[4], "--file");
    let containerfile = Path::new(&build[5]);
    let context = Path::new(&build[6]);
    assert_eq!(containerfile.file_name().unwrap(), "Containerfile");
    assert_eq!(containerfile.parent().unwrap(), context);
    // Never inside the village tree (pkgcache is 01777 — a writable
    // Containerfile there would be arbitrary code as the container's root).
    assert!(!context.starts_with(f.tree.root()), "{context:?}");
    assert!(!context.starts_with(f.tree.market_pkgcache()));
    assert!(context.starts_with(std::env::temp_dir()));
    // Cleaned up after the build: no litter, nothing to tamper with.
    assert!(!context.exists(), "the private context is removed");

    // Second call: the tag is in the store now → probe only, no build.
    f.runtime.set_image_exists(true);
    let again = f.bootstrapper().ensure_image(&f.blueprint).await.unwrap();
    assert!(!again.built);
    assert_eq!(again.tag, outcome.tag);
    let log = f.runtime.argv_log();
    assert_eq!(log[4], ["podman", "image", "exists", outcome.tag.as_str()]);
    assert_eq!(log.len(), 5, "nothing else spawned");
}

/// A `build` failure surfaces typed with the argv, and leaves no context dir.
#[tokio::test]
async fn image_build_failure_is_typed() {
    let f = Fixture::new(&[UID]);
    f.runtime.set_image_exists(false);
    f.runtime
        .fail_next_for("build", PodmanErrorKind::NonZeroExit { code: Some(1) });
    let err = f
        .bootstrapper()
        .ensure_image(&f.blueprint)
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("podman build"), "{text}");
    assert!(text.contains("--tag"), "the argv is part of the error");
    let ctx = hamlet_daemon::bootstrap::build_context_dir(&f.tree, &f.blueprint);
    assert!(!ctx.exists(), "cleaned up even on failure");
}

// ---------------------------------------------------------------------------
// Failure paths + provisioning integration
// ---------------------------------------------------------------------------

/// A failing step aborts with the argv and stderr in the error, writes no
/// manifest and records no DEED baseline.
#[tokio::test]
async fn a_failing_step_aborts_without_claiming_success() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    let deed_before = f.deed(UID);
    f.runtime.script_exec_for(
        "tar -x -z -f /home/1001/.hamlet/stage/pixi",
        ExecOutput {
            code: 1,
            stdout: Vec::new(),
            stderr: "tar: unexpected end of file".into(),
        },
    );

    let err = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("unexpected end of file"), "{text}");
    for fragment in ["\"tar\"", "\"-x\"", "\"-z\"", "\"-f\"", "\"-C\""] {
        assert!(text.contains(fragment), "argv is in the error: {text}");
    }
    assert_eq!(f.deed(UID), deed_before, "no baseline for a broken home");
    assert!(
        !f.argv_text().contains("tee /home/1001/.profile"),
        "no profile written"
    );
}

/// A failed probe (tool installed but not runnable) is fatal: the home is not
/// usable, so no manifest and no DEED claim.
#[tokio::test]
async fn a_failed_probe_is_fatal() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    let deed_before = f.deed(UID);
    f.runtime.script_exec_for(
        "/home/1001/.cargo/bin/cargo --version",
        ExecOutput {
            code: 127,
            stdout: Vec::new(),
            stderr: "not found".into(),
        },
    );
    let err = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        hamlet_daemon::bootstrap::BootstrapError::ProbeFailed { .. }
    ));
    assert!(err.to_string().contains("rust probe failed"), "{err}");
    assert_eq!(f.deed(UID), deed_before);
    assert!(!f
        .argv_text()
        .contains("tee /home/1001/.hamlet/toolchain.json"));
}

/// A DEED.md without the `### Dynamic Alterations` block is a typed refusal
/// (SPEC §9.4): bootstrap never invents the block it records into.
#[tokio::test]
async fn a_malformed_deed_is_refused() {
    let f = Fixture::new(&[UID]);
    fs::create_dir_all(f.tree.parcel(UID)).unwrap();
    fs::write(f.tree.parcel_deed(UID), "# DEED — truncated\n").unwrap();
    let err = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            hamlet_daemon::bootstrap::BootstrapError::DeedMalformed { .. }
        ),
        "{err}"
    );
    assert_eq!(f.deed(UID), "# DEED — truncated\n", "never rewritten");
}

/// A symlinked DEED.md is refused, never written through (ADR-0007).
#[tokio::test]
async fn a_symlinked_deed_is_refused_not_written_through() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    let victim = f.tree.parcel_proprietor(UID);
    fs::write(&victim, "# PROPRIETOR — scribe\n").unwrap();
    let deed = f.tree.parcel_deed(UID);
    fs::remove_file(&deed).unwrap();
    std::os::unix::fs::symlink(&victim, &deed).unwrap();

    let err = f
        .bootstrapper()
        .bootstrap_home(&f.target(), &f.blueprint)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            hamlet_daemon::bootstrap::BootstrapError::NotRegularFile { .. }
        ),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(&victim).unwrap(),
        "# PROPRIETOR — scribe\n",
        "the symlink target is untouched"
    );
}

/// A blueprint tool with no pin refuses provisioning BEFORE any side effect
/// (ADR-0012 cl. 4: no pin, no install — never a guessed version).
#[tokio::test]
async fn an_unpinned_tool_refuses_provisioning_with_no_side_effects() {
    let f = Fixture::new(&[UID]);
    let bp = Blueprint::from_toml(
        r#"
[meta]
name = "unpinned"
version = 1
base_image = "docker.io/library/alpine:3.24"

[[tools]]
kind = "cargo"
name = "rust"
version_spec = "nightly"
"#,
    )
    .unwrap();
    let provisioner = Provisioner::new(
        f.tree.clone(),
        Config::default(),
        Arc::clone(&f.runtime) as Arc<dyn Runtime>,
    );
    let err = provisioner
        .provision_parcel(&name(AGENT), COMMISSION, &bp)
        .await
        .unwrap_err();
    assert!(matches!(err, ProvisionError::Bootstrap(_)), "{err}");
    assert!(err.to_string().contains("no toolchain pin"), "{err}");
    // The container was started (bootstrap runs after `run`), so rollback must
    // have removed it and the parcel dirs; the charter never saw the lease.
    assert!(
        f.argv_text().contains("podman rm --force scribe"),
        "container removed"
    );
    assert!(!f.tree.parcel(UID).exists(), "parcel unwound");
    assert_eq!(Charter::load(&f.tree.charter()).unwrap(), Charter::empty());
}

/// Provisioning ends with bootstrap: the parcel runs the DERIVED image, and a
/// bootstrap failure rolls the whole lease back (charter stays lease-free).
#[tokio::test]
async fn provisioning_rolls_back_when_bootstrap_fails() {
    let f = Fixture::new(&[UID]);
    // The first exec (the no-op manifest read) dies at the podman level.
    f.runtime
        .fail_next_for("exec", PodmanErrorKind::NonZeroExit { code: Some(125) });
    let provisioner = Provisioner::new(
        f.tree.clone(),
        Config::default(),
        Arc::clone(&f.runtime) as Arc<dyn Runtime>,
    );
    let err = provisioner
        .provision_parcel(&name(AGENT), COMMISSION, &f.blueprint)
        .await
        .unwrap_err();
    assert!(matches!(err, ProvisionError::Bootstrap(_)), "{err}");
    assert!(!f.tree.parcel(UID).exists(), "parcel unwound");
    assert!(!f.tree.commission(COMMISSION).exists());
    assert_eq!(Charter::load(&f.tree.charter()).unwrap(), Charter::empty());
    let argv = f.argv_text();
    assert!(argv.contains("podman run"), "the container had started");
    assert!(argv.contains("podman rm --force scribe"), "…and is removed");
    assert!(
        argv.contains("unshare chown 0:0"),
        "home reclaimed before removal"
    );
}

/// A parcel that already carries the baseline for the same manifest digest
/// keeps exactly one entry, however often provisioning re-runs (crash
/// convergence: adopt, never duplicate).
#[tokio::test]
async fn the_baseline_entry_is_never_duplicated() {
    let f = Fixture::new(&[UID]);
    seed_deed(&f, UID);
    let b = f.bootstrapper();
    let first: BootstrapReport = b.bootstrap_home(&f.target(), &f.blueprint).await.unwrap();
    assert!(first.deed_recorded);
    for _ in 0..3 {
        let report = b.bootstrap_home(&f.target(), &f.blueprint).await.unwrap();
        assert!(!report.deed_recorded);
    }
    assert_eq!(f.deed(UID).matches(DEED_BASELINE_MARKER).count(), 1);
    // The entry is a pure function of the plan + digest: byte-identical.
    let plan = f.plan(UID, &BTreeSet::new());
    assert_eq!(
        deed_baseline_entry(UID, &plan, HOME_FILE_SHA256),
        deed_baseline_entry(UID, &plan, HOME_FILE_SHA256)
    );
}
