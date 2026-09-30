---
role: qa-engineer
sprint: 05
step: 4
---

# 05/04 — Adversarial mayor suite

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §7, ADR-0011, HANDOFF.md
sprint 05 (all steps).

Committed adversarial suite (FakeAgent mayor; no podman required except
guarded manor smoke):

1. **Decree forgery/injection**: decree files appearing from a NON-mayor
   writer (agent UID owns the file) — executor must verify provenance
   (only UID 1000's session may author decrees; perms + daemon check).
   Malformed/partial decree JSON → rejected ack, no partial execution.
2. **Replay**: same decree_id delivered twice, daemon restarted between →
   exactly-once effect.
3. **Zero-token sleep**: assert no pi stream activity between events over a
   monitored idle window; wake fires only on registered triggers.
4. **Packet hygiene**: charter projection contains no secrets (.env keys);
   packet is single-turn (no thread accumulation across 10 sequential wakes
   — invocation count = wake count).
5. **Wake storms**: 500 events → merged/serialized wakes, bounded queue, no
   interleaved evaluations, FIFO by decree_id ordering.
6. **Executor crash resilience**: kill daemon mid-ProvisionAgent → restart →
   no duplicate parcels/slots, decree either fully done or cleanly rejected.
7. **Escalation path**: EscalateToHuman observable from tree + CLI hook;
   ack timeouts re-wake the Mayor exactly once per timeout.

Findings → HANDOFF.md. No product fixes. Verdict: `GATE 05/04: PASS|FAIL`.
