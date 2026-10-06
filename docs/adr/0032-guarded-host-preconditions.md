# ADR-0032 — Guarded-suite host preconditions: skip with a remedy, never a vacuous green

Status: Accepted
Sprint: 05-mayor-loop · Prompt: QA04-07/08/09 blocker resolution
Amends: CONVENTIONS §5 (guarded battery); ADR-0013 cl. 3 (mirror is a cache)
Refs: SPEC §4 (mount table, UID policy); ADR-0011 cl. 2 (subuid userns),
ADR-0012 cl. 2 (derived images); CI04-01

## Context

Sprint 04 closed with one red: the guarded podman battery, blocked by three
filed host findings and no product defect —

- **QA04-07** the suites need the SPEC §4 per-slot uid policy to be
  *observable* on the host: `podman unshare chown` must be able to hand a path
  to a subuid, or parcel homes stay daemon-owned and every ownership assertion
  in the battery is vacuously true.
- **QA04-08** the cold bootstrap extracts ~1 GB of toolchains inside a
  `--memory`-capped parcel. On a RAM-backed `TMPDIR` the pages are charged to
  the parcel's cgroup, so the extract is OOM-killed (`exit 137`) or truncated
  (`tar: Unexpected EOF in archive`) — reproduced live here, and *not* a free-
  space question: it fails identically with 7.5 GB free on the tmpfs.
- **QA04-09** SPEC §4 mounts the parcel home and the pkgcache mirror as two
  separate bind mounts. When they land on different host devices the ADR-0013
  cl. 3 hardlink publish is EXDEV — explicitly non-fatal, "it is a cache" —
  so the mirror legitimately stays empty and `bootstrap_guarded`'s "the second
  parcel downloads nothing" assertion cannot hold there. Sprint 04 left the
  policy to a later step rather than deciding it.

A fourth, larger problem surfaced while fixing them: the harnesses could
report success for suites that asserted **nothing**. `e2e-04` T3 ran
`cargo test --test freight_guarded` without `-- --ignored` on a file whose
only test is `#[ignore]`d: zero tests executed, exit 0, `pass` printed. The
sprint-04 gate recorded that as "T3 guarded freight GREEN on real podman".
The same vacuity made a second, unrelated real defect (ADR-0031) invisible.

## Decision

1. **A host that cannot satisfy a suite's precondition SKIPS with a remedy.**
   `crates/hamlet-daemon/tests/support/mod.rs` holds the shared, pure-shape
   checks: `userns_precondition` (QA04-07: `podman unshare chown` must reach
   a subuid), `work_area_precondition` (QA04-08: the tree's filesystem is not
   RAM-backed and has ≥ 4 GiB free). Each skip line names the finding and the
   host remedy. **No assertion is weakened**: on a compliant host every
   assertion runs verbatim.

   **Correction, recorded because the first cut of this ADR was wrong.** The
   check as first written was "the daemon user must not be the podman owner",
   and `escape_guarded`'s disjointness assertion demanded that the daemon's
   host uid appear nowhere in `uid_map`. **No host can satisfy that**: rootless
   podman maps `0 → <invoking uid> 1` in every container it starts, so the
   invoking user is always present in its own containers' mapping. The
   invariant ADR-0011 cl. 2 actually forbids is a *slot* mapping back onto the
   daemon — so the assertion is now exempt for exactly that one-uid owner row
   (asserting instead that it maps container root onto the invoking user,
   which is rootless podman's documented identity model, ADR-0010), and the
   precondition is the subuid probe. A host requirement phrased as an
   impossibility is worse than no requirement: it manufactures a permanent
   skip and hides the part that is genuinely satisfiable.
2. **A precondition is applied only where the invariant needs it.** The
   freight ACL suite, the derived-image suites and the secret-scan suite stay
   ungated on QA04-07/08 axes they do not depend on — a blanket gate would
   trade a host-shaped RED for a silent coverage hole, which is the same lie
   wearing the opposite hat.
3. **QA04-09 branches on the measured device pair, never an assumption.**
   `same_device(parcel_home, market_pkgcache)` decides, once, before any
   assertion: same device → the mirror must warm, `publish_failures` must be
   empty, the second parcel must download nothing (the existing claims).
   Different device → every publish failure must be **EXDEV and only EXDEV**
   (so a digest or permission failure cannot hide behind the host excuse), the
   mirror must be empty, and the second parcel must legitimately re-download.
   The zero-download claim is stated only in the shape that supports it.
4. **The remedy is a script, not tribal knowledge.**
   `scripts/guarded-service-user.sh` runs the battery as the dedicated
   service user: `--prepare` grants it execute-only ACLs on the build tree
   (never world-readable, never writable), and the run itself needs no cargo —
   it executes the already-built binaries with `TMPDIR` pointed at a
   disk-backed work area. No product change.

5. **A guarded suite that skipped everything is not a pass.** `ci.sh` step 5
   and the `e2e-02`/`e2e-04` harnesses scan the test output for the recorded
   `SKIP` lines and print them (and report the step as skipped, not green).
   `e2e-04` T3 now passes `-- --ignored podman --nocapture`; it previously
   ran zero tests.

## Consequences

- On this host the guarded battery no longer reports a RED that reads like a
  product defect: QA04-07 suites skip with the remedy, and the suites that
  *can* run here (derived-image shape, freight ACLs with a disk-backed
  `TMPDIR`) run for real — which is how ADR-0031 was found.
- QA04-08's remedy is now stated by the tool that needs it: `TMPDIR` must be
  disk-backed, not merely large.
- Coverage accounting is honest in both directions: a skip is visible in the
  gate log rather than silently shrinking what the gate covers.
- The ADR-0013 EXDEV tolerance is unchanged; only the test's assertion shape
  now matches it on both host shapes.