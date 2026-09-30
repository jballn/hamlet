---
role: architect
sprint: 06
step: 1
---

# 06/01 — Commission state machine + git push mediation

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §9, §7 (validation loop),
§14 (git mediation [DEV]), HANDOFF.md (sprint 05 decree seams).

## Deliverable 1 — `hamlet-core::commission`

State machine (typed transitions, every `(state, event)` pair defined):
`Draft → Active → WindingDown → Closed` (+ `Failed`). Events: created,
indenture-posted, all-entries-done, deliverable-submitted, promotion-
approved/rejected, operator-close, escalation-fatal. Charter integration:
per-commission record (slug, state, roster of UIDs, repo path, indenture
entry statuses). INDENTURE.md structure: per-agent bounded contract blocks
(agent ref, goal, scope, done-criteria) — parseable schema (markdown with
fenced metadata or headings convention — decide, record).

## Deliverable 2 — git push mediation [DEV → ADR-0012]

Resolve SPEC §14: `/var/git/` is ro inside containers, so define how work
branches land. Evaluate at least: (a) daemon-side push proxy — agents write
bundles/patches to a per-parcel spool (`courier/` or parcel-local), daemon
validates + applies to the bare repo; (b) rw bind of repo internals rejected
by spec. Choose (a)-family, specify the spool format (git bundle preferred),
validation rules (author = slot UID, size caps, no hooks execution), and
conflict policy (branch-per-agent, daemon refuses non-fast-forward on shared
refs). `GitDiffReady` wake event definition for the Mayor.

## Deliverable 3 — `hamlet-core::promote`

DEED `### Dynamic Alterations` block grammar (tool kind/name/version-spec
lines mapping onto the blueprint `[tools]` schema) + vault promotion request
types (freight path, sanitized artifact name, commission ref).

## ADR
`docs/adr/0012-git-mediation.md`.

Verify: state-machine table tests, bundle-spool golden vectors, DEED grammar
goldens. Finish: HANDOFF.md. Commit: `feat(core): commission state machine,
git mediation schema, promotion parsers (ADR-0012)`.
