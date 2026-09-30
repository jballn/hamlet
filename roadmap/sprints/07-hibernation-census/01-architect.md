---
role: architect
sprint: 07
step: 1
---

# 07/01 — Stasis formats + resource-bounds policy

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §10, §14 (resource bounds
[DEV]), HANDOFF.md (sprints 05–06: Stasis trait seam, spin-down hook).

## Deliverable 1 — `hamlet-core::stasis`

- Patron record schema: `PROPRIETOR<name>/` layout (SafeName), serialized
  PROPRIETOR.md metadata block (machine-parseable frontmatter + freeform
  intelligence profile body — grammar yours to lock), `home.uid.bak/`
  manifest (checksums per top-level entry for integrity on awaken).
- TITLE.md grammar: append-only entry types (trigger, provision, session
  incarnation, crash, steer, promotion, hibernate, awaken), each
  daemon-stamped (timestamp as lineage data — never a state input, per
  CONVENTIONS §3.4).
- Convergence types: the two independent promotions (DEED-set → blueprint
  delta [reuse 06]; PROPRIETOR delta → patron record) with separate success
  tracking — one may fail without blocking the other.

## Deliverable 2 — resource policy [DEV → ADR-0013]

Resolve SPEC §14: podman limit flags per parcel (cpu shares, memory, pids —
config.json schema), pressure detection inputs (daemon-sampled container
stats via podman, sled-backed rolling window — telemetry, not state),
candidate selection ranking, and the hard rule: daemon **suggests**
(HibernateCandidate wake event with evidence); only a Mayor decree (or
operator CLI) hibernates. OOM/crash policy interplay with Sprint 03 restart
limits.

## ADR
`docs/adr/0013-stasis-and-resources.md`.

Verify: golden vectors (patron record, TITLE entries, manifest), schema
tests. Finish: HANDOFF.md. Commit: `feat(core): stasis schemas + resource
policy types (ADR-0013)`.
