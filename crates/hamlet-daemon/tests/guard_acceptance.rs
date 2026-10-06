//! Guard acceptance — the host preconditions themselves, tested (QA04-24).
//!
//! `tests/support/mod.rs` is where the guarded battery decides whether this
//! host can run at all: `userns_precondition` (QA04-07), `work_area_precondition`
//! (QA04-08), and the `mirror_warms` shape helper (QA04-09). Three of those
//! guards were themselves wrong in the **skip** direction before this suite
//! existed (QA04-07 restated twice, QA04-17), because nobody had ever watched
//! them *pass*. A guard that cannot be observed passing is a guess; a guard
//! that guesses "skip" is a silent coverage hole, which is exactly the CI04-01
//! class the sprint spent itself removing.
//!
//! So: this suite pins the guards in both directions.
//!
//! * **Negative** (a non-compliant host must be a recorded SKIP): every case is
//!   driven deterministically by a `podman` **shim** on `PATH` — one that exits
//!   0 without chowning anything (the "silent no-op" host), one that exits
//!   non-zero (the out-of-subuid-window `EINVAL` host of QA04-17) — and by real
//!   tmpfs vs disk directories for QA04-08. No podman container, no network,
//!   no root: the whole file runs in well under a second.
//! * **Positive** (a compliant host must NOT be skipped): the real-podman
//!   case, gated by `podman_env_guard`. This is the assertion that was missing
//!   three times: on a host whose subuids work perfectly,
//!   `userns_precondition()` must return `None`.
//!
//! Mutation-graded (see `scripts/e2e-06-guard-mutations.sh`): reverting any of
//! the fixes below must turn this suite RED.

mod support;

use std::path::{Path, PathBuf};

/// A `podman` stand-in placed on `PATH` by the harness. `mode` selects the
/// host shape being simulated:
// Where shim directories are created.
//
// `scripts/e2e-06-guard-mutations.sh` exports `QA_GUARD_SHIM_DIR` and cleans
// it up; a bare `cargo test --workspace` exports nothing, and this file used
// to `expect()` the variable and fail 8 tests red. That is the CI04-01 shape
// one level down: a gate that cannot be run outside its one script reads as a
// broken product. So an unset variable now means "make your own temp dir" —
// the shims are written *by these tests either way*, and nothing here depends
// on the harness.
fn shim_bin() -> PathBuf {
    if let Ok(dir) = std::env::var("QA_GUARD_SHIM_DIR") {
        return PathBuf::from(dir);
    }
    use std::sync::OnceLock;
    static FALLBACK: OnceLock<PathBuf> = OnceLock::new();
    FALLBACK
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("qa-guard-shims-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("shim dir");
            dir
        })
        .clone()
}

/// The disk-backed directory the QA04-08 arm measures (the real tmpfs is the
/// negative half). Same fallback reasoning as [`shim_bin`].
fn disk_area() -> PathBuf {
    let dir = PathBuf::from(
        std::env::var("QA_GUARD_DISK_AREA").unwrap_or_else(|_| "target/qa-guard-area".to_owned()),
    );
    std::fs::create_dir_all(&dir).expect("QA04-08 disk area");
    dir
}

