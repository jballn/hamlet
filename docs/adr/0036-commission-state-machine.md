# ADR-0036 — Commission state machine, charter v4 records, INDENTURE.md grammar

Status: Accepted
Sprint: 06-commission-lifecycle · Prompt: 01/05-architect (step 01)
Refs: SPEC §2 (state contract), §9.1 (commission ledger), §9.2 (INDENTURE
decomposition), §9.3 (convergence); CONVENTIONS §2 (purity seam, singular
governor), §3 r1/r4; ADR-0004 (charter schema + version steps), ADR-0026 cl. 2
(commission ledger id = charter key), ADR-0027 cl. 2 (state grid subset)

## Context

SPEC §9 makes the commission the unit of delegated work: an operator creates
it, the Mayor decomposes it into per-agent INDENTURE.md entries, the roster
executes, and the commission converges on one deliverable plus an optional
promotion. Sprint 05 shipped only a placeholder — `CommissionStatus::Open|Closed`
in the charter's `commissions` group — which cannot express decomposition
(roster, per-entry status), convergence (deliverable under review), or failure.
The Mayor's wake vocabulary already names the endpoints (`CommissionCreated`,
`GitDiffReady`), so the machine must exist before Sprint 06's flows can run:
step 02 creates commissions, step 03 posts indentures and marks entries,
steps 04/05 converge them.

Three coupled decisions land together because they share one fact source:
(1) the typed state machine; (2) how commission facts live in `charter.json`
without breaking v3 villages (file-as-truth: one home per fact); (3) the
INDENTURE.md grammar that decomposition writes and step 03 parses.

## Decision

1. **States, events, table.** Five states — `Draft`, `Active`, `WindingDown`,
   `Closed`, `Failed` — and seven addressable events: `IndenturePosted { entries }`,
   `AllEntriesDone`, `DeliverableSubmitted`, `PromotionApproved`,
   `PromotionRejected`, `OperatorClose`, `EscalationFatal`. `Created` is
   constructor-only (the charter op that inserts the record), never an event.
   The table is total; every other `(state, event)` pair is a typed error:

   | state      | event → next                                        |
   |------------|-----------------------------------------------------|
   | Draft      | IndenturePosted→Active · OperatorClose→Closed · EscalationFatal→Failed |
   | Active     | AllEntriesDone→WindingDown · OperatorClose→Closed · EscalationFatal→Failed |
   | WindingDown| DeliverableSubmitted→WindingDown (in_review=true) · PromotionApproved→Closed · PromotionRejected→Failed · OperatorClose→Closed · EscalationFatal→Failed |
   | Failed     | OperatorClose→Closed                                |
   | Closed     | — terminal to everything                             |

   Record-level preconditions sit beside the table, checked by the charter op:
   `IndenturePosted` requires a non-empty entry set ⊆ roster; `AllEntriesDone`
   requires every entry `Done`; `PromotionApproved/Rejected` require
   `deliverable_in_review`. Roster ops (`assign_uid` / `release_uid`) are legal
   in `Draft` and `Active` only — winding down means the team is fixed.

2. **Charter v4: one home per fact.** The `commissions` group's value type is
   retyped from the 2-state placeholder to a `CommissionRecord`:
   `{ slug, state, roster, indenture_entries, deliverable_in_review }`. This is
   the charter's first value retype; it is a version step (v3 → v4) under
   ADR-0004's migration chain. The wire type is deliberately lenient: the legacy
   string carries exactly two bits and maps **bijectively** — `"open"` → Draft,
   `"closed"` → Closed — so no information is invented or lost at any version.
   Migrated records get the deterministic slug `commission-{n}`. The repo path
   is *not* stored: it derives from the ledger id via `tree::commission_git(n)`
   (typed paths; a stored copy would be a second source of truth). New
   load-time invariant: every rostered uid must have a slot record bound to
   this commission (roster implies binding; the reverse is maintained by the
   ops, not assumed at load). `create_commission(slug)` allocates the next id
   and refuses duplicate slugs; `ensure_commission(n)` keeps the legacy
   auto-registration path (census) working as an idempotent no-op-if-present.

3. **INDENTURE.md grammar.** Headings, not YAML — YAML is not in the dependency
   stack, and the file is written by a model agent: strict line grammar with
   line-numbered errors beats a parser library's opaque ones. `# INDENTURE` on
   line 1; one `## <agent>` block per roster member (SafeName charset); exactly
   three bullets per block in order — `- goal:`, `- scope:`, `- done:` — each a
   single non-empty value ≤ 512 bytes; LF-only (ADR-0019 cl. 1); file ≤ 32 KiB;
   at least one block; no other content. Canonical rendering sorts blocks by
   agent name and round-trips (`parse(render(doc)) == doc`).

4. **State grid.** The Mayor's state grid (ADR-0027 cl. 2) shows the full record
   per commission instead of the placeholder string — additive richness for the
   exact consumer who must drive this machine; goldens updated with it.

## Consequences

- `CommissionStatus` is removed from the public API (replaced by
  `CommissionState` + `CommissionRecord`); ~20 call sites in daemon tests,
  census, freight, and packet goldens move to the record constructors.
- v3 villages parse unchanged; the version stamp rides the next charter save.
- The transition table is a pure function (`commission::transition`) with its
  own unit tests; the charter op is the only governor that mutates records.
