# Sprint 08 — CLI Consolidation & MUD-style TUI

**Goal:** Stage 1 is complete and stage 2 begins: the full normative CLI
surface (SPEC §12.1) polished, plus a MUD-style TUI mocking the village
layout over the daemon's local API — no new state anywhere.

**Scope boundary:** No WASM/pixel graphics (Sprint 09). TUI/CLI render from
daemon APIs only — zero direct `.hamlet/` access.

**Exit criteria (SPEC §12.1, §12.2):**
- CLI complete: `init`, `daemon {start|stop|status}`, `commission
  {new|list|show|close}`, `agent {list|hibernate|wake|steer}`, `decree
  {list|show}`, `vault {list|export}`, `metrics {show|export}`, `mail
  {send|read}` — man-page-quality help, consistent output modes (human/JSON).
- Daemon local API: the TUI talks over a documented local interface
  (unix socket or localhost HTTP/WS — decide + ADR-0014); CLI migrated onto
  it where practical.
- TUI: village rooms = SPEC §2 landmarks (guild, tavern, courier, market,
  keep, manor), navigation, per-room state views (rosters, spools, parcels,
  vault, ledger board), live mail/decree/event streams, `steer` + escalation
  prompts for human intervention. Keyboard-operable throughout.
- Restart-resumability: TUI reconnects cleanly after daemon restart.
- Branch merged, tag `sprint/08-cli-tui`.

## Prompt sequence
1. `01-architect.md` — CLI surface + local API contract (ADR-0014)
2. `02-systems-engineer.md` — full CLI implementation
3. `03-frontend-engineer.md` — MUD-style TUI village
4. `04-qa-engineer.md` — interface suite + scripted demo e2e
5. `05-ci-agent.md` — gate
6. `06-commit-agent.md` — merge & tag