fn with_shim<T>(name: &str, mode: &str, body: impl FnOnce() -> T) -> T {
    let dir = shim_bin().join(name);
    std::fs::create_dir_all(&dir).expect("shim dir");
    // `podman` is the subuid probe; `df` is the work-area probe. Both are
    // stand-ins for a HOST SHAPE, never for a product behaviour.
    let (podman, df): (Option<&str>, Option<&str>) = match mode {
        "noop" => (Some("#!/bin/sh\nexit 0\n"), None),
        "fail" => (
            Some("#!/bin/sh\necho 'chown: invalid argument' >&2\nexit 1\n"),
            None,
        ),
        // RAM-backed, plenty of free space: the QA04-08 shape whose failure
        // mode is an OOM wearing a tar costume.
        "tmpfs" => (
            None,
            Some("#!/bin/sh\ncase \"$*\" in *--output=avail*) printf 'Avail\\n8589934592\\n';; *--output=fstype*) printf 'Filesystem type\\ntmpfs\\n';; *) printf 'none\\n';; esac\n"),
        ),
        // Disk-backed but out of room: the ENOSPC shape.
        "disk-small" => (
            None,
            Some("#!/bin/sh\ncase \"$*\" in *--output=avail*) printf 'Avail\\n1073741824\\n';; *--output=fstype*) printf 'Filesystem type\\nbtrfs\\n';; *) printf 'none\\n';; esac\n"),
        ),
        // Disk-backed with room: the compliant shape, and the only half of
        // QA04-08 that cannot be faked from the negative side.
        "disk-ok" => (
            None,
            Some("#!/bin/sh\ncase \"$*\" in *--output=avail*) printf 'Avail\\n8589934592\\n';; *--output=fstype*) printf 'Filesystem type\\nbtrfs\\n';; *) printf 'none\\n';; esac\n"),
        ),
        other => panic!("unknown shim mode {other}"),
    };
    // Only `podman` is shimmed in the userns cases; `id -u` must stay honest:
    // the guard's self-validation is exactly "the file's owner is no longer
    // this user", and a lying `id` would let the probe pass itself.
    for (tool, script) in [("podman", podman), ("df", df)] {
        let Some(script) = script else { continue };
        let path = dir.join(tool);
        std::fs::write(&path, script).expect("write shim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }
    body()
}

/// One host probe at a time.
///
/// `PATH` is process-global and rustc's `set_var` is `unsafe` in edition
/// 2024 and racy across test threads; the `df`/`podman` the guards spawn
/// would otherwise observe each other's shims. `scripts/e2e-06` forces
/// `--test-threads=1`, but the file must also be correct under a bare
/// `cargo test --workspace` (which the CI battery runs), so every case that
/// spawns a host tool takes this lock. Cases that touch only the module's
/// pure functions need nothing.
///
/// Deliberately a `std` lock: no test in this file awaits anything, so the
/// async-aware kind would buy nothing.
fn host_probe_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Prepend a shim directory to `PATH` for the duration of `body`. Holds
/// [`host_probe_lock`] for the same reason (see its docs).
fn path_with<T>(dir: &Path, body: impl FnOnce() -> T) -> T {
    let _serial = host_probe_lock();
    struct Restore(std::ffi::OsString);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: serialised by `host_probe_lock` above.
            unsafe { std::env::set_var("PATH", &self.0) }
        }
    }
    let saved = std::env::var_os("PATH").unwrap_or_default();
    let _restore = Restore(saved.clone());
    let joined = format!("{}:{}", dir.display(), saved.to_string_lossy());
    // SAFETY: serialised by `host_probe_lock` above.
    unsafe { std::env::set_var("PATH", joined) }
    body()
}

// --- QA04-07: userns_precondition ------------------------------------------

/// The negative that QA04-17 needed and never got: a `podman unshare chown`
/// that **exits 0 and changes nothing** is not subuid support. The probe file
/// stays owned by this user, so the guard must report the host as unusable.
/// Reverting the self-validation (`m.uid() != own`) turns this RED — and the
/// resulting battery would run the SPEC §4 ownership suites on a host that
/// cannot express them at all.
#[test]
fn a_chown_that_changes_nothing_is_not_subuid_support() {
    with_shim("noop", "noop", || {
        path_with(&shim_bin().join("noop"), || {
            let reason = support::userns_precondition();
            assert!(
                reason.is_some(),
                "a no-op chown must be reported as an unmet QA04-07 precondition, not as support"
            );
            let reason = reason.unwrap();
            assert!(
                reason.contains("QA04-07"),
                "the skip must name its finding so the gate log is diagnosable: {reason}"
            );
            assert!(
                reason.contains("Remedy:"),
                "the skip must print the host remedy (ADR-0032 cl. 1): {reason}"
            );
        })
    });
}

/// A non-zero chown is the QA04-17 host exactly: the probe exited non-zero on
/// every machine because the probe uid was outside the mapped window. That
/// shape is a SKIP, not a pass.
#[test]
fn a_failing_chown_is_an_unmet_precondition() {
    with_shim("fail", "fail", || {
        path_with(&shim_bin().join("fail"), || {
            assert!(
                support::userns_precondition().is_some(),
                "a chown that exits non-zero cannot be subuid support"
            );
        })
    });
}

