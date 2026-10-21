# HANDOFF — Sprint 09 WASM 8-bit RPG Frontend

Objective: `OBJECTIVE.md`. Prompts: `01-architect` … `07-commit-agent`.

## Step 01 — architect: WS protocol + frontend asset architecture

Status: **done**. Commit: `feat(core,web): WS protocol + web architecture (ADR-0015)` → actual ADR **ADR-0044** (`0044-ws-protocol-and-web-arch.md`). The prompt cites "ADR-0015", but 0015 is `0015-session-protocol.md` (claimed in Sprint 03); per the `docs/adr/README.md` renumbering rule this work is ADR-0044 and the renumber table was updated. **Later sprint-09 prompts reading "ADR-0015" must read ADR-0044.**

### Done

- **Deliverable 1 — WebSocket Protocol Framing (ADR-0044)**:
  - Added pure wire framing contracts in `hamlet-core::api`:
    - `StateDeltaFrame`: Ephemeral `daemon_instance_id`, monotonic `seq`, `timestamp_unix_ms`, `is_snapshot: bool`, and typed projection slots (`census`, `commissions`, `decrees`, `vault`, `mail_counts`, `landmarks`, `escalations`). Reuses existing projection types from ADR-0041 and ADR-0043.
    - `MetricsBatchFrame` & `ParcelMetricTick`: Rate-limited batched slices of per-parcel telemetry (tokens in/out, micro USD cost, latency ms, memory bytes, CPU percent). Raw sled access is strictly prevented over the wire.
    - `BubbleFrame`, `BubbleTarget`, `BubbleKind`: Over-the-head chat bubbles for sprites. Enforces the **SPEC §12.3 honesty rule**: cross-owner mail bodies are strictly suppressed on the spectator wire; only subject and bounded snippet (`MAX_MAIL_SNIPPET_LEN = 64`) are sent. Steer confirmations, escalation notices, decree acks, and agent speech are all strongly typed.
    - `CommandAckFrame`: Correlated client mutation acknowledgments with `correlation_id`, HTTP-equivalent `status_code`, typed payload, and error message.
    - `WsServerFrame`: Tagged server-to-client enum covering `StateDelta(Box<StateDeltaFrame>)`, `Metrics`, `Bubble`, `CommandAck`, `ResyncRequired`, `Pong`, and `Event(WsEvent)`.
    - `WsClientFrame` & `WsCommandAction`: Tagged client-to-server enum covering `Command(WsCommandFrame)`, `Resync`, `Ping`, `Pong`. `WsCommandAction` maps 1:1 onto all daemon mutation endpoints (commissions, agent steer/hibernate/wake, mail send, vault export, escalation resolve, daemon stop) and provides `endpoint_path()` and `action_name()`.
  - Monotonic sequencing and resync discipline:
    - Sequence numbers (`seq: u64`) increment per connection frame.
    - Daemon restart / instance ID mismatch immediately forces cache invalidation and full snapshot resync. Clients never cache across daemon restarts.
  - Comprehensive unit test suite in `hamlet-core::api` with golden vectors, honesty rule enforcement, and endpoint mapping validation.

