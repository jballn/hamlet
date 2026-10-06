# ADR-0035 — Publish-arm fault-injection seam for the mirror-warm branch

Status: Accepted
Sprint: 05-mayor-loop · QA04-32 (owner: QA)
Amends: ADR-0013 cl. 3 (publish is `ln`, EXDEV on cross-device hosts)
Refs: SPEC §4 (mount table), ADR-0034 cl. 4/6 (test assertions); `tests/support/mod.rs`

## Context

QA04-32: the `mirror_warms` warm arm (second parcel downloads zero artifacts,
mirror holds all pinned artifacts) is only exercised on hosts where the parcel
home and the pkgcache mirror share a device — the ADR-0013 cl. 3 hard-link
publish (`ln <staged> <mirror>`) succeeds. On cross-device hosts (this CI
host, and any host where `market/pkgcache/` is a separate bind-mount
filesystem) the publish is EXDEV, the mirror legitimately stays empty, and the
zero-download claim is simply not applicable. The warm arm was **asserted
nowhere**, on this host or any host the suite has run on.

The contract gap: the product's `ln` publish is correct and normative
(ADR-0013 cl. 3 — symlink refusal, inode identity), but the *downstream
accounting* it enables (mirror file count, cache_hits/downloads, digest
re-verification, parcel B offline toolchain operation) was pinned only in the
shape the host happened to support. A host that takes the EXDEV arm never
exercises the warm-arm assertions at all.

## Decision

1. **A test-only fault-injection seam rewrites the publish argv.** A new
   `PublishSeamRuntime` decorator (in `tests/support/mod.rs`) wraps the real
   `Runtime` and intercepts `exec`. When the argv matches the publish pattern
   — exactly `["ln", <from>, <to>]` where `<to>` starts with
   `/var/pkgcache/dist/` — it rewrites to `["cp", "--", <from>, <to>]`.
   `cp` crosses device boundaries, so the mirror populates on any host.

2. **The seam is honest about its scope.** It simulates a shared-device host
   topology; it does NOT fake success. The injected arm runs a real `cp`
   exec, creates real files in the mirror, and the downstream accounting
   (mirror file count, `cache_hits == 7`, `downloads == 0`,
   `verified_artifacts` digest re-verification, parcel B toolchain operation)
   runs against those real files. What it does NOT exercise:
   * ADR-0013 cl. 3's symlink-refusal property (`ln` refuses an existing name;
     `cp --` overwrites).
   * ADR-0013 cl. 3's inode-identity property (`ln` publishes the exact
     verified inode; `cp` creates a new inode).
   These are product `ln` properties, separately pinned by the product's
   `chown_recursive_argv_pins_no_dereference` and the guarded suite's
   symlink-escalation test (`manor_guarded::podman_manor_home_brace...`).

3. **A new guarded test forces the warm arm.**
   `podman_bootstrap_forced_warm_mirror_makes_second_parcel_offline` (in
   `bootstrap_guarded.rs`) uses `PublishArm::ForceWarm` and asserts the full
   warm-arm contract: mirror holds `ARTIFACT_COUNT` files, parcel B has zero
   downloads / all cache hits / re-verified digests, and B's toolchains run
   and are owned by B's UID. The test is serialised behind
   `COLD_BOOTSTRAP_LOCK` (QA04-27) and guarded by the same host preconditions
   as the main bootstrap test.

4. **The seam is a test-side decorator, not a product change.** It implements
   the full `Runtime` trait (16 methods, 15 delegated), so it can be swapped
   in anywhere a `Runtime` is accepted. The product's publish path is
   unchanged; the ADR-0013 `ln` primitive remains normative. The seam lives
   entirely in `tests/support/mod.rs` and `bootstrap_guarded.rs`.

## Consequences

- The warm-arm contract is now exercised on **every** guarded run, regardless
  of host device topology. A regression in the downstream accounting
  (e.g., `cache_hits` not incremented, `verified_artifacts` not
  re-checked, parcel B toolchain not digest-verified) will fail the gate
  loudly on all hosts.
- The seam adds ~one cold bootstrap runtime (~350–600 s) to the guarded
  battery when run. This is the honest cost of pinning a contract arm that
  was previously unexercised. The suite is `#[ignore]`d and only runs in the
  guarded battery (`HAMLET_PODMAN_TESTS=1` + CI step 5), not on every
  `cargo test`.
- If CI cost becomes prohibitive, the test can be gated behind an additional
  env var (e.g., `HAMLET_GUARDED_FORCE_WARM=1`) without changing the seam
  or the assertions. The default remains: run it in the guarded battery.
- ADR-0013 cl. 3 remains the normative publish primitive. The seam's
  `cp --` is a *simulation* of the shared-device case, not a replacement.
  The product's `ln` still runs on shared-device hosts and is still
  pin-tested there.

## Verification

```bash
# In the guarded battery (HAMLET_PODMAN_TESTS=1, podman on PATH):
cargo test -p hamlet-daemon --test bootstrap_guarded -- \
  --ignored podman_bootstrap_forced_warm_mirror --nocapture
```

Expected: test passes, asserting mirror file count = 7, parcel B downloads = 0,
cache_hits = 7, verified_artifacts = 7, B's cargo/node versions match pins.
The seam's `rewrites() > 0` assertion confirms the argv was actually rewritten.