---
role: daemon-engineer
sprint: 05
step: 2
---

# 05/02 — Mayor session lifecycle + context reconstitution

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §7, ADR-0011, HANDOFF.md
(step 1; sprints 02–03 provisioning/session APIs).

## Deliverable — `hamlet-daemon::mayor` (lifecycle half)

- Manor provisioning: `keep/census/manor/` is parcel-shaped for UID 1000 —
  provision via Sprint 02 path with `MountSet::for_mayor()` (manor home rw,
  same courier/market mounts, ro git/context per active commission).
  MAYOR.md seeded from a committed default template (`defaults/MAYOR.md` —
  write a solid v1 trunk prompt: role, decree vocabulary usage, validation
  duty, dual-state reminders).
- **Zero-token sleep**: the Mayor session is NOT a looping chat. Model per
  SPEC §7: on wake, daemon builds the ContextPacket (ADR-0011) and runs ONE
  single-turn evaluation (fresh pi invocation or fresh-turn into a parked
  session — decide from ADR-0008's pi integration findings; record choice).
  Between events: no process tokens consumed; if the park strategy keeps a
  process alive, it must be idle-silent (assert no stream activity).
- Packet builder: charter → state-grid projection; event artifact loading
  (mail file, webhook event, diff summary); size guards (config caps,
  truncation policy recorded in HANDOFF).
- Wake serialization: one evaluation at a time; concurrent events queue
  (bounded, merged where identical — e.g. 5 new mails = one wake with 5
  artifacts).

Verify: FakeAgent-mayor tests: event → single-turn invocation with exact
packet contents (golden) → sleep; wake merging; serialization under event
storm. Finish: HANDOFF.md. Commit: `feat(daemon): mayor lifecycle +
single-turn context reconstitution (SPEC §7)`.
