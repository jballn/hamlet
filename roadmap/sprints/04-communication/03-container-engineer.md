---
role: container-engineer
sprint: 04
step: 3
---

# 04/03 — Freight POSIX ACL sandboxes

Read first: `roadmap/CONVENTIONS.md` §5, `SPEC.md` §4 (freight ACLs) and
§8.3, HANDOFF.md (sprint 02 perms model).

## Deliverable — freight ACL mechanics

- In-container tooling path: agents create `/var/freight/<dir>` and secure it
  with `setfacl` for a recipient UID (ensure acl utils in the default
  blueprint — update `defaults/village-alpine.toml` via the blueprint schema,
  bumping its version).
- Daemon-side **ACL audit** (`hamlet-daemon::freight`): on freight dir
  creation events (ADR-0009 reaction), verify ACL sanity: owner is a valid
  slot UID, no `other::rw` on secured dirs, no ACL granting a UID outside
  the commission's roster (policy: commission-scoped sharing — record
  decision in HANDOFF). Violations → sled trace + quarantine flag file
  (daemon-written, ro to agents); repeated violations escalate to a Mayor
  wake event (hook only — Sprint 05 consumes).
- Helper for the Sprint 06 promotion path: daemon read-access to any freight
  dir regardless of ACL (host-side capability), without relaxing agent-side
  ACLs.
- Guarded podman test: agent A secures a dir for agent B; B reads/writes;
  C is denied; audit flags a deliberately world-writable dir; daemon
  promotion-read succeeds on all.

Verify: guarded tests + FakeRuntime unit tests for audit logic (ACL listings
as fixtures). Finish: HANDOFF.md. Commit: `feat(daemon): freight ACL
sandboxes + audit (SPEC §4, §8.3)`.
