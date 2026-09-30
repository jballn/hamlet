---
role: frontend-engineer
sprint: 09
step: 4
---

# 09/04 — Management pixel UI + command channel

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §12.3, ADR-0015, HANDOFF.md
(steps 1–3).

## Deliverable — interactable work management

Pixel-UI panels (in-canvas or DOM-hybrid — consistent with step 03 arch;
record choice), each bound to landmark/sprite interactions:

- **Agent panel** (click sprite): live telemetry, TITLE lineage scroll,
  DEED/PROPRIETOR summaries, actions: steer (compose → command frame),
  hibernate, wake.
- **Guild board** (guild hall): commissions list/new/close, INDENTURE entry
  statuses per agent, roster management.
- **Vault** (keep): artifact list + checksums, export trigger.
- **Decree desk** (manor): decree queue, ack states, replay button for
  rejected, escalation inbox with resolve flow.
- **Courier** (post office): mail compose/read (owner-scoped), dispatch
  outbox/inbox view.
- **Tavern** (stasis): patron list, awaken-into-commission flow.

Every mutation = command frame → ack → optimistic-UI rollback on reject.
No panel invents state: everything renders projections from step 02/03
frames. Keyboard operable (focus traversal, enter/space activation);
screen-reader fallback = the DOM status region mirroring panel content.

Verify: view-model goldens per panel (fixture frames → panel scene);
command round-trip tests against the daemon test harness (every panel
action exercised, including rejects/rollbacks); a11y keymap table test.
Finish: HANDOFF.md. Commit: `feat(web): pixel management UI over command
channel (SPEC §12.3)`.