/// The assertion that has been missing through three wrong guards: on a host
/// that **can** hand a path to a subuid, the guard must return `None`, i.e.
/// the battery must run rather than skip.
///
/// Skipped (recorded) when podman is absent — this is the one case that needs
/// a real host, and it is the one whose absence is the defect.
#[test]
fn a_compliant_host_is_not_skipped() {
    let _serial = host_probe_lock();
    if let Some(why) = support::podman_env_guard() {
        support::skip("a_compliant_host_is_not_skipped", &why);
        return;
    }
    match support::userns_precondition() {
        None => {}
        Some(reason) => panic!(
            "this host CAN hand a path to a subuid, so the guard must not skip the battery. \
             It reported: {reason}\n\
             A guard that skips a compliant host is QA04-07/QA04-17 recurring: a vacuous green \
             that reads as coverage."
        ),
    }
}

// --- QA04-08: work_area_precondition ---------------------------------------

/// **QA04-08 negative #1 — RAM-backed work area.** Driven by a `df` shim so
/// the shape is deterministic on every host (creating a tmpfs needs root, and
/// `/tmp` being tmpfs is a property of *this* machine, not of the code).
///
/// Reverting `is_ram_backed` must turn this RED: without it the guard lets a
/// tmpfs `TMPDIR` through and the battery dies mid-extraction with `exit 137`
/// or `Unexpected EOF`, which reads as a product defect (QA04-08's whole
/// point).
#[test]
fn a_ram_backed_work_area_is_a_recorded_skip() {
    with_shim("tmpfs", "tmpfs", || {
        path_with(&shim_bin().join("tmpfs"), || {
            let reason = support::work_area_precondition(Path::new("/tmp"))
                .expect("a RAM-backed work area must be a recorded SKIP, even with 8 GiB free");
            assert!(
                reason.contains("QA04-08"),
                "must name its finding: {reason}"
            );
            assert!(
                reason.contains("Remedy:"),
                "must print the remedy: {reason}"
            );
            assert!(
                reason.contains("tmpfs"),
                "must name the filesystem it refused: {reason}"
            );
        })
    });
}

/// **QA04-08 negative #2 — disk-backed but out of room.** The ENOSPC class.
/// Dropping the `>= 4 GiB` floor must turn this RED.
#[test]
fn a_disk_backed_work_area_without_room_is_a_recorded_skip() {
    with_shim("disk-small", "disk-small", || {
        path_with(&shim_bin().join("disk-small"), || {
            let reason = support::work_area_precondition(Path::new("/tmp"))
                .expect("1 GiB free is below the QA04-08 floor and must be a SKIP");
            assert!(
                reason.contains("QA04-08"),
                "must name its finding: {reason}"
            );
            assert!(
                !reason.contains("RAM-backed"),
                "this is the space floor, not the tmpfs finding: {reason}"
            );
        })
    });
}

/// **QA04-08 positive — the same shimmed path, compliant shape.** A guard that
/// only ever says "no" is a guard nobody trusts; this is the other half, and
/// it is deterministic (it does not depend on the host's real `/tmp`).
#[test]
fn a_disk_backed_work_area_with_room_is_not_skipped() {
    with_shim("disk-ok", "disk-ok", || {
        path_with(&shim_bin().join("disk-ok"), || {
            assert_eq!(
                support::work_area_precondition(Path::new("/tmp")),
                None,
                "a disk-backed area with 8 GiB free is compliant and must not be skipped"
            );
        })
    });
}