- **Deliverable 2 — `hamlet-web` Architecture & Asset Pipeline (ADR-0044)**:
  - WASM crate layout:
    - `crates/hamlet-web/Cargo.toml` configured with `crate-type = ["cdylib", "rlib"]`.
    - Depends exclusively on `hamlet-core`, `serde`, and `serde_json` from the workspace. Zero external dependencies added to the locked stack (`scripts/check-deps.sh` 100% green with 0 violations).
    - Compiles directly to `wasm32-unknown-unknown` without bloated runtimes or external CLI bindings.
  - Rendering pipeline:
    - Evaluated 2D Canvas via thin JS glue vs. pure-Rust software framebuffer. Decided on **HTML5 2D Canvas driven by a pure-Rust scene-graph view-model**. Recorded tradeoffs in ADR-0044: hardware acceleration, crisp integer pixel scaling (`imageSmoothingEnabled = false`), zero-copy sprite blitting, and headless CI testability.
    - View model (`hamlet_web::view_model::VillageViewModel`): Pure state machine that consumes `WsServerFrame`s and generates a headless `SceneGraph` (`RenderLandmark`, `RenderSprite`, `RenderBubble`, `HudData`). Fully unit-tested on host (`cargo test -p hamlet-web`) without browser or WebGL dependencies.
    - C-ABI exports (`hamlet_web::ffi`): `hamlet_web_init`, `hamlet_web_version`, `hamlet_web_alloc`, `hamlet_web_dealloc`, `hamlet_web_process_frame`, `hamlet_web_tick`, `hamlet_web_render_scene_json`.
  - Village layout & tile map:
    - `crates/hamlet-web/assets/village_map.json`: Canonical 32x24 16x16-tile village layout mapping SPEC §2 landmarks 1:1 (Guild Hall, Courier Post, Keep & Vault, Tavern & Stasis, Market Stalls, Mayor's Manor) and 8 workshop parcel slots.
  - Static Web Shell & Thin JS Glue:
    - `crates/hamlet-web/web/index.html`: Canvas container (512x384), status HUD, and accessible landmark navigation region (`aria-live="polite"`).
    - `crates/hamlet-web/web/hamlet.js`: WebSocket connection management with `?token=` query auth, WASM memory bridge, animation frame loop, and 2D canvas blitting.
    - `crates/hamlet-web/web/style.css`: 8-bit retro dark theme with crisp pixelated rendering rules.
  - Build script:
    - `scripts/build-web.sh`: Compiles release wasm, copies static web assets and Kenney roguelike sprite sheets from `assets/`, verifies the `\0asm` WebAssembly magic header, computes a composite SHA-256 version hash, and writes `manifest.json`.
  - Asset serving contract:
    - Defined in ADR-0044 for daemon implementation in step 02 (`02-daemon-engineer.md`): static assets served under `/assets/` with `Cache-Control: public, max-age=31536000, immutable` and content hash ETag; `index.html` served under `/` with `no-cache`.

### Decisions (ADR refs)

- **ADR-0044 (WS Protocol Framing and Web Architecture)**:
  - Next sequential ADR number claimed (0044), superseding prompt's placeholder reference to ADR-0015 (which belongs to Sprint 03).
  - Bidirectional typed framing over `GET /api/v1/events/ws` with local token authentication.
  - SPEC §12.3 honesty rule: mail bodies are strictly suppressed on the spectator WS stream; only sanitized snippet and subject delivered.
  - HTML5 2D Canvas rendering via thin JS glue driven by pure-Rust scene-graph view-model.
  - Content-addressed version hash packaging with immutable cache headers.

### Verify

```sh
# Pure wire types and golden frame vector tests in hamlet-core
cargo test -p hamlet-core --lib api

# Pure view-model and tile map tests in hamlet-web
cargo test -p hamlet-web

# Build release wasm binary and package web distribution bundle
./scripts/build-web.sh

# Verify dependency allowlist
./scripts/check-deps.sh

# Verify formatting and clippy across workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Next Steps (Step 02) — COMPLETED

## Step 02 — daemon-engineer: axum asset serving + WS streams

Status: **done**. Commit: `feat(daemon): asset serving + WS streams (SPEC §12.3, ADR-0015)` (referencing ADR-0044).

### Done

- **Static Asset Serving Layer (`hamlet_daemon::web::AssetServer`)**:
  - Implemented `AssetConfig`, `AssetServer`, and `asset_router` in `crates/hamlet-daemon/src/web.rs`.
  - Serves distribution bundle (`dist/`) packaged by `scripts/build-web.sh`:
    - `index.html` (under `/` and `/index.html`) served with `Cache-Control: no-cache` (SPEC §12.3 & ADR-0044).
    - Built static assets (`/hamlet.js`, `/style.css`, `/hamlet_web.wasm`, `/manifest.json`, `/assets/*`) served with `Cache-Control: public, max-age=31536000, immutable` and `ETag: "\"{version_hash}\""` extracted from `manifest.json`.
    - Conditional request handling: `If-None-Match` header validation returns `304 Not Modified` with empty body.
    - Path traversal defense: strict path component validation rejecting `..` and non-normal segments with `403 Forbidden` / `404 Not Found`, preventing directory traversal outside the configured asset root.
    - Integrated with `local_api_router` so local REST endpoints, WebSocket streaming, and static asset serving share a single server surface.

- **WebSocket Streaming Engine (`hamlet_daemon::web::WsHub`)**:
  - Implemented `WsHub` mounted at `GET /api/v1/events/ws` (and unified in `frontend_router`):
    - **Authentication Handshake**: local token enforcement supporting query param (`?token=`), `Authorization: Bearer <token>`, and custom header `X-Hamlet-Local-Token`. Unauthenticated connections fail with `401 Unauthorized`.
    - **Initial State Snapshot**: immediately upon connection, client receives full `StateDeltaFrame` (`seq == 1`, `is_snapshot == true`, `daemon_instance_id`) with all 7 projection domains populated: `census`, `commissions`, `decrees`, `vault`, `mail_counts`, `landmarks`, and `escalations`.
    - **Monotonic Sequencing & Ephemeral ID**: strictly monotonic per-connection sequence numbering (`seq: u64`) and random daemon instance UUID. Sequence gaps or instance mismatch immediately trigger resync via `WsServerFrame::ResyncRequired`.
    - **Resync & Heartbeats**: handles `WsClientFrame::Ping` with `WsServerFrame::Pong` and `WsClientFrame::Resync` with fresh `StateDeltaFrame` snapshots.
    - **Live Inotify/Watcher Reactions**: `WsHub::handle_reaction` maps reactions to typed deltas and chat bubbles:
      - `Reaction::MailSpool` → mail counts projection delta + mail snippet bubble.
      - `Reaction::DecreeWrite` → decrees projection delta + decree ack bubble.
      - `Reaction::DeedMutation` → census & landmark projection deltas + agent speech bubble.
    - **SPEC §12.3 Honesty Rule**: mail bodies are strictly suppressed on the spectator wire. `BubbleFrame::mail_snippet` enforces bounded snippet (`MAX_MAIL_SNIPPET_LEN = 64`) and sanitized subject; private mail payloads are never exposed to spectators.
    - **Sled Metrics Batching**: `MetricsBatcher` buffers `ParcelMetricTick` telemetry and flushes rate-limited `WsServerFrame::Metrics(MetricsBatchFrame)` envelopes.
    - **Bidirectional Client Mutation Commands**: receives `WsClientFrame::Command(WsCommandFrame)`, invokes corresponding daemon subsystems (`CommissionNew`, `CommissionClose`, `AgentHibernate`, `AgentWake`, `AgentSteer`, `MailSend`, `VaultExport`, `EscalationResolve`, `DaemonStop`), and returns correlated `WsServerFrame::CommandAck(CommandAckFrame)` containing matching `correlation_id`, `command_endpoint`, and response payload.
    - **Multi-Viewer Fan-Out**: concurrent viewers receive independent monotonic sequences without cross-client leakage.
    - **Slow-Client Backpressure Policy**: bounded per-connection send queues (64 slots). Congested queues drop rate-limited `Metrics` frames (`dropped_metrics_count`) while strictly preserving critical `StateDelta`, `Bubble`, and `CommandAck` frames.

- **Workspace Verification & Testing**:
  - Integration tests in `crates/hamlet-daemon/tests/ws.rs` (9 comprehensive integration tests using `tokio-tungstenite`/`tungstenite`):
    - `test_static_asset_serving_and_traversal_defense`
    - `test_ws_auth_handshake_matrix`
    - `test_ws_initial_snapshot_contains_all_seven_projections`
    - `test_ws_ping_pong_and_resync`
    - `test_ws_live_reactions_and_bubble_honesty_rule`
    - `test_ws_metrics_batching_and_flushing`
    - `test_ws_command_round_trip_and_ack_correlation`
    - `test_ws_concurrent_fan_out_to_multiple_viewers`
    - `test_ws_backpressure_slow_client_drops_metrics_preserves_deltas`
  - Fixed `StateDeltaFrame.mail_counts` JSON deserialization with `string_or_int_map` helper for internally tagged `WsServerFrame` variants.
  - All workspace tests (`cargo test --workspace`) 100% green; `scripts/check-deps.sh` 100% green; `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` clean.

### Decisions (ADR refs)

- **ADR-0044 (WS Protocol Framing and Web Architecture)**:
  - Preserved runtime dependency allowlist by adding only `tokio-tungstenite = { workspace = true }` (pre-approved in runtime allowlist).
  - Maintained single-governor principle: WS commands interpret files and delegate to existing command routines without holding separate orchestration state.
  - Honesty rule enforced at the bubble constructor boundary in `web.rs`.

### Verify

```sh
# WS and asset integration tests in hamlet-daemon
cargo test -p hamlet-daemon --test ws

# All daemon tests
cargo test -p hamlet-daemon

# Full workspace test suite
cargo test --workspace

# Dependency allowlist and style checks
./scripts/check-deps.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Next Steps (Step 03) — COMPLETED

## Step 03 — frontend-engineer: village canvas, sprites, chat bubbles

Status: **done**. Commit: `feat(web): 8-bit village canvas + state-driven sprites (SPEC §12.3)`.

### Done

- **Tile Map & Sprite Sheets Architecture (`crates/hamlet-web/src/tiles.rs`, `scene.rs`)**:
  - Embedded normative SPEC §2 Landmark ↔ Tree Topology Mapping Table in doc comments:
    - `guild/` → Guild Hall (commissions ledger, register, blueprints)
    - `courier/` → Courier Post (mail spools, freight, dispatch)
    - `keep/` → Keep & Vault (promoted deliverables, archives)
    - `tavern/` → Tavern & Stasis (hibernated patrons, gossip)
    - `market/` → Market Stalls (skills, pkgcache)
    - `keep/census/manor/` → Mayor's Manor (Mayor orchestrator home, decrees)
    - `keep/census/parcel<uid>/` → Census Workshops (one per active parcel)
  - Modeled `RenderWorkshop` structures in the census zone, tying each active agent lease to its designated workshop structure.
  - Kenney roguelike sprite sheets integration (`characters.png`, `village.png`, `ui.png`) with pixel-art integer scaling (`imageSmoothingEnabled = false`).

- **State-Driven Agent Sprite Animations (`crates/hamlet-web/src/view_model.rs`, `web/hamlet.js`)**:
  - Pure view-model state mapping for animations:
    - `working` = tool-swing animation (oscillating swinging pickaxe/hammer tool frame).
    - `idle` = loiter animation (gentle sine-wave bobbing cycle).
    - `sleeping` / `stasis` = floating animated "zzz" text particles over sprite.
    - `crashed` = fallen orientation (rotated collapsed sprite).
    - `steered` = attention mark alert badge (`!`) rendered above sprite head.
  - Sprite counts and positions derive **exclusively** from incoming `StateDeltaFrame` projections (zero client orchestration state, zero phantom sprites).

- **Chat Bubbles with Queueing, Timeout & Honesty Rule (`crates/hamlet-web/src/view_model.rs`)**:
  - FIFO bubble queue per target: targets display one active bubble at a time; subsequent incoming bubbles queue cleanly and display sequentially upon timeout.
  - Client-side honesty enforcement (SPEC §12.3 defense in depth): `BubbleKind::MailSnippet` display text is clamped to `MAX_MAIL_SNIPPET_LEN` (64 chars) and formatted strictly as `[Mail] <subject>: <snippet>`. Any raw message body is strictly stripped on client presentation.
  - Color-coded borders by bubble kind: gold for mail, blue for steer confirmation, red for escalation notices, green for decree acks, gray for agent speech.

- **Metrics HUD, Sparklines & Frame-Budget Guard (`crates/hamlet-web/src/view_model.rs`, `web/hamlet.js`)**:
  - Per-sprite rolling metric history: buffers recent token/cost activity (`SPARKLINE_HISTORY_LEN = 12`) and renders mini sparkline polylines on canvas.
  - Village-wide ticker tape: captures telemetry ticks, snapshot digests, command acks, and events into rolling ticker buffer (`HudData.ticker`).
  - Frame-budget guard policy (60 FPS target): JS blitter monitors rolling `avgFrameTime`. If frame time exceeds 20ms (> 50fps drop), degraded mode is activated via `hamlet_web_set_degraded(1)`:
    - Chat bubbles degrade first: bubble timeouts accelerate at 2x decay rate, and active rendered bubbles clamp to 2 (prioritizing critical escalations and steers).
    - Sprites, landmarks, workshop structures, and HUD metrics remain at full fidelity.

- **Canvas Interaction & Keyboard Accessibility (`crates/hamlet-web/web/hamlet.js`, `index.html`)**:
  - Click & hover hit-testing on canvas for landmarks, workshops, and sprites.
  - Dispatches custom `hamlet:selection` DOM event with selected entity details for Step 04 interaction panels.
  - Hotkeys 1–6 map to the 6 SPEC §2 landmarks; Tab/Enter navigation on `.nav-btn` list.
  - Real-time accessibility announcements via `#aria-status` (`aria-live="polite"`).

- **Resync & Disconnect Reconnect Backoff**:
  - Exponential backoff with jitter on WebSocket disconnect (500ms, 1000ms, 2000ms, 4000ms, max 8000ms).
  - Complete cache invalidation on daemon instance ID mismatch or `ResyncRequired` server frame.

- **WASM Size Budget & Verification**:
  - WASM binary size: **1,823,601 bytes** (~1.8 MB uncompressed release binary).
  - 13 comprehensive pure unit and golden frame tests in `crates/hamlet-web` (`cargo test -p hamlet-web`).
  - Automated manual smoke harness in `scripts/run-village-demo.sh` (`--smoke` mode verifies daemon boot, fixtures, and HTTP asset serving).
  - Entire CI battery green (`scripts/ci-local.sh` PASS).

### Decisions (ADR refs)

- **ADR-0044 (WS Protocol Framing and Web Architecture)**:
  - Honesty rule client display rule implemented in `view_model.rs` as defense in depth.
  - Frame-budget guard policy: chat bubbles degrade first (2x decay rate, clamp to 2 bubbles prioritizing escalations/steers).
  - Pure view-model scene graph generation decoupled from browser/DOM dependencies.

### Verify

```sh
# Pure view-model unit & golden tests in hamlet-web
cargo test -p hamlet-web

# Build release wasm binary and package web distribution bundle
./scripts/build-web.sh

# Run automated browser smoke test (daemon + demo fixtures + HTTP verification)
./scripts/run-village-demo.sh --smoke

# Verify workspace dependencies and format/clippy
./scripts/check-deps.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Full local CI battery
./scripts/ci-local.sh
```

### Next Steps (Step 04) — COMPLETED

## Step 04 — frontend-engineer: pixel management UI + command channel

Status: **done**. Commit: `feat(web): pixel management UI over command channel (SPEC §12.3)`.

### Done

- **Architecture Choice: DOM-Hybrid Pixel UI Drawer (`crates/hamlet-web/web/`)**:
  - Evaluated pure canvas immediate-mode UI vs. DOM-hybrid overlay drawer. Decided on **DOM-hybrid pixel-art drawer overlay** (`#management-drawer` in `index.html` and `style.css`):
    - *Rationale*: Keeps canvas frame rate strictly locked to 60 FPS without layout computation overhead; provides native browser input focus, accessible screen-reader traversal (`role="dialog"`, `aria-modal="true"`, `aria-labelledby`), native text selection, and form input controls while styled with crisp retro 8-bit CRT aesthetic (scanlines, pixel fonts, custom scrollbars).
    - Driven directly from the pure-Rust view model projections via C-ABI export `hamlet_web_render_panel_json`.

- **Pure View-Model Panel Projections (`crates/hamlet-web/src/panels.rs`, `src/view_model.rs`)**:
  - Implemented strongly typed panel view models:
    1. **Agent panel** (`AgentPanelView`): Rendered on sprite click or hotkey. Displays live telemetry (input/output tokens, micro-USD cost, latency ms, memory, CPU), TITLE lineage digest, DEED lease info, PROPRIETOR identity/role, and action forms: steer directive composer, hibernate agent, and wake agent.
    2. **Guild board** (`GuildBoardView`): Rendered for Guild Hall. Displays commission summaries (active, draft, closed counts), registered worker roster, available blueprints, and per-agent INDENTURE status breakdown. Includes commission create and commission close actions.
    3. **Vault** (`VaultPanelView`): Rendered for Keep. Displays promoted deliverables, artifact size in bytes, SHA-256 checksums, and export action trigger.
    4. **Decree desk** (`DecreeDeskView`): Rendered for Mayor's Manor. Displays decree queue, ACK statuses, replay action trigger for rejected decrees, and escalation inbox with resolve mutation flow.
    5. **Courier** (`CourierPanelView`): Rendered for Post Office. Displays owner-scoped mailbox (inbox/outbox), mail counter, and mail compose form with recipient UID, subject, and body.
    6. **Tavern** (`TavernPanelView`): Rendered for Tavern & Stasis. Displays hibernated patron list with session counts and title digests, and awaken-into-commission flow.
  - Projections derive strictly from `StateDeltaFrame` and `ParcelMetricTick` frames — no panel invents state.

- **Optimistic UI & Command Correlation Engine (`crates/hamlet-web/src/view_model.rs`, `web/hamlet.js`)**:
  - Implemented `OptimisticMutation` tracking state machine:
    - Every mutation dispatches a `WsClientFrame::Command(WsCommandFrame)` with unique correlation ID `corr-<action>-<seq>-<rand>`.
    - Pure view-model registers mutation in `pending_mutations` with `MutationStatus::Pending`.
    - When `WsServerFrame::CommandAck(ack)` arrives:
      - On success (`status_code < 400`): status updates to `MutationStatus::Confirmed`, cleared error state, and ticker logs `[ACK OK] <desc>`.
      - On reject (`status_code >= 400`): status rolls back to `MutationStatus::RolledBack`, `last_mutation_error` is populated, drawer displays prominent reject banner, and ticker logs `[ACK REJECT <status>] <desc>`.
  - Exported C-ABI functions: `hamlet_web_open_panel`, `hamlet_web_close_panel`, `hamlet_web_render_panel_json`, `hamlet_web_record_mutation`.

- **Console Boot Logo & Audio Sting Sound**:
  - User requirement implemented: `assets/audio/toBe.wav` is packaged as the boot sting sound.
  - `scripts/build-web.sh` copies `assets/audio/toBe.wav` to `dist/assets/audio/toBe.wav` and records it in `manifest.json`.
  - `crates/hamlet-daemon/src/web.rs` updated to serve `.wav` with MIME type `audio/wav`.
  - `crates/hamlet-web/web/index.html` displays `#boot-overlay` with 8-bit hamlet console logo.
  - `crates/hamlet-web/web/hamlet.js` triggers audio playback (`assets/audio/toBe.wav`) upon console initialization, with graceful fallback to first user interaction if browser autoplay policy blocks audio.

- **Keyboard Operability & A11y Contract**:
  - `Escape` key immediately closes the active drawer and returns focus to canvas/landmarks.
  - Hotkeys `1`–`6` open corresponding landmark panels: `1` Guild Board, `2` Courier Post, `3` Keep Vault, `4` Tavern, `5` Market Stalls, `6` Mayor's Manor.
  - Tab and Shift+Tab navigate focus cyclically within the drawer; Enter/Space activates actions.
  - ARIA status announcer (`#aria-status` with `aria-live="polite"`) mirrors panel states and mutation results for screen readers.

- **Automated Verification & Golden Suites**:
  - Pure unit and golden tests in `crates/hamlet-web/tests/panel_goldens.rs` (8 tests):
    - `golden_test_agent_panel_projection`
    - `golden_test_guild_board_projection`
    - `golden_test_vault_panel_projection`
    - `golden_test_decree_desk_projection`
    - `golden_test_courier_panel_projection`
    - `golden_test_tavern_panel_projection`
    - `golden_test_optimistic_ui_mutation_and_rollback_on_reject`
    - `golden_test_a11y_keymap_table`
  - Integration round-trip and rejection test in `crates/hamlet-daemon/tests/ws.rs:test_ws_command_round_trip_and_ack_correlation` (exercises commission create, commission close, mail send, and vault export rejection).
  - Smoke test verification in `scripts/run-village-demo.sh --smoke` verifying daemon boot, static web files, WASM magic header, and `toBe.wav` audio serving.

### Decisions (ADR refs)

- **ADR-0044 (WS Protocol Framing and Web Architecture)**:
  - DOM-hybrid pixel UI drawer overlay adopted for complex work management panels, preserving canvas rendering performance while providing full keyboard/accessibility support.
  - Command channel correlation with optimistic UI state machine (`Pending` -> `Confirmed` / `RolledBack`).
  - Audio asset MIME type resolution (`audio/wav`) integrated into daemon static asset server.

### Verify

```sh
# Pure panel view-model golden tests
cargo test -p hamlet-web --test panel_goldens

# Build release wasm binary with audio assets
./scripts/build-web.sh

# Automated smoke test (daemon boot, web assets, and audio serving)
./scripts/run-village-demo.sh --smoke

# Check dependencies allowlist
./scripts/check-deps.sh

# Formatting, clippy, and full local CI suite
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/ci-local.sh
```

### Next Steps (Step 05) — COMPLETED

## Step 05 — qa-engineer: Protocol & e2e suite

Status: **done**. Verdict: `GATE 09/05: PASS` (Remediated by Daemon Engineer; all findings verified and resolved; full workspace CI and e2e test suite passing).

### Findings & Remediations

#### QA09-01 (HIGH — Malformed WebSocket Command Frames Silently Dropped with Zero Response Instead of Returning Typed Error Ack) — RESOLVED
- **File:Line**: `crates/hamlet-daemon/src/web.rs:1694-1721`
- **Repro Command**:
  ```sh
  cargo test -p hamlet-daemon --test adversarial_ws test_repro_qa09_01_malformed_command_dropped_instead_of_error_ack -- --nocapture
  ```
- **Expected vs Actual**:
  - *Expected*: Per Sprint 09 Step 5 Committed Suite 1 ("malformed command frames → typed error acks, no daemon panic"), any command frame received over WebSocket with an invalid action, missing required fields, or unparseable payload (e.g. invalid `SafeName` in `slug` or `CommissionCloseRequest` missing required fields) must return a typed `CommandAckFrame` with HTTP-equivalent 400 Bad Request status code, matching `correlation_id`, and descriptive error message. This allows the client UI to display the rejection banner and roll back optimistic mutation state.
  - *Actual*: In `crates/hamlet-daemon/src/web.rs:1695`, incoming frames were deserialized with `if let Ok(client_frame) = serde_json::from_str::<WsClientFrame>(&text)`. When `serde_json::from_str` failed on malformed command actions or invalid payloads, the `if let` block failed silently with no `else` branch.
- **Remediation**: In `crates/hamlet-daemon/src/web.rs:1700-1720`, if `WsClientFrame` deserialization fails and the JSON object has `"type": "command"`, the daemon inspects `correlation_id` (if present) and immediately emits a typed `WsServerFrame::CommandAck(CommandAckFrame::error(seq, now, correlation_id, "/api/v1/command", 400, format!("malformed command frame: {err}")))`. Non-command wire garbage without correlation ID is ignored gracefully without polluting client frames. Verified in `test_repro_qa09_01_malformed_command_dropped_instead_of_error_ack`.

#### QA09-02 (HIGH — Client Command Spam / Broadcast Channel Lag Permanently Terminates Forwarder Task and Starves Concurrent Spectators) — RESOLVED
- **File:Line**: `crates/hamlet-daemon/src/web.rs:1623-1626`
- **Repro Command**:
  ```sh
  cargo test -p hamlet-daemon --test adversarial_ws test_repro_qa09_02_client_flood_starves_viewers_via_lagged_error -- --nocapture
  ```
- **Expected vs Actual**:
  - *Expected*: Per Sprint 09 Step 5 Committed Suite 4 ("Backpressure: ... client flood (command spam) rate-limited without starving other viewers"), flooding commands from one client or a burst of broadcast events must not starve or disconnect other concurrent viewers. If broadcast channel buffer capacity (512 items) is exceeded, lagged receivers must recover, resync, or skip dropped telemetry ticks while keeping the forwarder task alive.
  - *Actual*: In `crates/hamlet-daemon/src/web.rs:1623`, the forwarder task used `while let Ok(msg) = bcast_rx.recv().await`. When the broadcast channel lagged, `bcast_rx.recv()` returned `Err(tokio::sync::broadcast::error::RecvError::Lagged(count))`. This terminated the loop and aborted the spectator's forwarder task permanently.
- **Remediation**: In `crates/hamlet-daemon/src/web.rs:1630-1650`, the forwarder explicitly matches on `bcast_rx.recv().await`. On `broadcast::error::RecvError::Lagged(skipped)`, it queues `WsServerFrame::ResyncRequired` and continues the forwarder loop rather than exiting. Concurrently, `handle_socket` enforces bounded per-connection rate-limiting (leaky bucket) to throttle command floods. Verified in `test_repro_qa09_02_client_flood_starves_viewers_via_lagged_error`.

#### QA09-03 (MEDIUM — View-Model Ignores Sequence-Number Gaps Without Triggering Snapshot Resync) — RESOLVED
- **File:Line**: `crates/hamlet-web/src/view_model.rs:247`, `crates/hamlet-web/src/view_model.rs:303`, `crates/hamlet-web/src/view_model.rs:336`, `crates/hamlet-web/web/hamlet.js:165-183`
- **Repro Command**:
  ```sh
  cargo test -p hamlet-web --test adversarial_web test_repro_qa09_03_view_model_sequence_gap_ignored -- --nocapture
  ```
- **Expected vs Actual**:
  - *Expected*: Per ADR-0044 cl. 2 and Sprint 09 Step 5 Committed Suite 1 ("sequence-number gaps trigger resync"), monotonic sequence numbers increment per connection frame (`seq: u64`). When a frame arrives with `seq > last_seq + 1` (indicating frames were dropped or missed in transit), the view-model / client glue must detect the sequence gap, invalidate incremental diffs, and dispatch a `WsClientFrame::Resync { last_seen_seq, daemon_instance_id }` request to the daemon to receive a fresh full snapshot.
  - *Actual*: In `crates/hamlet-web/src/view_model.rs`, frames were accepted with `self.last_seq = frame.seq` with zero gap detection. Gaps in the stream were silently accepted.
- **Remediation**: In `crates/hamlet-web/src/view_model.rs`, added `resync_needed: bool` to `VillageViewModel`. When `!is_snapshot && self.last_seq > 0 && seq > self.last_seq + 1`, `self.resync_needed` is set to `true` and a ticker entry `[GAP] Missing frames in stream (seq {} -> {})` is recorded. In `crates/hamlet-web/src/ffi.rs`, exported `hamlet_web_resync_needed()` and `hamlet_web_clear_resync_needed()`. In `crates/hamlet-web/web/hamlet.js`, `ws.onmessage` checks `hamlet_web_resync_needed()` and sends `{ type: 'resync', last_seen_seq: lastSeq, daemon_instance_id: daemonInstanceId }`. Verified in `test_repro_qa09_03_view_model_sequence_gap_ignored`.

### Committed Suite

1. **Protocol Conformance Suite**:
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_protocol_all_adr0044_frames_round_trip`: Full golden round-trip serialization and deserialization across all server and client frames (`StateDeltaFrame`, `MetricsBatchFrame`, `BubbleFrame` (all 5 variants), `CommandAckFrame` (success/error), `ResyncRequired`, `Ping`, `Pong`, `Event`).
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_protocol_unknown_frames_ignored_gracefully`: Verified unknown frame types and forward-compatible schemas are gracefully ignored without breaking WebSocket connection.
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_protocol_wire_garbage_and_fuzz_safety`: Streaming wire garbage (non-JSON, raw HTTP text, unclosed objects, deep nesting, null bytes) does not panic daemon or abort process.
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_protocol_auth_rejection_matrix`: Verified 401 Unauthorized for missing tokens, empty tokens, forged query tokens, and forged bearer headers; verified connection for valid query tokens, Bearer headers, and `X-Hamlet-Local-Token`.
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_repro_qa09_01_malformed_command_dropped_instead_of_error_ack`: Pinned reproduction of QA09-01.

2. **Honesty Rules**:
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_honesty_spectator_never_receives_secrets_or_cross_owner_mail_bodies`: Over a fully populated demo village with secrets in `.env` and confidential mail bodies, verified that `.env` keys, raw sled keys, and full mail bodies are NEVER present on the wire. Snippet length is strictly clamped to `MAX_MAIL_SNIPPET_LEN` (64 chars).
   - `crates/hamlet-web/tests/adversarial_web.rs::test_adversarial_view_model_honesty_isolation`: Proved client-side rendering of owner-only data is impossible from wire content alone.

3. **State Fidelity (Cross-Surface Parity)**:
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_state_fidelity_snapshot_projections_parity_with_local_api`: Snapshot projection values (`census`, `commissions`, `decrees`, `vault`, `landmarks`) byte-and-struct identical to local REST endpoints (`ENDPOINT_CENSUS`, `ENDPOINT_COMMISSIONS`, `ENDPOINT_DECREES`, `ENDPOINT_VAULT`, `ENDPOINT_LANDMARKS`).
   - `crates/hamlet-web/tests/adversarial_web.rs::test_adversarial_view_model_state_fidelity_five_checkpoints`: View model state reconstructed across 5 checkpoints (1: Initial -> 2: Commission -> 3: Work -> 4: Promote/Vault -> 5: Hibernate/Stasis) matches CLI `--json` output projections.

4. **Backpressure & Slow Client**:
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_backpressure_slow_client_drops_metrics_preserves_deltas`: Slow client queue congestion drops rate-limited `MetricsBatchFrame`s (`dropped_metrics > 0`) while strictly preserving critical `StateDeltaFrame`s.
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_repro_qa09_02_client_flood_starves_viewers_via_lagged_error`: Pinned reproduction of QA09-02.

5. **Reconnect & Zero Ghosts**:
   - `crates/hamlet-daemon/tests/adversarial_ws.rs::test_reconnect_daemon_kill_and_resync_no_ghosts`: Daemon instance ID mismatch immediately responds with `ResyncRequired` notice and full snapshot; view model flushes cache, ensuring zero ghost agents or projections.

6. **End-to-End Suite Harness**:
   - `scripts/e2e-village-web.sh`:
     - T1: Full lifecycle e2e flow (daemon init/start -> realistic fixtures -> headless browser smoke via Chromium -> WS simulation harness driving full interactions across Guild, Courier, Vault, and Closing -> byte-identical vault artifact export -> clean stop).
     - Record shipped tooling: `headless-browser smoke (chromium) + WS simulation harness`.
     - T2: Adversarial suites execution (10 ws + 4 web + 9 daemon ws + 8 panel + 6 view model = 37 tests).
     - T3: Source-level ratchets (WASM binary size within 2MB budget, static asset traversal defense, audio sting packaging, bounded drop backpressure).
     - T4: Guarded podman accounting.

### Verify

```sh
# Run new Sprint 09 end-to-end integration and adversarial harness
./scripts/e2e-village-web.sh

# Run adversarial WebSocket tests in daemon
cargo test -p hamlet-daemon --test adversarial_ws

# Run adversarial view-model tests in hamlet-web
cargo test -p hamlet-web --test adversarial_web

# Run full CI battery including all e2e scripts
./scripts/ci-local.sh
```

### Next Steps (Step 06) — COMPLETED

## Step 06 — ci-agent: Sprint 09 verification battery & gate verdict (GATE 09/06: PASS)

Status: **done, gate PASS**. Precondition `GATE 09/05: PASS` verified. Strict role boundary maintained: **no product code was edited**; only `scripts/ci-local.sh` was updated to incorporate the `scripts/build-web.sh` WASM build step and host precondition environment (`TMPDIR`, `HAMLET_PODMAN_TESTS`, `RUST_TEST_THREADS`).

### Verification Battery Results

| Check / Gate | Target / Scope | Result | Details |
|---|---|---|---|
| `scripts/build-web.sh` | WASM build & web asset packaging | **PASS** | `hamlet_web.wasm` built (1,875,246 bytes), `\0asm` magic header verified, `manifest.json` written with version hash `7f28938ad4da82535bd62f8ac2cfb9b2bfc54430719618e6a5aab11297b858c0` |
| `cargo fmt --all --check` | Workspace-wide code formatting | **PASS** | Cleanly formatted across all workspace crates |
| `cargo clippy --workspace --all-targets -- -D warnings` | Workspace-wide static analysis | **PASS** | 0 warnings across all targets |
| `cargo test --workspace` | Full workspace unit, integration, & golden suites | **PASS** | All unit, integration, golden, and adversarial tests green (100% pass) |
| `scripts/check-deps.sh` | Dependency graph & runtime allowlist | **PASS** | 0 violations against locked dependency allowlist |
| Guarded Podman Suite (`scripts/guarded-entry.sh`) | 7 guarded podman suites (`HAMLET_PODMAN_TESTS=1`) | **PASS** | All seven suites ran and passed (`podman_guarded`, `bootstrap_guarded`, `provision_guarded`, `escape_guarded`, `freight_guarded`, `supervise_guarded`, `manor_guarded`) |
| `scripts/e2e-01-safename-compile-fail.sh` | SafeName compile-time failure | **PASS** | T1 compile-fail boundary type-enforced |
| `scripts/e2e-01-topology-state.sh` | Topology & state invariant e2e | **PASS** | T1-T7 normative dirs, permissions (0600 .env), crash recovery green |
| `scripts/e2e-02-isolation.sh` | Subuid isolation e2e | **PASS** | T1-T4 isolation suite, source ratchets, .env single seam, escape suite green |
| `scripts/e2e-03-adversarial.sh` | Adversarial boundary harness | **PASS** | T1-T4 mock agent, tail-loss, and unmasking seam ratchets green |
| `scripts/e2e-04-comms.sh` | Inter-agent communications e2e | **PASS** | T1-T3 adversarial comms (29/29), mail parse, constant-time auth, freight suite green |
| `scripts/e2e-05-mayor-adversarial.sh` | Mayor adversarial e2e harness | **PASS** | T1-T3 decree security, provenance, wake queue deduplication, manor suite green |
| `scripts/e2e-06-guard-mutations.sh` | Guard mutation testing e2e | **PASS** | T1-T3 guard acceptance (15/15) and all guard mutations killed |
| `scripts/e2e-07-ci-abort.sh` | CI abort & verdict backstop e2e | **PASS** | T1-T3 parse/source aborts and host lock concurrency guards green |
| `scripts/e2e-07-stasis.sh` | Stasis lifecycle flow & adversarial suite | **PASS** | T1-T3 CLI stasis flow, 26 adversarial tests, lineage and error ratchets green |
| `scripts/e2e-commission.sh` | Commission lifecycle flow & adversarial suite | **PASS** | T1-T3 CLI commission flow, 22 adversarial tests, git mediation ratchets green |
| `scripts/e2e-village-demo.sh` | Sprint 08 village demo & interface matrix | **PASS** | T1-T3 4-tier flow, TUI headless capture, CLI verbs, 18 interface/view tests green |
| `scripts/e2e-village-web.sh` | Sprint 09 WASM web frontend e2e | **PASS** | T1-T4 full lifecycle, Chromium headless DOM smoke, WS simulation harness across all panels, 37 adversarial ws/web tests green |
| `scripts/ci-local.sh` | Full verification battery execution | **PASS** | 100% clean exit across all gates and e2e scripts |

### ADR-0015 (ADR-0044) Implementation Verification

Per the ADR index (`docs/adr/README.md`), Sprint 09 "ADR-0015" corresponds to **ADR-0044** (`0044-ws-protocol-and-web-arch.md`). Implementation fidelity was verified:

1. **WebSocket Protocol Framing (`hamlet-core::api`)**:
   - Pure framing types defined in `crates/hamlet-core/src/api.rs`: `StateDeltaFrame`, `MetricsBatchFrame`, `BubbleFrame` (all 5 kinds), `CommandAckFrame`, `WsServerFrame`, `WsClientFrame`.
   - Monotonic sequencing (`seq: u64`) and random ephemeral `daemon_instance_id`. Sequence gaps or daemon restart mismatch immediately trigger full snapshot resync via `WsServerFrame::ResyncRequired` and client cache flush.
   - SPEC §12.3 Honesty Rule: Private mail bodies are strictly suppressed on the spectator WebSocket stream; only subject and bounded snippet (`MAX_MAIL_SNIPPET_LEN = 64`) are transmitted. Enforced both at daemon construction (`BubbleFrame::mail_snippet`) and client view-model rendering.

2. **WASM View-Model Architecture & Asset Pipeline (`hamlet-web`)**:
   - `crates/hamlet-web` compiles to `wasm32-unknown-unknown` (`cdylib` + `rlib`) using exclusively `hamlet-core`, `serde`, and `serde_json`. Zero dependencies added to the locked stack (`scripts/check-deps.sh` 100% clean).
   - HTML5 2D Canvas driven by pure-Rust scene-graph view-model (`VillageViewModel` -> `SceneGraph`), fully testable headlessly in CI without browser or Canvas dependencies.
   - Pixel-art integer scaling (`imageSmoothingEnabled = false`) with Kenney roguelike sprite sheets.
   - DOM-hybrid pixel management drawer (`#management-drawer`) driven by typed panel projections (`AgentPanelView`, `GuildBoardView`, `VaultPanelView`, `DecreeDeskView`, `CourierPanelView`, `TavernPanelView`).
   - Optimistic UI command correlation engine (`Pending` -> `Confirmed` / `RolledBack` with error banner).
   - Frame budget guard: chat bubbles degrade first (2x decay rate, clamped to 2 bubbles prioritizing escalations/steers) if frame time exceeds 20ms (> 50fps drop).
   - Audio sting sound: `assets/audio/toBe.wav` packaged and served with MIME type `audio/wav`.

3. **Asset Serving & Security Policy (`hamlet-daemon::web`)**:
   - Axum static asset server serves distribution bundle (`dist/`) with `Cache-Control: no-cache` for `index.html` and `Cache-Control: public, max-age=31536000, immutable` with composite SHA-256 version hash ETag for static assets (`/assets/*`).
   - Path traversal defenses reject `..` and foreign paths with `403 Forbidden` / `404 Not Found`.
   - Local token authentication enforced across REST and WebSocket (`?token=`, `Authorization: Bearer <token>`, `X-Hamlet-Local-Token`).
   - Slow-client backpressure: bounded queues drop metrics frames on congestion while strictly preserving critical state deltas and command acks.

### WASM Artifact Details

- **Binary Path**: `crates/hamlet-web/dist/hamlet_web.wasm`
- **WASM Binary Size**: **1,875,246 bytes** (~1.88 MB, well within the 2.0 MB budget)
- **Version Hash (SHA-256)**: `7f28938ad4da82535bd62f8ac2cfb9b2bfc54430719618e6a5aab11297b858c0`
- **Magic Header**: `0061736d` (`\0asm` valid WebAssembly magic)
- **Shipped Tooling**: Headless browser smoke (Chromium) + WebSocket simulation test harness in `scripts/e2e-village-web.sh`

### Gate Verdict

`GATE 09/06: PASS`

---

## Step 07 — commit-agent: WASM frontend merge & tag

Status: **done**. Precondition `GATE 09/06: PASS` verified.

### Commits (`sprint/09-wasm-frontend`, in order)

| SHA | Scope | Why |
| --- | ----- | --- |
| `2c65b6f` | `feat(core,web)` | WS protocol + web architecture (ADR-0015 / ADR-0044) |
| `f6a14c5` | `feat(daemon)` | asset serving + WS streams (SPEC §12.3, ADR-0015 / ADR-0044) |
| `900237c` | `feat(web)` | 8-bit village canvas + state-driven sprites (SPEC §12.3) |
| `311c9d0` | `feat(web)` | pixel management UI over command channel (SPEC §12.3) |
| `26bb227` | `test(web,daemon)` | e2e suite + resolve QA09-01, QA09-02, QA09-03 protocol findings |
| `a11b200` | `ci(scripts,roadmap)` | wire build-web into ci-local and record GATE 09/06 PASS in HANDOFF |

### Merge & tag

- **Squash-merged** `sprint/09-wasm-frontend` → `main`: **`08ba61a`** (575 files changed, +11374, -300).
- **Tagged** `sprint/09-wasm-frontend` → `08ba61a`.
- **Battery on merged main**: `./scripts/ci-local.sh` PASS (fmt, clippy, check-deps, all workspace tests, village disk policy, guarded podman battery, and all 12 e2e scripts green).


