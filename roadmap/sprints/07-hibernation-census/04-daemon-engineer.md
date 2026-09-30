---
role: daemon-engineer
sprint: 07
step: 4
---

# 07/04 — Quick awakening + dual-state convergence

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §10 (quick awakening,
dual-state convergence), ADR-0013, HANDOFF.md (steps 1–3).

## Deliverable 1 — quick awakening

`awaken(patron_ref, commission)`:
- match patron by skill configuration (register ref + blueprint ref +
  PROPRIETOR frontmatter tags — matching rule recorded in HANDOFF);
- allocate next available UID slot (fresh slot; patron's old slot mapping
  persists as historical); provision parcel WITHOUT clean bootstrap —
  drop `home.uid.bak/` into `parcel<uid>/home/` (manifest-verified),
  seed PROPRIETOR.md from the patron record, fresh TITLE.md opening entry
  (lineage: awakened-from patron + prior commission);
- session start (Sprint 03) with the patron's intelligence profile as the
  brief overlay. Assert the fast path: no blueprint tool downloads
  (instrumented pkgcache counters stay zero).
- CLI `hamlet agent wake <patron> [--commission <slug>]`.

## Deliverable 2 — dual-state convergence at commission end

On commission close (Sprint 06 WindingDown → Closed): for each rostered
parcel run BOTH promotions independently (06/05 DEED→blueprint; 07/03
PROPRIETOR→patron), track success separately, hibernate remaining sessions,
export final archives. One promotion failing must not block the other;
failures surface as EscalateToHuman-eligible events.

Verify: hibernate → awaken round-trip preserves home content (checksum) +
profile; convergence matrix tests (both ok / deed fails / proprietor fails);
restart mid-awaken → resumable. Finish: HANDOFF.md. Commit:
`feat(daemon,cli): quick awakening + dual-state convergence (SPEC §10)`.