/// The positive: a real disk-backed directory with room is compliant and must
/// not be skipped. This half cannot be shimmed — it is the claim that a
/// guard did not cry wolf.
#[test]
fn the_work_area_guard_separates_ram_from_disk() {
    let _serial = host_probe_lock();
    // /tmp is tmpfs on the overwhelming majority of hosts and is the default
    // TMPDIR; if this host differs, fall back to whatever temp_dir() is and
    // assert only the disk half.
    let tmp = Path::new("/tmp");
    if support::device_of(tmp).is_some() {
        let reason = support::work_area_precondition(tmp);
        // If this host's /tmp is disk-backed, `None` is a legitimate pass.
        if let Some(r) = reason {
            assert!(r.contains("QA04-08"), "a skip must name its finding: {r}");
        }
    }
    let disk = disk_area();
    std::fs::create_dir_all(&disk).expect("disk work area");
    let reason = support::work_area_precondition(&disk);
    assert!(
        reason.is_none(),
        "a disk-backed area with >= 4 GiB free is compliant and must not be skipped: {reason:?}"
    );
}

/// `avail_bytes` must never report a number it did not measure. A guard that
/// invents headroom is worse than one that admits ignorance, so the
/// unverifiable case is a SKIP with its own wording, not a pass.
#[test]
fn an_unmeasurable_work_area_is_reported_not_assumed() {
    let _serial = host_probe_lock();
    let ghost = PathBuf::from("/nonexistent-qa-guard-area/deeper");
    // The parent does not exist either: `df` cannot answer.
    assert!(
        support::avail_bytes(&ghost).is_none(),
        "no such directory cannot report free space"
    );
}

// --- QA04-09: the mirror-shape helper --------------------------------------

/// Pinned so the next reader knows what the helper actually does: it measures
/// the **nearest existing ancestor**, so a not-yet-created path reports the
/// device it *will* land on rather than `None`. Harmless for QA04-09 (the tree
/// is created before the branch is taken) and strictly more useful than a
/// `None`, but it is a claim about a path that does not exist yet, so it is
/// pinned here rather than left implicit.
#[test]
fn device_measurement_falls_back_to_the_nearest_existing_ancestor() {
    assert_eq!(
        support::same_device(Path::new("/"), Path::new("/")),
        Some(true),
        "a path compared with itself is the same device by definition"
    );
    assert_eq!(
        support::same_device(Path::new("/"), Path::new("/nonexistent-qa-guard-area/x")),
        Some(true),
        "an uncreated path measures its nearest existing ancestor's device"
    );
}

/// Only EXDEV may excuse an empty mirror (ADR-0013 cl. 3). The classifier is
/// the *entire* defence of the QA04-09 split arm, so its negative case is
/// asserted directly: a digest or permission failure must NOT be excused.
#[test]
fn only_exdev_excuses_an_empty_mirror() {
    for good in [
        "cross-device link",
        "Invalid cross-device link",
        "CROSS-DEVICE",
    ] {
        assert!(
            support::is_exdev(good),
            "a real EXDEV publish failure must classify as EXDEV: {good}"
        );
    }
    for bad in [
        "sha256 mismatch: expected abc, got def",
        "permission denied",
        "no space left on device",
        "",
    ] {
        assert!(
            !support::is_exdev(bad),
            "a non-EXDEV failure must not be excused by the QA04-09 branch: {bad:?}"
        );
    }
}

// --- QA04-26: per-run container names --------------------------------------

/// The scoped name must be a **usable** container name, not just a different
/// string: it is passed through the §4 sanitizer (it becomes a charter slot
/// name, a brief filename and a `podman --name`) on every path.
#[test]
fn a_scoped_container_name_is_sanitizer_safe_and_still_distinct() {
    for base in [
        "qa-escape-a",
        "qa-escape-integrity",
        "hamlet-boot-a",
        "hamlet-freight-c",
        "hamlet-guard-b",
        "hamlet-supervise-smoke",
        "hamlet-podman-smoke",
        "qa-mayor-agent",
    ] {
        let name = support::scoped(base);
        assert_ne!(
            name, base,
            "{base} was not suffixed: two overlapping batteries would share it"
        );
        assert!(
            name.starts_with(base),
            "{name} must stay recognisable as {base}"
        );
        assert!(
            hamlet_core::sanitize::sanitize_name(&name).is_ok(),
            "{name} must pass the §4 sanitizer (sanitize_name)"
        );
        assert!(
            name.chars().count() <= hamlet_core::sanitize::MAX_NAME_LEN,
            "{name} exceeds the 64-char name limit even scoped"
        );
    }
    // Distinct bases stay distinct, and the suffix is uniform.
    let a = support::scoped("hamlet-boot-a");
    let b = support::scoped("hamlet-boot-b");
    assert_ne!(a, b);
    assert_eq!(support::run_suffix(), support::run_suffix());
}

