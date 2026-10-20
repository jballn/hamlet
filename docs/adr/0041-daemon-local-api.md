# ADR-0041 — Daemon local API contract

Status: Accepted
Sprint: 08-cli-tui · Prompt: 01-architect (step 01)
Refs: SPEC §12.1–§12.2, §14; CONVENTIONS §2 (singular governor, purity seam), §3 (state contract); ADR-0003 (tree layout), ADR-0022 (axum)

## Context

SPEC §12 tasks Sprint 08 with defining the daemon-local interface consumed by the CLI, TUI, and future web frontends.
Direct sled or filesystem access by client processes violates CONVENTIONS §2 (daemon is the singular governor) and invites concurrent corruption.
Unix domain sockets versus localhost HTTP+WS were evaluated: localhost HTTP+WS over existing Axum (Sprint 04) is selected because Axum and tokio-tungstenite are already runtime-allowlisted, WS event streaming is native, and HTTP proxies cleanly to future browser/WASM frontends. Localhost binding with 0600 token authentication provides equivalent local security boundaries.

## Decision

1. **Transport & Discovery**: Localhost HTTP+WS server in `hamlet-daemon::local_api` bound strictly to `127.0.0.1:<dynamic_port>`. On startup, daemon writes `run/daemon.endpoint` (`http://127.0.0.1:<port>`), `run/daemon.pid`, and `run/daemon.token` with 0600 file permissions. Files are unlinked on graceful shutdown.
2. **Local Token Authentication**: Requests must supply the 32-byte hex token in `Authorization: Bearer <token>` or `x-hamlet-local-token: <token>`. Validated via constant-time comparison; invalid requests return 401 Unauthorized with standard `ApiErrorResponse`.
3. **Pure Schema Types in `hamlet-core::api`**: `hamlet-core` defines all wire structs, projection views (`CensusRecordView`, `CommissionSummaryView`, `DecreeSummaryView`, `VaultItemView`, `MetricsSummaryView`, `MailMessageSummary`), request/response payloads, and error schemas.
4. **Zero-Orchestration Endpoints**:
   - Projections (GET): `/api/v1/status`, `/api/v1/census`, `/api/v1/commissions`, `/api/v1/commissions/:slug`, `/api/v1/decrees`, `/api/v1/decrees/:name`, `/api/v1/vault`, `/api/v1/metrics`, `/api/v1/mail`.
   - Commands (POST): `/api/v1/commissions/new`, `/api/v1/commissions/close`, `/api/v1/agents/hibernate`, `/api/v1/agents/wake`, `/api/v1/agents/steer`, `/api/v1/mail/send`, `/api/v1/vault/export`, `/api/v1/daemon/shutdown`.
   Handlers delegate directly to existing daemon ledger/stasis/session subsystems without intermediate orchestration logic.
5. **WebSocket Feed**: `/api/v1/events/ws` provides real-time `WsEvent` broadcasts keyed by topic (`census`, `commissions`, `decrees`, `mail`, `metrics`, `stasis`), reusing existing watcher/reaction semantics.

## Consequences

- CLI and TUI become thin clients over HTTP/WS with zero direct filesystem mutation or sled access.
- `hamlet-core` remains side-effect-free, supplying only data definitions and URL route constants.
- Future WASM / web frontends reuse this exact API surface without server changes.
