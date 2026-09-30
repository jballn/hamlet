# Sprint 09 — WASM 8-bit RPG Frontend

**Goal:** Stage 3: axum serves the pixel-RPG asset layers and streams
real-time canvas metrics + chat bubbles over WebSockets; the browser renders
an interactable 8-bit village mapped 1:1 to the SPEC §2 landmarks, with a
pixel UI for managing work — all state from daemon pushes, never direct.

**Scope boundary:** Functional pixel village, not a finished art game
(SPEC §13 out-of-scope note). No new orchestration logic anywhere.

**Exit criteria (SPEC §12.3):**
- WS protocol resolved (ADR-0015): canvas metrics stream (per-parcel token/
  cost/latency, from sled projections), chat-bubble feed (mail/steer
  snippets, escalation notices), state-delta frames (census/ledger/decree
  projections), command channel (token-authed, same endpoints as CLI/TUI).
- axum serves `hamlet-web` assets (wasm + JS glue + sprite layers), with
  asset caching headers; wasm built via a committed build script.
- Village canvas: rooms as map regions (guild hall, tavern, courier post,
  market stalls, keep/vault, manor, census parcels as workshop sprites);
  agent sprites animate from live state (working/idle/hibernated/crashed);
  chat bubbles over-the-head from the WS feed.
- Interactions: click sprite → agent panel (telemetry, TITLE lineage, steer
  dialog); click landmark → management panel (commissions board, vault,
  decrees, mail) — every mutation through command channel.
- Reconnect/resync on daemon restart; no ghost state.
- e2e: headless-browser smoke (or WS-protocol-level simulation if browser
  tooling unavailable in CI — record choice) driving a demo village.
- Branch merged, tag `sprint/09-wasm-frontend`.

## Prompt sequence
1. `01-architect.md` — WS protocol + asset architecture (ADR-0015)
2. `02-daemon-engineer.md` — axum asset serving + WS streams
3. `03-frontend-engineer.md` — village canvas + sprites + bubbles
4. `04-frontend-engineer.md` — management pixel UI + command channel
5. `05-qa-engineer.md` — protocol/e2e suite
6. `06-ci-agent.md` — gate
7. `07-commit-agent.md` — merge & tag
