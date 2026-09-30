---
role: daemon-engineer
sprint: 05
step: 3
---

# 05/03 — Decree executor + wake triggers

Read first: `roadmap/CONVENTIONS.md` §2–§3, `SPEC.md` §7, ADR-0011,
HANDOFF.md (steps 1–2).

## Deliverable 1 — decree watcher + executor

- Watch the decree dir (Sprint 04 reaction registry) → validate against
  ADR-0011 → replay-check (`decree_id` in charter) → execute → ack file.
- Executors (all idempotent, all charter-updating atomically, all TITLE/
  sled-stamped):
  - `ProvisionAgent`: register+blueprint refs validated → Sprint 02 parcel
    provisioning → Sprint 03 session start (brief = register + PROPRIETOR
    seeds; INDENTURE scanning arrives Sprint 06).
  - `HibernateAgent`: delegate to the Sprint 07 API — until then, implement
    the clean stop + charter status flip behind a `Stasis` trait seam and
    record the hook in HANDOFF.
  - `ReassignTask` / `SteerSession`: route through Sprint 03 queue /
    interrupt paths (source = mayor).
  - `EscalateToHuman`: write to a watched escalation inbox (CLI-visible,
    Sprint 08) + optional dispatch webhook.
  - `ApprovePromotion`/`RejectPromotion`: Sprint 06 seam — define the trait,
    stub with recorded TODO.
- Invalid decree → `rejected` ack with reason; never partial execution
  (validate fully first); executor crashes mid-decree → replay-safe resume
  (test it).

## Deliverable 2 — wake triggers

Wire reactions → mayor wakes: mail addressed to UID 1000, inbound webhook
events matching config topics, freight audit violations, decree ack
timeouts, session crash escalations (config: which crashes wake the Mayor).

Verify: full fake loop test — event → wake → packet → FakeAgent-mayor emits
decree file → executor acts → ack → sleep; replay idempotence; crash-
mid-decree resume. Finish: HANDOFF.md. Commit: `feat(daemon): decree
executor + mayor wake triggers (SPEC §7, ADR-0011)`.
