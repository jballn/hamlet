# ADR-0008 — Slot identity uniqueness is a load-time invariant

Status: Accepted
Date: Sprint 01 (QA fix pass)
Amends: ADR-0004 clause 4 (invariant list; additive tightening)
Refs: SPEC §4; QA finding F01-04

## Context

SPEC §4 allocates UID slots "sequentially and permanently per agent
identity" — one identity, one slot, forever. QA F01-04: `Charter::validate`
checked parcel↔slot / agent↔slot identity matches, commission refs, and uid
ranges, but NOT slot-table identity uniqueness — a hand-edited charter
binding one identity to two slots loaded `Ok`. Downstream, `allocate`'s
find-by-identity became ambiguous (returns the lowest uid), hibernation /
reactivation of "the" slot was undefined, and `next_free_uid` bookkeeping
could drift — silent divergence instead of the fail-hard load ADR-0004
requires.

## Decision

`Charter::validate` rejects any identity bound to more than one slot with
`CharterError::Invariant`, naming the identity and the offending uid. The
check runs inside the existing slot loop in uid-sorted order, preserving
"first violation (sorted order)" error reporting. Like every other
invariant it applies on every `load`/`from_bytes` and on explicit
`validate()` calls. No schema change: `charter_version` stays 1 — this is a
validation tightening, not a migration.

## Consequences

- Duplicate-identity charters fail hard at load; no downstream code can
  observe the ambiguity (pinned by
  `duplicate_identity_across_slots_must_violate_invariant`, both on-disk
  and in-memory forms).
- `slots::allocate` idempotency (find-by-identity) is unambiguous for any
  charter that loads at all.
- Charters produced by hamlet itself could never contain duplicates
  (allocation is idempotent per identity and no removal API exists), so
  the invariant only ever fires on corruption or hand edits — exactly the
  population ADR-0004's fail-hard load policy targets.
