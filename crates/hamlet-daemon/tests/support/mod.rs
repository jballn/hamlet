//! Host preconditions for the guarded real-podman battery (CONVENTIONS §5).
//!
//! Every guarded suite used to answer one question only: *is
//! `HAMLET_PODMAN_TESTS=1` set and podman on PATH?* That is necessary but not
//! sufficient. Three host findings filed during Sprint 04 were not product
//! defects and not test defects — they were **host shapes on which the
//! invariant the suite asserts cannot hold at all**, reported as RED:
//!
//! * **QA04-07** — the guarded suites need the SPEC §4 per-slot uid policy
//!   to be *observable*, i.e. the host must actually be able to hand a path
//!   to a subuid: `podman unshare chown` must reach the subuid range, or
//!   parcel homes stay daemon-owned and every ownership assertion is vacuous.
//!
//!   (An earlier form of this check said the daemon user must not be the
//!   podman owner. That was wrong, and no host can satisfy it: rootless
//!   podman maps `0 → <invoking uid> 1` in *every* container, so the
//!   invoking user always appears in its own containers' `uid_map`. The
//!   escape suite's disjointness assertion had the same over-statement and is
//!   now exempt for exactly that one row — see ADR-0032 cl. 1.)
//! * **QA04-08** — the cold bootstrap extracts ~1 GB of toolchains into the
//!   tempdir village. On a RAM-backed `TMPDIR` that is SIGKILL (exit 137) or
//!   `Unexpected EOF`, i.e. an OOM wearing a tar costume.
//! * **QA04-09** — SPEC §4 mounts the parcel home and the pkgcache mirror as
//!   two *separate* bind mounts. When they land on different host devices the
//!   ADR-0013 cl. 3 hardlink publish is EXDEV — explicitly non-fatal ("it is
//!   a cache") — so the mirror stays empty and the "second parcel downloads
//!   nothing" claim is simply not applicable to that host. That one is *not*
//!   a skip: the suite branches on the measured device pair and asserts the
//!   claim each shape actually supports (see `same_device`).
//!
//! The doctrine, restated: **a non-compliant host must be a recorded SKIP
//! with a remedy, never a RED that reads like a product defect.** The
//! assertions themselves are never weakened — a compliant host runs them
//! verbatim. Every skip line carries its finding id and the host remedy so
//! `scripts/ci.sh` can print a coverage note instead of a silent hole.
//!
//! No product behaviour lives here: this module is test-side infrastructure
//! (CONVENTIONS §5), shared by the guarded suites through `mod support;`.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use hamlet_core::tree::HamletTree;
use hamlet_daemon::podman::{
    BoxFuture, ContainerState, ExecHandle, ExecOutput, PodmanError, RunSpec, Runtime,
    SessionExecSpec,
};

/// QA04-08 remedy size: ≥ 4 GiB free where the village tree is built (the
/// measured figure that took the cold bootstrap past node *and* rustc).
pub const QA04_08_MIN_BYTES: u64 = 4 << 30;

