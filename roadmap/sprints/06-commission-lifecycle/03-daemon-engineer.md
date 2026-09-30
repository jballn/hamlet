---
role: daemon-engineer
sprint: 06
step: 3
---

# 06/03 — INDENTURE decomposition flow

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §9.2–§9.3, HANDOFF.md
(steps 1–2; sprint 05 mayor/decree APIs).

## Deliverable — indenture machinery

- Mayor wake on `CommissionCreated` delivers COMMISSION.md (done in 02);
  now handle the Mayor's *output*: decomposition arrives as decrees
  (`ProvisionAgent` with commission ref) + an INDENTURE.md write. The
  daemon validates INDENTURE.md against the Sprint 06/01 schema (per-agent
  bounded blocks; every referenced agent is rostered; blocks are well-
  formed) — invalid → rejected ack + re-wake with the parse errors as the
  event artifact (self-healing loop, bounded retries → EscalateToHuman).
- On valid INDENTURE: charter records entry statuses (pending → delivered →
  in-progress → done, per agent); each rostered agent receives its entry via
  mail (daemon-authored, sequential stdin per SPEC §8.1) with the commission
  context refs.
- Entry completion signalling: agent mails a completion notice (convention
  documented in defaults/MAYOR.md + agent brief addendum — write it) →
  daemon flips charter status → when all entries done → `AllEntriesDone`
  wake for the Mayor (validation phase begins).
- `hamlet commission show` renders indenture entry statuses.

Verify: FakeAgent-mayor + FakeAgent-workers integration test: creation →
decomposition decrees → validated INDENTURE → per-agent delivery → completion
notices → AllEntriesDone wake; invalid-INDENTURE retry loop; bounded retry
escalation. Finish: HANDOFF.md. Commit: `feat(daemon): INDENTURE
decomposition + entry tracking (SPEC §9.2–§9.3)`.
