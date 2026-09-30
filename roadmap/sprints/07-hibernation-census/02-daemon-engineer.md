---
role: daemon-engineer
sprint: 07
step: 2
---

# 07/02 — TITLE chain-of-title + census records

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §10, ADR-0013, HANDOFF.md
(step 1).

## Deliverable — census plane

- `hamlet-daemon::census`: TITLE.md writer — every lifecycle event across
  sprints 02–06 (provision, session start/crash/restart, steer, interrupt,
  bundle apply, submission, promotion, spin-down) appends a stamped entry.
  Retrofit the call sites (search daemon for the event points; keep entries
  terse). Append-only: the daemon never rewrites or truncates TITLE.md;
  agent-side edits are detected (checksum-on-read) → trace + Mayor wake
  (tamper evidence, not fatal).
- Census view: `keep/census/` listing API (parcels, states, commissions,
  uptime-ish counters from sled) consumed by CLI `hamlet agent list`
  (functional now) and later TUI/web.
- Manor exempt from hibernation; its TITLE lives at
  `keep/census/manor/TITLE.md` (create it).

Verify: integration test — scripted lifecycle leaves the exact golden TITLE
sequence; tamper detection fires; census listing matches charter.
Finish: HANDOFF.md. Commit: `feat(daemon): TITLE chain-of-title + census
view (SPEC §10)`.