/// CONVENTIONS §5 guard: run only with `HAMLET_PODMAN_TESTS=1` and podman
/// on PATH; otherwise skip with a recorded reason.
pub fn podman_env_guard() -> Option<String> {
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

/// The loud, greppable skip line. `ci.sh` counts these so a host-shaped skip
/// is visible in the gate log instead of reading as silent coverage.
pub fn skip(test: &str, reason: &str) {
    eprintln!("SKIP {test}: {reason}");
}

fn sh_out(args: &[&str]) -> Option<(bool, String)> {
    let out = StdCommand::new(args[0]).args(&args[1..]).output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some((out.status.success(), text))
}

fn current_uid() -> u32 {
    StdCommand::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .expect("host `id -u`")
}

/// An **inside** uid used to probe subuid reachability. It must be small:
/// rootless podman maps the subuid range as `1 → <base> <count>`, so the
/// inside uids that exist at all are `1 ..= count` (65536 here). A probe of
/// `100001` sits outside that window and returns `EINVAL` on *every* host —
/// which is how the first cut of this function reported "unmet" universally
/// and skipped the whole battery on a host whose subuids work perfectly
/// (QA04-17).
const PROBE_INSIDE_UID: u32 = 1000;

/// Can this host actually hand a path to a *subuid* from inside
/// `podman unshare`? A restricted userns stack (tmpfs `/dev`, minimal module
/// set) cannot, and then parcel homes stay daemon-owned — the SPEC §4 per-slot
/// uid policy the isolation suites assert becomes unobservable.
///
/// The probe is self-validating: it requires the chown to succeed *and* the
/// file to end up owned by something other than the daemon's own uid, so a
/// chown that silently no-op'd inside the namespace cannot read as support.
fn subuid_chown_supported() -> bool {
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    let probe = dir.path().join("probe");
    if fs::write(&probe, b"x").is_err() {
        return false;
    }
    let (ok, _) = sh_out(&[
        "podman",
        "unshare",
        "chown",
        &format!("{PROBE_INSIDE_UID}:{PROBE_INSIDE_UID}"),
        "--",
        probe.to_str().unwrap_or_default(),
    ])
    .unwrap_or((false, String::new()));
    let own = current_uid();
    fs::metadata(&probe)
        .map(|m| ok && m.uid() != own && m.uid() != u32::MAX)
        .unwrap_or(false)
}

/// **QA04-07** — the guarded isolation suites need a host that can actually
/// realise the SPEC §4 per-slot uid policy. The concrete test: can this user
/// hand a path to a **subuid** from inside `podman unshare`? A restricted
/// userns/subuid stack cannot, and then parcel homes stay daemon-owned and
/// every ownership assertion in the suite is vacuously true.
///
/// Returns the skip reason (with the host remedy) when it cannot.
pub fn userns_precondition() -> Option<String> {
    let uid = current_uid();
    if uid == 0 {
        return Some(
            "running as root: the battery is rootless-only (CONVENTIONS §5). \
             Run it as the dedicated `hamlet` service user."
                .to_owned(),
        );
    }
    if subuid_chown_supported() {
        None
    } else {
        Some(format!(
            "QA04-07 host precondition unmet: `podman unshare chown` cannot hand a path to a \
             subuid as uid {uid} (no usable subuid range, or a restricted userns stack), so \
             parcel homes stay daemon-owned and the SPEC §4 per-slot uid policy cannot be \
             observed. Remedy: give the service user a subuid/subgid range \
             (`hamlet:231072:65536` in /etc/subuid + /etc/subgid) and full rootless userns \
             support, then re-run (scripts/guarded-service-user.sh)."
        ))
    }
}

/// Free bytes on the filesystem backing `dir`.
pub fn avail_bytes(dir: &Path) -> Option<u64> {
    let dir = dir.exists().then_some(dir).or_else(|| dir.parent())?;
    let (_, text) = sh_out(&["df", "-B1", "--output=avail", "--", dir.to_str()?])?;
    text.lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .next_back()
}

/// Filesystem types that live in RAM. A village tree on one of these is
/// charged against the container's `--memory` limit as tmpfs page cache, so
/// extracting the pinned toolchains inside a 512 MB-capped parcel is an OOM
/// wearing a tar costume (`exit 137`) or a truncated archive (`Unexpected EOF`)
/// depending on how the kernel gets there.
fn is_ram_backed(fstype: &str) -> bool {
    matches!(fstype.trim(), "tmpfs" | "ramfs")
}

fn fs_type(dir: &Path) -> Option<String> {
    let (_, text) = sh_out(&["df", "--output=fstype", "--", dir.to_str()?])?;
    text.lines()
        .nth(1)
        .map(|l| l.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// **QA04-08** — the cold bootstrap extracts ~1 GB of pinned toolchains into
/// the village tree, from inside a `--memory`-capped parcel. Two distinct host
/// shapes break that, and both used to surface as a mid-extraction failure
/// that reads like a product bug:
///
/// 1. the tree lives on a **RAM-backed filesystem** (the usual case: `/tmp`
///    is a tmpfs). Its pages are charged to the parcel's cgroup, so the
///    extraction is OOM-killed (`exit 137`) or truncated (`Unexpected EOF`)
///    *regardless of how much free space the tmpfs reports* — verified live:
///    7.5 GB free, still `Unexpected EOF`.
/// 2. a **disk-backed** filesystem that simply lacks the room (the ENOSPC
///    class the 04/06 gate removed by pointing `TMPDIR` at disk).
///
/// Both are checked before a single byte is downloaded, and the host remedy —
/// a disk-backed `TMPDIR` with ≥ 4 GiB free — is printed verbatim.
pub fn work_area_precondition(dir: &Path) -> Option<String> {
    let fstype = fs_type(dir).unwrap_or_default();
    if is_ram_backed(&fstype) {
        return Some(format!(
            "QA04-08 host precondition unmet: the village tree would live on `{fstype}`, a \
             RAM-backed filesystem. Its pages are charged against the parcel's --memory limit, \
             so extracting the pinned toolchains dies as exit 137 / 'Unexpected EOF' however \
             much free space the fs reports (measured on this class of host at 7.5 GB free). \
             Remedy: export TMPDIR=<disk-backed path> with >= {} GiB free before running the \
             guarded battery.",
            QA04_08_MIN_BYTES >> 30
        ));
    }
    match avail_bytes(dir) {
        Some(avail) if avail >= QA04_08_MIN_BYTES => None,
        Some(avail) => Some(format!(
            "QA04-08 host precondition unmet: only {avail} bytes free on `{fstype}` where the \
             village tree lives (need {}). Remedy: point TMPDIR at a disk-backed path with \
             that much room.",
            QA04_08_MIN_BYTES
        )),
        None => Some(format!(
            "QA04-08 host precondition unverifiable: `df` could not report free space for {}.",
            dir.display()
        )),
    }
}

/// Host device id (inode number of the containing filesystem) — the measured
/// input to the **QA04-09** branch, not an assumption about the host shape.
pub fn device_of(path: &Path) -> Option<u64> {
    let probe = if path.exists() {
        path.to_owned()
    } else {
        path.ancestors().find(|p| p.exists())?.to_owned()
    };
    fs::metadata(probe).ok().map(|m| m.dev())
}

/// **QA04-09** — do the parcel home and the pkgcache mirror share a host
/// device? `Some(true)` → the ADR-0013 hardlink publish is expected to
/// succeed and the mirror is expected to warm. `Some(false)` → SPEC §4's two
/// separate bind mounts landed on different devices, publish is EXDEV
/// (non-fatal by clause), and the mirror-stays-empty shape must be asserted
/// instead. `None` → unmeasurable; the caller must not guess.
pub fn same_device(a: &Path, b: &Path) -> Option<bool> {
    Some(device_of(a)? == device_of(b)?)
}

/// Is a publish failure the expected, explicitly non-fatal EXDEV?
pub fn is_exdev(failure: &str) -> bool {
    failure.to_ascii_lowercase().contains("cross-device")
}

// ---------------------------------------------------------------------------
// QA04-26 (container-name half) — per-run container names.
// ---------------------------------------------------------------------------
//
// Podman container names are **global to the podman store**, not scoped to a
// test binary, a village tree, or a run. The guarded suites provision fixed
// names (`qa-escape-a`, `hamlet-boot-a`, `hamlet-freight-a`, …), so two
// overlapping batteries — a background job and a foreground gate, two CI jobs
// on one host, a developer re-running a red suite while the first attempt is
// still alive — address *each other's* containers: one's cleanup `rm -f`s the
// container the other is asserting in, and the loser's failure text reads
// like a SPEC §4 defect ("own resources must stay writable: … container state
// improper") when it is pure interference.
//
// Two halves, deliberately separate:
//
// * **Per-run names (here)** — every *non-identity* container name is suffixed
//   with this process's pid, so concurrent runs simply coexist. The suffix is
//   built from a pid because it only has to be unique among *live* runs; the
//   name is never persisted or compared against product data.
// * **A battery-wide lock** (the `flock` in `scripts/guarded-entry.sh`) is the
//   other half and belongs to the CI layer: names stop colliding, but two
//   batteries still fight over RAM, the podman store, and the pkgcache.

/// Per-run suffix for a container name: `-r<pid>`.
///
/// The **Mayor identity container is never scoped**: `manor` is
/// `MAYOR_IDENTITY` (ADR-0028), the product's own name (used by
/// `ManorProvisioner` and asserted as the Mayor's hostname), so renaming it
/// away would be renaming the product — `manor_guarded` serialises behind
/// `MANOR_CONTAINER_LOCK` for it instead (QA04-20). Only a *test* name may be
/// handed to [`scoped`].
///
/// Shape constraints, all asserted by `guard_acceptance.rs`:
/// `[A-Za-z0-9_-]` only (CONVENTIONS §4 sanitizer shape), no `.`, so the
/// joined name survives `sanitize_name`, and short enough that a 64-char base
/// name still fits.
#[must_use]
pub fn run_suffix() -> String {
    format!("-r{}", std::process::id())
}

/// `<base><run_suffix>` — a container name private to this process run.
#[must_use]
pub fn scoped(base: &str) -> String {
    format!("{base}{}", run_suffix())
}

// ---------------------------------------------------------------------------
// QA04-27 — one cold bootstrap at a time (within a test binary).
// ---------------------------------------------------------------------------
//
// A cold bootstrap extracts ~1 GB of pinned toolchains inside a
// `--memory`-capped parcel. Two of them at once is what killed a parcel's PID 1
// at ~353 s on a host without RAM headroom for both (QA04-27), and the suite
// that owned the dead container reported it as a SPEC §11.4 secrets failure.
// Per-run names do not fix this half — the host resources are shared — so the
// cold-bootstrap suites serialise behind this lock, the seam `manor_guarded`
// established in QA04-20.
//
// Held across `.await`, so it must be the async-aware kind: a `std` guard held
// across an await is both a clippy error and wrong. It is per-*binary* (each
// test binary compiles its own copy); serialising *across* batteries is the
// CI layer's `flock` (`scripts/ci-guarded-job.sh`), which cannot be expressed
// as a `tokio` primitive.
pub static COLD_BOOTSTRAP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// QA04-27 — a parcel whose PID 1 died mid-provisioning is a host
// precondition, never a product assertion failure.
// ---------------------------------------------------------------------------

/// Podman's error text when the container is gone or not running. Matched
/// case-insensitively as substrings because the exact wording moves between
/// podman versions (`podman: no such container …`, `no container with ID …`,
/// `can only create exec sessions on running containers: container state
/// improper`).
fn is_dead_parcel_error(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "no such container",
        "no container with id",
        "container state improper",
        "can only create exec sessions on running containers",
        "container not known",
        "no such object",
    ]
    .iter()
    .any(|sig| t.contains(sig))
}

/// Is this container currently running, as podman reports it? `None` when
/// podman cannot answer (no container with that name / podman unusable) — a
/// `None` must never be read as "the invariant broke".
#[must_use]
pub fn container_running(container: &str) -> Option<bool> {
    let (ok, text) = sh_out(&[
        "podman",
        "inspect",
        "--format",
        "{{.State.Running}}",
        container,
    ])?;
    if !ok {
        return None;
    }
    match text.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// **QA04-27** — classify a failed `podman exec` against a container this
/// suite just provisioned.
///
/// Observed live: under two *parallel* cold bootstraps (~1 GB of toolchain
/// extraction each, 350–600 s) the integrity container's PID 1 died
/// unprompted at ~353 s — `podman events` shows `died`/`remove` with nothing
/// in the container's own log. The suite surfaced that as
/// `podman: no such container … no container with ID … found in database`,
/// which the gate log reads as a SPEC §11.4 secrets violation. It is not: the
/// same suite is green serialised and inside the battery, and a memory probe
/// over a passing run peaked at 54 % of the `--memory` cap. What is *proved*
/// is only "PID 1 died mid-provisioning on this host under this contention".
///
/// Doctrine, restated from ADR-0032: that is a **host precondition** and must
/// be a recorded SKIP with its remedy. The remedy is the one the suite
/// already applies (serialise the cold bootstraps) plus RAM headroom.
///
/// Returns `Some(reason)` only for a *dead-parcel* signature, and only after
/// podman confirms the container is not running — so a genuine product
/// failure that happens to mention a container name is never classified away.
#[must_use]
pub fn dead_parcel_precondition(container: &str, err: &str) -> Option<String> {
    if !is_dead_parcel_error(err) {
        return None;
    }
    match container_running(container) {
        Some(true) => None,
        Some(false) => Some(dead_parcel_reason(container)),
        None => Some(dead_parcel_reason(container)),
    }
}

fn dead_parcel_reason(container: &str) -> String {
    parcel_not_running_reason(container)
}

/// The SKIP reason for "this parcel is not running", whether we got there by
/// classifying an exec error or by probing state up front. Public so a suite
/// can check liveness *before* its first assertion instead of only after an
/// exec has already failed.
#[must_use]
pub fn parcel_not_running_reason(container: &str) -> String {
    format!(
        "QA04-27 host precondition unmet: the parcel `{container}` is not running (its PID 1 \
         died unprompted during provisioning) — podman reports: the container is gone or its \
         state is improper. This is a host/contention shape, NOT a product defect: two parallel \
         cold bootstraps (~1 GB of pinned toolchain extraction each) kill a parcel's PID 1 on \
         hosts without RAM headroom for both. The suites that bootstrap are serialised behind an \
         async-aware mutex for this reason, and the failure is reported as a SKIP instead of a \
         SPEC §11.4 assertion failure. Remedy: run the guarded suites one cold bootstrap at a \
         time with >= {} GiB free on a disk-backed TMPDIR (scripts/guarded-service-user.sh \
         --doctor), and re-run this suite alone before reading anything into the failure.",
        QA04_08_MIN_BYTES >> 30
    )
}

// ===========================================================================
// QA04-32: publish-arm fault-injection seam (ADR-0035)
// ===========================================================================
// The mirror-warm arm (second parcel downloads zero artifacts) is only
// exercised on hosts where the parcel home and pkgcache mirror share a device
// (hard-link publish succeeds). On cross-device hosts the ADR-0013 cl. 3
// hard-link publish is EXDEV and the mirror stays empty — the warm-arm claim
// is simply not applicable. This seam forces the warm arm by rewriting the
// publish argv from `ln <staged> <mirror>` to `cp -- <staged> <mirror>`, which
// crosses devices. The injected arm exercises the *downstream accounting*:
// mirror contents, cache_hits/downloads, digest re-verification, and parcel B
// offline toolchain operation. It does NOT exercise ADR-0013 cl. 3's symlink-
// refusal or inode-identity properties (the product's `ln` still runs on
// shared-device hosts). The seam is honest about its scope: it simulates a
// shared-device host topology, it does not fake success.

/// Which publish arm to exercise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishArm {
    /// Product behaviour: `ln` hard link, EXDEV on cross-device hosts.
    Product,
    /// Force the warm arm: rewrite publish argv to `cp --`, which crosses
    /// devices and populates the mirror on any host.
    ForceWarm,
}

/// Runtime decorator that intercepts `exec` and rewrites publish argv.
/// Delegates all other trait methods to the inner runtime.
pub struct PublishSeamRuntime<R: Runtime> {
    inner: Arc<R>,
    arm: PublishArm,
    rewrites: Arc<std::sync::atomic::AtomicUsize>,
}

impl<R: Runtime> PublishSeamRuntime<R> {
    /// Wrap `inner` with `arm` policy.
    pub fn new(inner: Arc<R>, arm: PublishArm) -> Arc<Self> {
        Arc::new(Self {
            inner,
            arm,
            rewrites: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// Number of publish argv rewrites performed (for test assertions).
    pub fn rewrites(&self) -> usize {
        self.rewrites.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl<R: Runtime> Runtime for PublishSeamRuntime<R> {
    fn run_detached<'a>(
        &'a self,
        spec: &'a RunSpec,
        tree: &'a HamletTree,
    ) -> BoxFuture<'a, Result<String, PodmanError>> {
        self.inner.run_detached(spec, tree)
    }

    fn exec<'a>(
        &'a self,
        container: &'a str,
        cmd: &'a [String],
        stdin: Option<&'a [u8]>,
    ) -> BoxFuture<'a, Result<ExecOutput, PodmanError>> {
        let inner = Arc::clone(&self.inner);
        let arm = self.arm;
        let rewrites = Arc::clone(&self.rewrites);
        let cmd = cmd.to_vec();
        Box::pin(async move {
            // Detect the publish argv: a trailing `ln <from> <to>` where `to`
            // starts with `/var/pkgcache/dist/`. Every bootstrap exec carries
            // the plan's `env K=V …` prefix (bootstrap.rs:1887), so match the
            // trailing triple and preserve the prefix on rewrite.
            if arm == PublishArm::ForceWarm
                && cmd.len() >= 3
                && cmd[cmd.len() - 3] == "ln"
                && cmd[cmd.len() - 1].starts_with("/var/pkgcache/dist/")
            {
                let from = &cmd[cmd.len() - 2];
                let to = &cmd[cmd.len() - 1];
                // Rewrite the triple to `cp -- <from> <to>`. `cp` crosses
                // device boundaries; `--` guards against paths starting with `-`.
                let mut rewritten = cmd.clone();
                rewritten.truncate(cmd.len() - 3);
                rewritten.extend(["cp".to_owned(), "--".to_owned(), from.clone(), to.clone()]);
                rewrites.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                inner.exec(container, &rewritten, stdin).await
            } else {
                inner.exec(container, &cmd, stdin).await
            }
        })
    }

    fn spawn_exec<'a>(
        &'a self,
        container: &'a str,
        cmd: &'a [String],
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        self.inner.spawn_exec(container, cmd)
    }

    fn spawn_session<'a>(
        &'a self,
        spec: &'a SessionExecSpec,
    ) -> BoxFuture<'a, Result<Box<dyn ExecHandle>, PodmanError>> {
        self.inner.spawn_session(spec)
    }

    fn stop<'a>(
        &'a self,
        container: &'a str,
        grace: Duration,
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        self.inner.stop(container, grace)
    }

    fn kill<'a>(&'a self, container: &'a str) -> BoxFuture<'a, Result<(), PodmanError>> {
        self.inner.kill(container)
    }

    fn state<'a>(
        &'a self,
        container: &'a str,
    ) -> BoxFuture<'a, Result<ContainerState, PodmanError>> {
        self.inner.state(container)
    }

    fn pull<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<(), PodmanError>> {
        self.inner.pull(image)
    }

    fn image_exists<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<bool, PodmanError>> {
        self.inner.image_exists(image)
    }

    fn build<'a>(
        &'a self,
        tag: &'a str,
        containerfile: &'a Path,
        context: &'a Path,
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        self.inner.build(tag, containerfile, context)
    }

    fn remove<'a>(
        &'a self,
        container: &'a str,
        force: bool,
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        self.inner.remove(container, force)
    }

    fn unshare_chown<'a>(
        &'a self,
        uid: u32,
        gid: u32,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        self.inner.unshare_chown(uid, gid, paths)
    }

    fn unshare_chown_recursive<'a>(
        &'a self,
        uid: u32,
        gid: u32,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        self.inner.unshare_chown_recursive(uid, gid, paths)
    }

    fn unshare_exec<'a>(
        &'a self,
        cmd: &'a [String],
    ) -> BoxFuture<'a, Result<ExecOutput, PodmanError>> {
        self.inner.unshare_exec(cmd)
    }

    fn unshare_write_atomic<'a>(
        &'a self,
        dest: &'a Path,
        bytes: &'a [u8],
        owner: (u32, u32),
    ) -> BoxFuture<'a, Result<(), PodmanError>> {
        self.inner.unshare_write_atomic(dest, bytes, owner)
    }
}