/// The **identity** name is not a test name. `manor` is `MAYOR_IDENTITY`
/// (ADR-0028): the product provisions it, asserts its hostname, and a scoped
/// variant would be a different product. This pins the *invariant* — the
/// scoped form is the only shape any guarded suite may build, and the identity
/// string is what the guarded suites must still contain verbatim.
#[test]
fn the_mayor_identity_name_is_never_a_scoped_name() {
    let identity = hamlet_daemon::mayor::MAYOR_IDENTITY;
    assert_eq!(identity, "manor");
    assert_ne!(
        support::scoped(identity),
        identity,
        "if this ever becomes equal, `scoped` has started rewriting identity names"
    );
    // The product derives container names from sanitized identities, so the
    // identity must itself be sanitizer-legal (it is passed to `podman run`).
    assert!(hamlet_core::sanitize::sanitize_name(identity).is_ok());
}

// --- QA04-27: a dead parcel is a host precondition -------------------------

/// The classifier must be *narrow*. A §11.4/§4 failure that does not mention a
/// gone container stays a product failure — otherwise the suite would learn to
/// skip itself exactly when it matters.
#[test]
fn only_a_dead_parcel_is_classified_as_a_host_precondition() {
    for err in [
        "secret material found in the home or the mirror (SPEC §11.4)",
        "ro escape attempt must fail: `touch /var/git/escape` (SPEC §4)",
        "boom",
        "",
    ] {
        assert_eq!(
            support::dead_parcel_precondition("some-container", err),
            None,
            "{err:?} must not be classified as a host precondition"
        );
    }
}

/// The positive arm: each of podman's wordings for a container that is gone or
/// not running must classify, and the skip must name the finding and print a
/// remedy (ADR-0032 cl. 1). Uses the `fail`/`noop` shim so `podman inspect`
/// cannot answer `{{.State.Running}}` — an unreadable state must still skip,
/// never fail the suite.
#[test]
fn a_dead_parcel_is_reported_as_a_host_precondition_with_a_remedy() {
    for err in [
        "podman: no such container d268cc",
        "Error: no container with ID d268cc found in database",
        "can only create exec sessions on running containers: container state improper",
        "Error: no such object",
    ] {
        with_shim("deadparcel", "fail", || {
            path_with(&shim_bin().join("deadparcel"), || {
                let reason = support::dead_parcel_precondition("qa-escape-integrity", err)
                    .unwrap_or_else(|| panic!("{err:?} must classify as QA04-27"));
                assert!(reason.contains("QA04-27"), "{reason}");
                assert!(reason.contains("Remedy:"), "{reason}");
                assert!(
                    reason.contains("host precondition"),
                    "the wording must say host precondition, not a defect: {reason}"
                );
            })
        });
    }
}

/// `podman inspect` answering `true` is the one input that must NOT be
/// classified away: if the container is alive, whatever podman said is a real
/// product failure. Driven with a `df`-style shim extended to answer inspect.
#[test]
fn a_live_container_is_never_classified_as_a_host_precondition() {
    let dir = shim_bin().join("liveparcel");
    std::fs::create_dir_all(&dir).expect("shim dir");
    let path = dir.join("podman");
    std::fs::write(
        &path,
        "#!/bin/sh\ncase \"$*\" in *State.Running*) echo true;; *) exit 1;; esac\n",
    )
    .expect("write shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path_with(&dir, || {
        assert_eq!(
            support::container_running("qa-escape-a"),
            Some(true),
            "a live parcel reads as running"
        );
        assert_eq!(
            support::dead_parcel_precondition("qa-escape-a", "podman: no such container d268cc"),
            None,
            "a container that podman reports as running is not a QA04-27 host shape: \
             whatever else failed is a product failure"
        );
    });
}
