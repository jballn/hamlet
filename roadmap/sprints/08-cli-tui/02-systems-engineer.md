---
role: systems-engineer
sprint: 08
step: 2
---

# 08/02 — Full CLI implementation

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §12.1, ADR-0014, HANDOFF.md
(step 1; existing CLI verbs from sprints 01/06/07).

## Deliverable — complete `hamlet` binary

- Implement/migrate the full ADR-0014 command tree: `init`, `daemon
  {start|stop|status}` (daemonize or foreground `--fg`; pidfile + readiness
  probe; record policy), `commission {new|list|show|close}`, `agent
  {list|hibernate|wake|steer}`, `decree {list|show}`, `vault {list|export}`,
  `metrics {show|export}`, `mail {send|read}`.
- All verbs talk to the local API (no direct tree/charter reads — the CLI is
  a client like any other). `--json` on every read verb; stable exit codes;
  errors rendered from typed API errors (never raw stack traces).
- `hamlet daemon status` renders the village at a glance (active parcels,
  commissions, pending decrees, queued mail counts) — the operator's
  heartbeat view.
- Shell completion generation (clap_complete if allowlist permits — else
  hand-rolled; record decision).

Verify: CLI integration tests against a tempdir village + real daemon
process (spawn in test harness): every verb's human + JSON output goldens;
failure modes (daemon down, bad slug, unknown uid) render cleanly.
Finish: HANDOFF.md. Commit: `feat(cli): full normative CLI surface over
local API (SPEC §12.1)`.
