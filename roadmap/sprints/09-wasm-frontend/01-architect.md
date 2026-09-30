---
role: architect
sprint: 09
step: 1
---

# 09/01 — WS protocol + frontend asset architecture

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §3 (axum/tungstenite),
§12.3, ADR-0014 (local API — reuse, don't fork), HANDOFF.md (sprint 08).

## Deliverable 1 — WS protocol [DEV → ADR-0015]

Resolve SPEC §14: frame schema over the existing local-API auth (token):
- `state-delta`: projection snapshots + diffs (census, ledger, decrees,
  vault, mail counts) — derived from the ADR-0014 read projections.
- `metrics`: per-parcel/session token/cost/latency ticks from sled (rate-
  limited, batched; never raw sled access from the wire).
- `bubble`: chat-bubble events (mail subject/snippet — body only to the
  owner's session per SPEC §12.3 honesty rule, steer confirmations,
  escalation notices, decree acks).
- `command`: client → daemon mutations mapping 1:1 onto ADR-0014 command
  endpoints; ack frames with correlation ids.
- Resync: full `state-delta` snapshot on (re)connect; sequence numbers;
  client never caches across daemon restarts without resync.

## Deliverable 2 — `hamlet-web` architecture

Wasm crate layout (Rust → wasm32-unknown-unknown; rendering approach:
2D canvas via a thin JS glue layer OR pure-Rust pixel buffer — decide,
record tradeoffs), asset pipeline (sprite sheets for landmarks/agents/
states, tile map JSON = village layout mirroring SPEC §2), build script
`scripts/build-web.sh`, asset serving contract (paths, cache headers,
versioning hash).

## ADR
`docs/adr/0015-ws-protocol-and-web-arch.md`.

Verify: protocol types + golden frame vectors in core; build script produces
a loadable wasm artifact (hello-village placeholder). Finish: HANDOFF.md.
Commit: `feat(core,web): WS protocol + web architecture (ADR-0015)`.
