---
role: container-engineer
sprint: 02
step: 3
---

# 02/03 — Mounts, permissions & parcel provisioning

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §2/§4/§10, HANDOFF.md
(steps 1–2).

## Deliverable 1 — host-side permission model

Before any container starts, the daemon establishes host permissions so the
mount table is genuinely enforcing:
- parcel `home/` owned by its UID slot (chown via podman unshare or host
  userns tooling — decide + record in ADR-0007 addendum).
- `courier/mail/` spool: per-UID spool dirs 0700 owned by that UID; the
  daemon (root-equivalent in its userns) can always write.
- `courier/freight/` world-traversable (0710 + ACL-ready); ACL sandbox
  mechanics are Sprint 04 — just don't block them now.
- commission `git/` and `tavern/gossip/` readable by all slots, writable by
  none (daemon-owned) — the ro mount is belt, these perms are braces.

## Deliverable 2 — parcel provisioning flow

`provision_parcel(identity, commission) -> ParcelLease`:
1. allocate UID slot (charter, Sprint 01 API) — hard error on exhaustion;
2. create `keep/census/parcel<uid>/` with `home/` + stub `TITLE.md`
   (first chain-of-title entry: trigger + timestamp as lineage data),
   `DEED.md` (with empty `### Dynamic Alterations` block), `PROPRIETOR.md`
   (seeded from the register brief);
3. apply permissions (Deliverable 1);
4. `run_detached` with `MountSet::for_agent`;
5. record lease in charter (atomic save).

Rollback: any step failure unwinds cleanly (no orphan dirs/slots/containers);
test the rollback paths with FakeRuntime failure injection.

Verify: unit tests (FakeRuntime) + guarded podman test asserting inside the
container: `/var/git` and `/var/context` are ro (write fails), home is rw and
owned by the mapped UID, another parcel's home is unreadable. Finish:
HANDOFF.md. Commit: `feat(daemon): parcel provisioning + enforcing mount
permissions (SPEC §4)`.
