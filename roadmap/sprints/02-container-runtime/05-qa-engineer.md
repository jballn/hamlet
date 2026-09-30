---
role: qa-engineer
sprint: 02
step: 5
---

# 02/05 — Isolation & escape suite

Read first: `roadmap/CONVENTIONS.md` §5, `SPEC.md` §4 isolation invariants,
HANDOFF.md sprint 02 (all steps).

Write the committed adversarial suite (guarded podman tests + FakeRuntime
unit tests where applicable):

1. **ro mounts**: from inside an agent container, writes to `/var/git/` and
   `/var/context/` fail; symlink-into-ro tricks fail; the host dirs are also
   unwritable by the mapped UID directly (perms braces).
2. **Cross-UID**: agent A cannot read agent B's `/home/`, mail spool, or
   freight ACL dirs (freight ACLs land in Sprint 04 — test what exists:
   spool 0700 perms).
3. **No infrastructure**: no podman socket, no docker socket, no host dbus
   visible inside; privileged mode never appears in any recorded argv
   (FakeRuntime argv audit test over ALL spawn paths).
4. **Userns mapping**: inside-container UID matches the charter slot; host
   uid mapping doesn't collide with the daemon's own user beyond policy.
5. **Provisioning rollback**: inject failures at each provision step →
   no orphan slots/dirs/containers; charter unchanged after failed provision.
6. **Slot exhaustion**: provision 1000 agents (FakeRuntime) → 1001st hard
   errors, no wrap.
7. **Bootstrap integrity**: no `.env` secret material inside any provisioned
   home or image layer.

File findings into HANDOFF.md (file:line, repro, expected vs actual, SPEC
citation). Do NOT fix product code. Verdict line: `GATE 02/05: PASS|FAIL`.
