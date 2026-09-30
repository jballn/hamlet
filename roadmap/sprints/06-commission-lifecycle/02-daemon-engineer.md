---
role: daemon-engineer
sprint: 06
step: 2
---

# 06/02 — Commission creation, bare repos, CLI basics

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §9.1, ADR-0012, HANDOFF.md
(step 1).

## Deliverable — `hamlet-daemon::ledger` + CLI

- `hamlet commission new <slug> --brief <file|->`: sanitize slug → create
  `guild/ledger/commission<n>/` (n sequential from charter), `git init
  --bare git/`, write COMMISSION.md from the brief, register in charter
  (Draft → Active on creation), fire `CommissionCreated` mayor wake with the
  COMMISSION.md artifact.
- `hamlet commission list|show <slug>`: from charter + tree only.
- Roster management API (daemon-side): assign/release UIDs to a commission
  (feeds ACL audit policy from Sprint 04 and decree validation).
- Repo seeding: optional `--seed <dir>` importing existing files as the
  initial commit (daemon-authored, recorded in TITLE-style lineage).
- Multi-tenancy test: two commissions, disjoint rosters, independent states,
  independent git dirs; mount sets differ per commission (Sprint 02 API).

Verify: tempdir integration tests (no podman): full creation flow, wake
fired (reaction spy), slug sanitization rejects, list/show output goldens.
Finish: HANDOFF.md. Commit: `feat(daemon,cli): commission creation + bare
git repos (SPEC §9.1)`.
