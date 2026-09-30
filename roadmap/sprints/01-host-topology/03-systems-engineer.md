---
role: systems-engineer
sprint: 01
step: 3
---

# 01/03 — Charter state model + UID slot allocation

Read first: `roadmap/CONVENTIONS.md` §3 (state contract — ALL six rules),
`SPEC.md` §11.1 and §4 (UID policy), HANDOFF.md (steps 1–2).

## Deliverable 1 — `hamlet-core::charter`

In-memory `Charter` state built exclusively from `BTreeMap`/`BTreeSet`:
- `charter_version: u32` (start at 1) + additive-only migration fn skeleton.
- Normative groups (SPEC §11.1): `slots` (uid → identity record), `agents`
  (registry refs), `commissions` (id → state enum placeholder), `parcels`
  (uid lease records), `decrees` (queue status placeholder), `webhooks`
  (config refs placeholder). Later sprints extend; you define the shape now.
- `load(path)` / `save_atomic(path)`: temp file in same dir → write → fsync →
  rename. Sorted-key canonical JSON (serde_json over BTreeMap). Never in
  place. Corruption on load = hard error with the offending byte offset, not
  silent recovery.

## Deliverable 2 — `hamlet-core::slots`

UID slot allocator: 1000 statically reserved for the Mayor/manor identity;
1001–2000 allocated sequentially and permanently per agent identity.
Allocation records: identity name (SafeName), status (active/hibernated/
crashed), commission ref (optional). Hibernated slots keep their mapping
(SPEC §10). Exhaustion beyond 2000 → typed hard error. No reuse while any
record (incl. patron stasis) references the slot.

## Deliverable 3 — ADR + resumability test

- `docs/adr/0004-charter-schema.md`: v1 schema, versioning/migration policy.
- Kill/restart test: build a charter, save, `kill -9`-equivalent (drop
  without flush), reload from disk, assert byte-identical re-serialization
  and equal state. Also: torn-write test (truncate the temp mid-write) proves
  the previous charter survives.

Verify: `cargo test -p hamlet-core` green; ci-local green. Finish: HANDOFF.md
(record schema decisions → ADR-0004). Commit:
`feat(core): atomic charter state model + permanent UID slots (ADR-0004)`.
