---
role: daemon-engineer
sprint: 06
step: 5
---

# 06/05 — DEED → blueprint promotion

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §5 (blueprint evolution),
§9.4, §10 (dual-state), HANDOFF.md (steps 1–4; sprint 01 promote parser
types from 06/01).

## Deliverable — environment-layer promotion

- Watch DEED.md mutations per parcel (ADR-0009 reaction): maintain parsed
  view of `### Dynamic Alterations` (incremental, checksummed — agents
  append; the daemon never rewrites agent history inside an active parcel).
- At entry-completion (or commission wind-down trigger — support both,
  record default): daemon validates alteration lines against the blueprint
  `[tools]` grammar (06/01) — unknown kinds, impossible versions, or
  injection attempts (newlines, shell metachars, path separators in names)
  → rejected with a report mailed to the agent + sled trace; valid set →
  merged into the parent blueprint in `guild/blueprints/` with version bump
  and provenance comment (commission, uid, timestamp-as-lineage-data).
- Blueprints remain immutable-at-runtime: promotion writes a NEW version
  (append-only versioning scheme — decide file layout, record in HANDOFF);
  existing parcels keep their bootstrap manifest.
- Idempotent: re-promoting an already-merged alteration set is a no-op
  (content-hash dedupe).

Verify: golden DEED blocks → blueprint version diffs; rejection matrix
(injection, unknown tool, dup); idempotence test; restart mid-promotion →
no half-written blueprint (atomic write). Finish: HANDOFF.md. Commit:
`feat(daemon): DEED dynamic-alterations → blueprint evolution (SPEC §9.4)`.
