# ADR-0021 — Freight ACL sandboxes: audit, quarantine, promotion-read

Status: Accepted
Date: Sprint 04 (step 03)
Amends: ADR-0011 cl. 3 (freight-root mode row only)
Amended by: ADR-0031 (cl. 1: the listing keeps its `# owner:` header — the
original `-c` flag suppressed the very state the audit's first rule reads),
ADR-0033 (cl. 6: promotion-read's pre-read kind check escalates through the
userns, because a 0700 subuid-owned sandbox denies the daemon's own lstat)
Refs: SPEC §4 (Freight ACLs), §8.3; ADR-0019 cl. 5 (`Reaction::FreightDir`)

## Context

SPEC §4: agents secure `/var/freight/<dir>` artifacts with `setfacl` for a
recipient UID; the daemon provides the tooling but NEVER relaxes ACLs on an
agent's behalf. ADR-0011 left `courier/freight/` 0710 daemon-owned
("ACL-ready"): no slot could even enter. The audit must read ACLs of
subuid-owned 0700 dirs — impossible for the daemon's host user directly.

## Decision

1. Audit engine is pure (`hamlet-core::freight`): strict `getfacl -p -n`
   parser + rule table over committed golden fixtures. Rules: owner is a
   charter slot; no `other` write (access or default); every named grantee
   UID is on the roster; no named *group* entries (groups aren't charter
   state).
2. Daemon-side ACL ops run through `podman unshare <host tool>` (ADR-0011
   cl. 4: userns root sees every sandbox; numeric IDs are in-ns slot UIDs).
   New `Runtime::unshare_exec` + typed argv builders (`getfacl_cmd`,
   `setfacl_cmd` accepting only `u:<uid>:[r-][w-][x-]` tokens, `cat_cmd`).
   Host prerequisite: `acl` + coreutils (guarded-test preflight skips with
   recorded reason otherwise).
3. **Commission-scoped sharing** (the step's policy question): grantee
   roster = Mayor (1000) + slots bound to an **Open** commission; owner set
   = every charter slot. Closed-commission and idle slots are not
   shareable. Sprint 06 per-commission scoping narrows the roster only —
   the audit consumes an injected `AuditPolicy`.
4. Freight root: mode **0o1770** (amends ADR-0011's 0710 row) — group bits
   are the POSIX ACL *mask slot* once slots hold `u:<uid>:rwx` root grants;
   rwx there keeps a later chmod from clipping the mask and disarming
   grants. Sticky bit: slots share root rwx, so only sticky stops
   cross-unlink/rename of sandboxes. Grants come from the freight plane
   (`ensure_root_acl`, idempotent, every charter slot) and `grant_slot` at
   provisioning (Sprint 07 advisory, mirroring `ensure_spool_dirs`). These
   are the ONLY host-side ACL writes; sandbox ACLs are never touched.
5. Violations → sled trace + quarantine flag
   `courier/freight/.quarantine/<name>.json` (hidden from the watcher,
   daemon-owned dir 0755, flag 0444: agent-readable evidence, never
   agent-writable). Flags persist until Mayor/operator resolution (Sprint
   05+); re-audit overwrites with fresh evidence. Escalation: ≥ 3 flags per
   owner (counted from the flag files — file-as-truth, restart-safe) fires
   the `MayorWake` hook once per owner per process lifetime.
6. Promotion-read (Sprint 06 helper): `podman unshare cat` — host-side
   userns capability, zero ACL relaxation. Symlinked sandboxes and
   symlinked/non-regular targets are refused by lstat BEFORE any read
   (SPEC §4 traversal invariant).

## Consequences

- Non-directory freight entries (files/symlinks) are flagged
  `not_a_directory` (owner 0) — escape shapes become evidence, not silent
  skips; sanitizer-rejecting names are unauditable (skipped with a log —
  promotion refuses them too).
- Guarded suites asserting freight 0710 update to 0o1770; in-container
  "freight unreachable" assertions hold wherever the freight plane never
  ran (other::--- and no root grants).
- The escalation threshold (3) is a const, not config: tuning it is a
  superseding ADR, not a knob.
