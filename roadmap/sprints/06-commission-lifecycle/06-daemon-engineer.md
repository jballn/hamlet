---
role: daemon-engineer
sprint: 06
step: 6
---

# 06/06 — Vault promotion + spin-down + archives

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §7 (validation loop), §9.4,
§11.2, HANDOFF.md (steps 1–5; sprint 05 ApprovePromotion seam).

## Deliverable — deliverable promotion loop

- Flow: agent writes finished product to `/var/freight/` → mails submission
  (convention) → daemon fires `DeliverableSubmitted` mayor wake (artifact:
  freight listing + refs) → Mayor single-turn validation against
  COMMISSION/INDENTURE goals → `ApprovePromotion` / `RejectPromotion`
  decree → executor (implement the Sprint 05 seam now):
  - **Approve**: re-verify freight path exists + ACL-readable daemon-side →
    sanitize artifact name (SafeName; reject on failure with reason ack) →
    atomic copy into `keep/vault/<commission-slug>/<artifact>` (checksum
    recorded) → charter commission state advance → trigger session spin-down
    for the submitting parcel (graceful stop API; hibernation itself is
    Sprint 07 — leave the Stasis hook) → telemetry export to
    `keep/archives/<commission>/` (Sprint 01 export API).
  - **Reject**: reason mailed to agent + Mayor re-wake hook for re-work
    loop (bounded: N rejections → EscalateToHuman, N in config).
- `hamlet vault list|export` CLI (functional now; polish Sprint 08).
- Commission close: operator `hamlet commission close <slug>` → WindingDown
  (no new provisions) → when all parcels stopped → Closed + final archives
  export.

Verify: integration test full loop with FakeAgents (submit → approve →
vault artifact byte-identical + checksum; reject → rework → approve);
sanitizer rejection path; restart mid-copy → no partial vault artifact.
Finish: HANDOFF.md. Commit: `feat(daemon,cli): vault promotion loop +
commission close (SPEC §7, §9)`.
