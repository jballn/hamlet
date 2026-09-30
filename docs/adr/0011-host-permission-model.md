# ADR-0011 — Host permission model & subuid userns mapping

Status: Accepted
Date: Sprint 02 (step 03)
Amends: ADR-0010 cl. 3 only (cl. 1–2, 4–6 stand)
Refs: SPEC §4; CONVENTIONS §5; step-03 prompt ("ADR-0007 addendum" =
ADR-0010 per the Sprint 02 renumbering note)

## Context

SPEC §4 shares `courier/mail/` across ALL agent containers, cross-UID
reads "blocked by spool perms". ADR-0010 cl. 3 pinned
`--userns=keep-id:uid=<uid>`, which maps the ONE host daemon user onto
each container's own slot UID: daemon-owned files appear owned by the
container user everywhere, so ownership can never distinguish slots on a
shared mount. Permission-based isolation requires the standard rootless
subuid mapping, identical in every container of the same host user.

## Decision

1. Containers run with podman's DEFAULT rootless private userns (host
   user → 0; subuid[i] → i+1) plus `--user <uid>:<uid>`. No `--userns`
   flag on `run`, ever; cl. 4 prohibitions/hardening unchanged.
2. Per-UID host ownership is set via `podman unshare chown <uid>:<gid> --
   <paths…>` (same subuid mapping containers see; no real root, no bare-
   host chown). Applied to parcel `home/` and `courier/mail/<uid>/`.
3. Host mode table (ro mounts are the belt; these perms are braces):
   `parcel<uid>/home/` 0700 uid:uid · `courier/mail/<uid>/` 0700 uid:uid
   · `courier/mail/` 0755 daemon · `courier/freight/` 0710 daemon
   (ACL-ready, Sprint 04 setfacl) · `commission<n>/git/` + `tavern/gossip/`
   0755 daemon (all slots read, none write) · `market/pkgcache/` 01777
   daemon (sticky shared cache) · `parcel<uid>/` + docs 0755/0644 daemon.
4. The daemon writes spools / maintains homes from INSIDE its userns
   (`podman unshare` or in-container tooling) — root-equivalent there,
   ordinary user on the bare host fs.

## Consequences

- Host prerequisite: /etc/subuid range ≥ 2000 ids (standard rootless
  podman requirement; exercised only on podman-capable CI hosts).
- Daemon host user cannot directly read subuid-owned 0700 homes/spools:
  Sprint 04 mail writes + Sprint 07 hibernation copies go through
  `podman unshare` / in-container tooling.
- Step-02 run-argv goldens change (keep-id removed); step-05 QA audit
  additionally asserts NO `--userns` flag on any run argv.
- Cross-UID isolation on shared mounts becomes ownership-enforceable and
  is guarded-tested from inside real containers.
