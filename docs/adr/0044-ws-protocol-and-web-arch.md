# ADR-0044 — WS protocol framing and web architecture

Status: Accepted
Sprint: 09-wasm-frontend · Prompt: 01-architect (step 01)
Refs: SPEC §2, §3, §12.3, §14; CONVENTIONS §2, §3; ADR-0041, ADR-0043

## Context

SPEC §14 deferral requires resolving the WebSocket protocol schema for canvas metrics, chat bubbles, and state sync over the local-API token auth (§12.3). The browser cannot mutate `.hamlet/` directly and requires an 8-bit village canvas mapped to SPEC §2 landmarks. Direct sled telemetry streaming over WS risks unbounded bandwidth, while naive full-state pushes overwhelm clients. We must decide the framing protocol, honesty boundaries for cross-owner mail, client mutation routing, rendering pipeline (2D canvas vs software framebuffer), and asset serving cache contract.

## Decision

1. **WS Frame Schema (`hamlet-core::api`)**: Typed bidirectional framing over `GET /api/v1/events/ws` (token-authenticated via query parameter `?token=` or bearer header):
   - Server frames (`WsServerFrame`): `StateDelta` (snapshots on connect/resync, diffs on change; includes census, ledger commissions, decrees, vault, mail counts, landmarks, escalations), `Metrics` (rate-limited, batched `ParcelMetricTick` slices from sled), `Bubble` (over-the-head events with SPEC §12.3 honesty rule: mail bodies are strictly suppressed; only subject/snippet sent), `CommandAck` (correlation ID, HTTP-equivalent status code, response body, error string), and `ResyncRequired`.
   - Client frames (`WsClientFrame`): `Command` (mapping 1:1 onto ADR-0041/ADR-0043 endpoints: commissions, agent steer/hibernate/wake, mail send, vault export, escalation resolve, daemon stop), `Resync` (request snapshot upon seq gap or daemon restart), `Ping`/`Pong`.
2. **Resync & Sequencing**: Monotonic sequence numbers per connection (`seq: u64`) and ephemeral `daemon_instance_id`. Sequence gaps or instance mismatch immediately flush client caches and trigger a full `StateDelta` snapshot resync.
3. **Web Architecture & Rendering Pipeline (`hamlet-web`)**: Rust compiles to `wasm32-unknown-unknown` (`cdylib` + `rlib`). Rendering utilizes HTML5 2D Canvas driven by a pure-Rust scene-graph view-model (`SceneGraph`, `RenderCommand`, `RenderSprite`, `RenderTile`, `RenderBubble`). Thin JS glue handles blitting (`ctx.drawImage`) with pixel-art integer scaling (`imageSmoothingEnabled = false`). Software framebuffer blitting is rejected due to 60fps IPC bandwidth overhead and lost text fidelity.
4. **Asset Pipeline & Serving Contract**: Landmarks map 1:1 to SPEC §2 on a 16x16 Kenney roguelike tile grid (`village_map.json`). Built artifacts (`index.html`, `hamlet.js`, `style.css`, `hamlet_web.wasm`, sprites, map JSON) are packaged by `scripts/build-web.sh` with a content-addressed SHA-256 version hash into `dist/manifest.json`. Axum serves static assets under `/assets/` with `Cache-Control: public, max-age=31536000, immutable` and `index.html` with `no-cache`.

## Consequences

- `hamlet-core` remains side-effect-free, holding all protocol frames and golden serialization vectors.
- Spectator clients never receive secret keys, unmasked env vars, or cross-owner mail bodies over WS.
- `hamlet-web` view-model is fully testable headlessly in CI without browser or Canvas dependencies.
- Zero runtime allowlist changes: `hamlet-web` uses only `hamlet-core`, `serde`, and `serde_json`.
