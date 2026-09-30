---
role: architect
sprint: 08
step: 1
---

# 08/01 — CLI surface + daemon local API contract

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §12.1–§12.2, HANDOFF.md
(sprints 01–07: existing functional CLI bits to consolidate).

## Deliverable 1 — local API contract [DEV → ADR-0014]

Decide the daemon-local interface both CLI and TUI (and later the WASM
frontend's server-side) consume: recommend a localhost HTTP+WS API on the
existing axum server (Sprint 04) with a unix-socket option — pick one,
justify. Contract: read-only state projections (census, ledger, decrees,
vault, metrics, mail listings) + command endpoints (commission new/close,
agent hibernate/wake/steer, mail send, daemon control) + event stream (WS
topic feed reusing the reaction vocabulary). Authn: local-only bind +
token file (0600) — record scheme. Every command endpoint must map onto
EXISTING daemon APIs — the API layer adds no orchestration logic.

## Deliverable 2 — CLI command tree spec

Full SPEC §12.1 surface as a clap command tree table: flags, args, output
modes (`--json` everywhere for scripting), exit-code policy, error rendering.
Mark which verbs already exist (migrate) vs. new.

## Deliverable 3 — TUI information architecture

Room graph for the MUD mock: landmarks per SPEC §2, what each room renders
(state projections used), navigation model, live-stream placement, human-
intervention surfaces (steer dialog, escalation inbox, decree review).
ratatui + crossterm (already allowlisted). No state, no tree access.

## ADR
`docs/adr/0014-local-api.md`.

Verify: contract types compile in core/daemon; table-driven route tests.
Finish: HANDOFF.md. Commit: `feat(core,daemon): local API contract + CLI/
TUI specs (ADR-0014)`.
