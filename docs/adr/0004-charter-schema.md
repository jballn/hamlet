# ADR-0004 — Charter schema v1, canonical JSON & atomic persistence

Status: Accepted (clause 3 temp naming superseded by ADR-0006; clause 4
invariant list extended by ADR-0008)
Date: Sprint 01 (step 01/03)
Refs: SPEC §11.1, §4, §10; CONVENTIONS §3; ADR-0002, ADR-0003

## Context

SPEC §11.1 leaves the `charter.json` schema `[DEV]`. Sprint 01 needs the
normative groups, the CONVENTIONS §3 persistence contract, and the SPEC §4
UID policy as testable code; sprints 03/05/06/07 extend the same file, so
versioning/migration policy is fixed now.

## Decision

1. **v1 groups** (all `BTreeMap`/`BTreeSet`, `deny_unknown_fields`):
   `slots: uid→{identity: SafeName, status: active|hibernated|crashed,
   commission?}`, `agents: SafeName→{slot}` (placeholder, sprint 03),
   `commissions: u64→open|closed` (placeholder, sprint 06),
   `parcels: uid→{identity, stasis}` (SPEC §10),
   `decrees: u64→queued|delivered|failed` (placeholder, sprint 05),
   `webhooks: BTreeSet<SafeName>` config-ref names.
2. **Canonical JSON**: render via `serde_json::to_value` (objects are
   BTreeMaps → keys sorted at every level) + pretty + trailing newline.
   Empty groups are skipped: a fresh village is `{"charter_version": 1}`.
3. **Atomic persistence**: `save_atomic` = temp `.<name>.tmp` same dir →
   write → fsync → chmod 0644 → rename → dir fsync; never in place. This
   makes `charter::{load, save_atomic}` the second sanctioned I/O point in
   hamlet-core (extends ADR-0003's `ensure()` precedent — the write protocol
   is itself the unit under test, so it cannot be mocked).
4. **Load is fail-hard**: JSON corruption → error with line/column/**byte
   offset**; `charter_version` 0 or > current → `UnsupportedVersion`;
   cross-group invariants (parcel↔slot and agent↔slot identity, commission
   refs, uid ranges) validated on every load. No silent recovery.
5. **Migrations additive-only**, explicit per-version steps in `migrate()`:
   add fields/groups with defaults — never rename, retype, or drop. Readers
   reject newer versions instead of guessing.
6. **UID policy** (SPEC §4): 1000 statically reserved for the Mayor;
   1001–2000 allocated sequentially; permanence is structural — `slots`
   exposes no removal API, allocation is idempotent per identity,
   hibernated/crashed slots keep their mapping, exhaustion is
   `SlotError::Exhausted` (never a wrap).

## Consequences

- Later sprints extend groups additively under a version bump + migration
  step; anything else requires a superseding ADR.
- Kill/restart resumability is a core-level test contract (torn temp files
  never affect the committed charter); daemon tests build on it.
- Placeholder enums (commissions/decrees) gain variants via additive
  migration when sprints 05/06 land full state machines.
