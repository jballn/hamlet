---
role: architect
sprint: 05
step: 1
---

# 05/01 — Decree vocabulary + file encodings

Read first: `roadmap/CONVENTIONS.md` §3, `SPEC.md` §7, HANDOFF.md (sprint
04 reaction registry).

## Deliverable 1 — `hamlet-core::decree` [DEV → ADR-0011]

Normative decree set (SPEC §7): `ProvisionAgent {register_ref, blueprint_ref,
commission}`, `HibernateAgent {uid}`, `ReassignTask {uid, indenture_ref}`,
`SteerSession {uid, directive}`, `EscalateToHuman {summary, evidence_refs}`,
`ApprovePromotion {freight_path, artifact_name}` / `RejectPromotion
{freight_path, reason}`. Encoding: one JSON file per decree in a watched
decree directory (choose location under `keep/census/manor/` — record it);
atomic temp+rename by the Mayor; each decree carries `decree_id` (content-
derived, monotonic counter in charter for ordering) enabling **idempotent
replay protection** (executed ids recorded in charter).

Ack protocol: daemon writes `<decree_id>.ack` (result: done|rejected+reason)
next to the decree; ack timeout (config) → Mayor wake event.

## Deliverable 2 — context packet schema

Typed `ContextPacket` in core: MAYOR.md text, charter state grid rendering
(human-readable projection — decide exact serialization, e.g. pretty JSON
subset), event artifact (typed: MailReceived | WebhookEvent | FreightViolation
| DecreeAckTimeout | CommissionCreated | GitDiffReady), wake reason. Single-
turn by construction: the packet IS the whole prompt; nothing accumulates.

## Deliverable 3 — ADR

`docs/adr/0011-decree-protocol.md`: vocabulary, encodings, ack/replay rules,
decree dir layout, ordering semantics (decree_id monotonicity vs. file
timestamps — state contract forbids clock-derived ordering).

Verify: golden decree vectors (parse/serialize/replay-idempotence), packet
rendering goldens. Finish: HANDOFF.md. Commit: `feat(core): decree
vocabulary + context packet schema (ADR-0011)`.
