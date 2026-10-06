# ADR-0033 — Promotion-read's pre-read kind check escalates through the userns

Status: Accepted
Sprint: 05-mayor-loop · Prompt: QA04-07/08/09 blocker resolution (QA04-13)
Amends: ADR-0021 cl. 6
Refs: SPEC §4 (Freight ACLs; no escape via promoted filenames); ADR-0011
cl. 2 (0700 subuid-owned sandboxes), cl. 4 (host access through
`podman unshare`); ADR-0030 cl. 2 (escalate only on `PermissionDenied`);
ADR-0031 (the other defect this battery exposed)

## Context

Running the guarded freight suite for real (ADR-0032 cl. 4 made that
possible) failed one step further along:

```
PodmanError { kind: InvalidSpec("lstat .../courier/freight/exchange/payload.md:
Permission denied (os error 13)") }
```

`FreightGuard::promotion_read` is the daemon read-access to any freight
artifact regardless of ACL: it refuses symlinked sandboxes and
symlinked/non-regular targets **before** any read (SPEC §4 traversal
invariant), then reads through `podman unshare cat`. But the refusals were
decided by the daemon's own `fs::symlink_metadata` — and a sandbox an agent
secured is **0700 subuid-owned** (ADR-0011 cl. 2), so `other` has neither
`r` nor `x` and that `lstat` returns EACCES for exactly the artifacts the
function exists to serve. The invariant check was performed with a capability
the daemon does not have in the case it was written for.

The 17 fake-runtime tests passed because the tempdir sandboxes are
daemon-owned, where the host `lstat` *does* work — the seam covered only the
easy half of the precondition, and only the guarded suite reaches the other
half.

## Decision

1. **The kind check escalates, narrowly.** The daemon's `lstat` stays the
   first attempt (cheap, and the whole job for daemon-owned sandboxes); only
   `PermissionDenied` escalates, because every other errno means what it
   always meant — the same reasoning ADR-0030 cl. 2 applies to the decree
   executor's write path.
2. **The escalated probe is a typed argv, never a shell** (ADR-0010 cl. 1):
   new `podman::stat_cmd` → `stat -c %F -- <path>` behind `--`. No `-L`, so
   the probe keeps `lstat` semantics and a symlinked target is still *seen* as
   `symbolic link` rather than silently resolved. `stat` is coreutils, already
   a named ADR-0021 cl. 2 host prerequisite alongside `cat`.
3. **Both routes refuse identically.** A pure `check_kind` classifies the host
   path and the userns path from the same table (`symbolic link` →
   `SymlinkRefused`, anything-but-directory at sandbox level →
   `NotADirectory`, anything-but-regular-file at artifact level →
   `NotAFile`), so the escalation cannot weaken the invariant it exists to
   perform.

## Consequences

- Promotion-read now works for the sandboxes it was written for, and still
  refuses symlinked sandboxes, symlinked artifacts and directories — on both
  routes, with the refusals pinned by tests (19 in `tests/freight.rs`, incl.
  the EACCES shape built without podman via a `chmod 000` sandbox).
- One extra `podman unshare stat` per promotion-read on a subuid-owned
  sandbox. Promotion is a Sprint 06 operation, not a hot path; the audit path
  is unchanged.
- `stat`'s `%F` vocabulary (`directory`, `regular file`, `symbolic link`,
  …) is now part of the contract: an unexpected string is refused as
  `NotAFile`, never treated as promotable.
- The guarded freight suite passes end to end on a real podman host for the
  first time; the defects it exposed are ADR-0031 and this one.