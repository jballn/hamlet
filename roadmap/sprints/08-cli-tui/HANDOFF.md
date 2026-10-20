# HANDOFF — Sprint 08 CLI & TUI

Objective: `OBJECTIVE.md`. Prompts: `01-architect` … `07-commit-agent`.

## Step 01 — architect: CLI surface + daemon local API contract

Status: **done**. Commit: `feat(core,daemon): local API contract + CLI/TUI specs (ADR-0014)` → actual ADRs **ADR-0041** (daemon local API contract) and **ADR-0042** (CLI command tree and TUI architecture). The prompt cites "ADR-0014", but 0014 is `0014-parcel-env-policy.md` (claimed in Sprint 02); per the `docs/adr/README.md` renumbering rule this work is ADR-0041/0042 and the renumber table was updated. **Later sprint-08 prompts reading "ADR-0014" must read ADR-0041/0042.**

### Done

- **Deliverable 1 — Local API Contract (ADR-0041)**:
  - **Transport & Discovery**:
    - Axum HTTP+WS localhost server in `hamlet-daemon::local_api` bound to `127.0.0.1:<dynamic_port>`.
    - Discovery files written on startup with 0600 permissions:
      - `run/daemon.endpoint` containing `http://127.0.0.1:<port>`
      - `run/daemon.pid` containing daemon process ID
      - `run/daemon.token` containing 32-byte hex authentication token
    - Clean teardown unlinks all three files via `cleanup_daemon_files`.
  - **Local Token Authentication**:
    - Requests require token via `Authorization: Bearer <token>` or `x-hamlet-local-token: <token>`.
    - Validated using constant-time comparison against `LocalToken`. Unauthenticated or invalid requests receive HTTP 401 with typed `ApiErrorResponse { error: "unauthorized" }`.
  - **Pure Wire Contracts in `hamlet-core::api`**:
    - Endpoint path constants: `ENDPOINT_STATUS`, `ENDPOINT_CENSUS`, `ENDPOINT_COMMISSIONS`, `ENDPOINT_COMMISSION_NEW`, `ENDPOINT_COMMISSION_CLOSE`, `ENDPOINT_DECREES`, `ENDPOINT_VAULT`, `ENDPOINT_VAULT_EXPORT`, `ENDPOINT_METRICS`, `ENDPOINT_MAIL`, `ENDPOINT_MAIL_SEND`, `ENDPOINT_AGENT_HIBERNATE`, `ENDPOINT_AGENT_WAKE`, `ENDPOINT_AGENT_STEER`, `ENDPOINT_DAEMON_SHUTDOWN`, `ENDPOINT_EVENTS_WS`.
    - State projections: `CensusRecordView`, `CommissionSummaryView`, `CommissionDetailView`, `DecreeSummaryView`, `DecreeDetailView`, `VaultItemView`, `MetricsSummaryView`, `MailMessageSummary`, `DaemonStatusResponse`.
    - Command request/responses: `CommissionNewRequest/Response`, `CommissionCloseRequest/Response`, `AgentHibernateRequest/Response`, `AgentWakeRequest/Response`, `AgentSteerRequest/Response`, `MailSendRequest/Response`, `VaultExportRequest/Response`.
    - WebSocket models: `WsEvent`, `WsTopic`.
  - **Zero-Orchestration Daemon Routes (`hamlet-daemon::local_api`)**:
    - Read-only projections: `/api/v1/status`, `/api/v1/census`, `/api/v1/commissions`, `/api/v1/commissions/:slug`, `/api/v1/decrees`, `/api/v1/decrees/:name`, `/api/v1/vault`, `/api/v1/metrics`, `/api/v1/mail`.
    - Command endpoints: `/api/v1/commissions/new`, `/api/v1/commissions/close`, `/api/v1/agents/hibernate`, `/api/v1/agents/wake`, `/api/v1/agents/steer`, `/api/v1/mail/send`, `/api/v1/vault/export`, `/api/v1/daemon/shutdown`.
    - WebSocket stream: `/api/v1/events/ws` broadcasting real-time events.
    - Strict architectural rule: Handlers only project state and invoke existing daemon subsystems (`Ledger`, `Supervisor`, `ProductionStasis`, `DecreeExecutor`, `Vault`); zero added orchestration logic.
  - **Tree Accessors (`hamlet-core::tree`)**:
    - Added `daemon_pid() -> PathBuf`, `daemon_token() -> PathBuf`, `daemon_endpoint() -> PathBuf` pointing to `run/daemon.{pid,token,endpoint}`.
- **Deliverable 2 — CLI Command Tree Spec (ADR-0042)**:
  - Command tree table covering full SPEC §12.1 surface: `init`, `daemon` (`run`, `status`, `stop`), `commission` (`list`, `new`, `show`, `close`), `agent` (`list`, `show`, `hibernate`, `wake`, `steer`), `decree` (`list`, `show`, `escalate`), `mail` (`list`, `send`, `show`), `vault` (`list`, `export`), `metrics` (`--live`), and `tui` (`--room`).
  - Standard flags: `--dir <PATH>` (village root), `--json` (machine-readable stdout), `--quiet`, `--verbose`.
  - Exit code policy: `0` = Success, `1` = Domain/operational failure, `2` = Usage/parse error, `3` = Daemon unreachable.
  - Verb migration map: explicitly identifies verbs migrated from earlier sprints versus new verbs.
- **Deliverable 3 — TUI Information Architecture (ADR-0042)**:
  - Landmark room graph: Guild (commissions, active parcels), Tavern (patron census, stasis archives), Courier (mail outbox/inbox, webhook logs), Market (vault blueprints, freight transfers), Keep (decrees, charter, escalations), Manor (metrics, daemon health, host resource telemetry).
  - Navigation: `Tab`/`Shift-Tab` room cycling, `1-6` hotkeys, `h/j/k/l` list traversal, `?` help, `q` quit.
  - WS stream ticker: bottom strip displaying live events.
  - Modals: Steer (`s`), Decree review (`d`), Escalation inbox (`e`).
  - Constraint: Pure client over HTTP/WS; zero state mutation and zero direct filesystem access.
- **Dependency Allowlist Updates**:
  - `clap`, `ratatui`, `crossterm` promoted from `DEV_ALLOW` to `RUNTIME_ALLOW` in `scripts/check-deps.sh` per ADR-0042.

### Decisions (ADR refs)

- **ADR-0041 (Daemon Local API Contract)**:
  - Localhost HTTP+WS axum server on dynamic port (`127.0.0.1:0`).
  - Local auth token stored in `run/daemon.token` (0600) and verified via constant-time equality.
  - Zero-orchestration handlers delegating directly to daemon services.
- **ADR-0042 (CLI Command Tree & TUI Architecture)**:
  - Runtime promotion of `clap`, `ratatui`, and `crossterm`.
  - Standard exit code policy (0/1/2/3) and `--json` support across all CLI verbs.
  - Landmark room layout and pure view model for ratatui TUI over HTTP/WS.

### Verify

```sh
cargo test -p hamlet-core --lib api              # pure wire types and schema tests
cargo test -p hamlet-core --lib tree             # daemon run path accessors
cargo test -p hamlet-daemon --test local_api     # 7: table-driven route matrix (unauth/auth), status, commissions, mail, vault, decrees, discovery lifecycle
./scripts/check-deps.sh                          # verify runtime allowlist (clap, ratatui, crossterm promoted)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Next Steps (Step 02)

- CLI Engineer implements Step 02 (`02-cli-engineer.md`): CLI consolidation, clap command tree, local API client, exit code contract, `--json` formatting, and tests.

## Step 02 — systems-engineer: Full CLI implementation

Status: **done**. Commit: `feat(cli): full normative CLI surface over local API (SPEC §12.1)`

### Done

- **Full CLI Command Tree over Local API (SPEC §12.1, ADR-0041, ADR-0042)**:
  - Implemented client architecture in `crates/hamlet-cli/`:
    - `client.rs`: `HamletClient` HTTP client with automatic discovery via `.hamlet/run/daemon.{endpoint,token}`, typed error dispatch (`Usage` -> exit code 2, `DaemonUnreachable` -> exit code 3, `Api`/`Operational` -> exit code 1), and bearer token authentication.
    - `daemon.rs`: `start` (daemonize child process with 5s polling readiness probe or `--fg`), `run` (foreground governor alias), `status` (renders operator heartbeat at-a-glance view: PID, version, uptime, village root, active parcels, commissions, pending decrees, queued mail counts), and `stop` (graceful shutdown over local API).
    - `commission.rs`: `list` (table formatted matching goldens), `new` (`--brief <file|->` and `--seed <dir>`), `show` (renders commission metadata, brief, and indenture entries), and `close` (marks commission closed, unlinks leases).
    - `agent.rs`: `list` (table formatted census with UID, identity, commission, state, sessions, traces, last_event), `show` (detailed parcel inspection), `hibernate` (moves parcel to patron stasis archive), `wake` (awakens patron with optional commission binding), and `steer` (injects directive into active session).
    - `decree.rs`: `list` (table of decrees with decree ID, sequence, action, status, reason) and `show` (full detail with JSON body and ack verdict).
    - `mail.rs`: `send` (sends message with optional `--from`), `list` (inbox/outbox overview), and `read` (full message inspection).
    - `vault.rs`: `list` (vault artifacts table with commission, size, sha256) and `export` (copies artifact with ADR-0007 symlink refusal, cross-commission ambiguity detection, and resolved commission provenance).
    - `metrics.rs`: `show` (summary totals and per-agent metrics breakdown) and `export` (dumps formatted JSON to file or stdout).
    - `completions.rs`: hand-rolled shell completion scripts for `bash`, `zsh`, and `fish` without external dependencies.
    - `init.rs`: retained existing atomic filesystem bootstrap with added `--json` support.
    - `main.rs`: unified clap derive entrypoint enforcing global `--dir`, `--json`, `-q`/`--quiet`, `-v`/`--verbose`, and strict exit code mapping (0 = success, 1 = domain/operational error, 2 = usage/parse error, 3 = daemon unreachable).
- **Daemon Extensions (`hamlet-daemon`)**:
  - `serve_daemon`: dynamic or fixed TCP port listener, atomic discovery file writing (`tempfile -> fsync -> rename -> dir fsync`), Linux `/proc/<pid>` PID liveness check, graceful shutdown on signal or `/api/v1/daemon/shutdown`, automatic unlinking of discovery files.
  - Added `mail_message_handler` for `GET /api/v1/mail/:uid/:file`.
  - Added resolution of omitted commission in `vault_export_handler` with 409 Conflict on ambiguous names across commissions and ADR-0007 symlink refusal on destination.
- **Dependency & Standards Compliance**:
  - Locked runtime dependency allowlist strictly preserved (0 violations on `./scripts/check-deps.sh`).
  - No wall-clock in state derivation, atomic writes (`temp -> fsync -> rename`) throughout.
  - 100% clean formatting (`cargo fmt --all --check`) and clippy clean (`cargo clippy --workspace --all-targets -- -D warnings`).
- **Tests & Golden Verification**:
  - `init_golden.rs` (6 tests passing).
  - `adversarial_init.rs` (13 tests passing).
  - `commission_golden.rs` (3 tests passing with live daemon harness).
  - `vault_golden.rs` (3 tests passing with live daemon harness).
  - `agent_golden.rs` (7 tests passing with live daemon harness and exit code contract).
  - `cli_integration.rs` (7 comprehensive tests verifying daemon lifecycle, kill/restart resumability, mail, decree, metrics, steer, shell completions, and exit code contract 0/1/2/3).
  - Full workspace test suite passing (`cargo test --workspace`).

### Verify

```sh
cargo test -p hamlet-cli
cargo test -p hamlet-cli --test cli_integration
./scripts/check-deps.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Next Steps (Step 03)

- TUI Engineer implements Step 03: terminal MUD interactive TUI (`hamlet-tui`) connecting over local API and event stream.

## Step 03 — frontend-engineer: MUD-style TUI village (`hamlet-tui`)

Status: **done**. Commit: `feat(tui): MUD-style village over local API (SPEC §12.2)`

### Done

- **ADR-0043 (TUI Rooms & Landmark Projection)**:
  - Authored `docs/adr/0043-tui-rooms-landmark-projection.md` defining the 1:1 mapping between SPEC §2 normative village landmarks and TUI rooms:
    - Guild (`guild/`): commission ledgers, active blueprints, indenture statuses.
    - Tavern (`tavern/`): gossip feed, patron stasis archives.
    - Courier (`courier/`): mail spools, outbox/inbox, dispatch streams.
    - Market (`market/`): skill catalogs, pkgcache registry statistics.
    - Keep (`keep/`): vault artifacts, decree history, active parcel census with live telemetry.
    - Manor (`keep/census/manor/`): Mayor wake history, decree queue, pending acks, escalation inbox.
  - Defined composite HTTP projection endpoint `GET /api/v1/landmarks` (`LandmarksView`) to deliver full village topology in a single roundtrip.
  - Defined escalation inspection & resolution endpoints `GET /api/v1/escalations` and `POST /api/v1/escalations/:id/resolve` (`EscalationResolveRequest/Response`).
  - Defined WS event broadcast for mutating command completions (`cmd_steer`, `cmd_mail`, `cmd_escalation_resolve`).
- **Daemon Extensions (`hamlet-core`, `hamlet-daemon`)**:
  - `hamlet-core::api`: Added `LandmarksView`, `GuildLandmark`, `TavernLandmark`, `CourierLandmark`, `MarketLandmark`, `KeepLandmark`, `ManorLandmark`, `EscalationView`, `EscalationResolveRequest`, `EscalationResolveResponse`, `EscalationResolution`.
  - `hamlet-core::tree`: Added tree accessors `manor_escalations_resolved()`, `manor_escalation_resolution()`, and `manor_escalation_notice()`.
  - `hamlet-daemon::local_api`: Implemented `/api/v1/landmarks`, `/api/v1/escalations`, `/api/v1/escalations/:id/resolve`, support for query token auth on `/api/v1/events/ws`, and WS event notification broadcasts for command executions.
  - `crates/hamlet-daemon/tests/local_api.rs`: Extended route matrix test and added dedicated test `test_landmarks_and_escalations_endpoints`.
- **MUD TUI Village Implementation (`crates/hamlet-tui`)**:
  - **Zero Orchestration & Pure Client Boundary**:
    - Discovers daemon exclusively through `daemon.endpoint` and `daemon.token` files. Never touches or mutates any other `.hamlet/` files.
    - All reads use local API projections (`/api/v1/landmarks`, `/api/v1/status`, `/api/v1/metrics`, `/api/v1/mail`, etc.) and the `/api/v1/events/ws` WebSocket stream.
    - All state mutations execute through typed API command endpoints (`/api/v1/agents/steer`, `/api/v1/mail/send`, `/api/v1/escalations/:id/resolve`).
  - **Normative Room Views (`src/ui/`)**:
    - `guild.rs`: Commissions table with state, indentures, and blueprint listings.
    - `tavern.rs`: Gossip log and hibernated patron stasis registry.
    - `courier.rs`: Unread mail counter, sent/received message tables, and message viewer.
    - `market.rs`: Shared skill inventory and package cache status.
    - `keep.rs`: Vault artifacts table with sha256 checksums, and active agent census table with live metrics/telemetry.
    - `manor.rs`: Mayor health, wake sequence log, pending decrees, and escalation notice overview.
  - **Navigation & Interaction (`src/app.rs`, `src/model.rs`)**:
    - Tab / Shift-Tab cycling and number keys `1`–`6` for instantaneous room switching.
    - Vim motion keys (`h`/`j`/`k`/`l`, `g`/`G`) for table scrolling and tab cycling.
    - Mouse capture: clickable room tabs, clickable landmark sections, and list selection.
    - MUD flavor descriptions dynamically generated from honest projection counts.
  - **Live WebSocket Event Ticker**:
    - Bottom event ticker streaming events (`mail_delivered`, `decree_executed`, `bundle_applied`, `agent_woken`, `escalation_filed`, `command_executed`).
    - Per-room event filtering toggleable via `f` key.
  - **Intervention Surfaces (Modals)**:
    - Steer dialog (`s`): compose and dispatch steer directives to active agents.
    - Decree review (`d`): inspect queued decrees, action payloads, and ack verdicts.
    - Escalation inbox & resolution (`e`): inspect pending escalations and resolve with approval/rejection.
    - Mail compose (`m`): compose and dispatch messages to agent parcels.
    - Landmark detail view (`Enter`): detailed inspection of selected list items.
    - Help dialog (`?`): modal showing key bindings and navigation guide.
  - **Resilient Reconnection**:
    - Exponential backoff (1s, 2s, 4s, 8s, 16s max) when connection drops.
    - Full village snapshot refresh upon reconnect, preventing stale or ghost state.
- **CLI Integration (`crates/hamlet-cli`)**:
  - Added `hamlet tui [--room <name>] [--demo]` subcommand delegating to `hamlet_tui::run(options)`.
  - Also available as standalone binary `hamlet-tui`.
- **View-Model Tests & Headless Frame Goldens (`tests/`)**:
  - `tests/view_model_test.rs`: 7 pure unit tests verifying room topology, WS filtering, ticker ring buffer, modal state machine, demo fixtures, backoff schedule, and interactive event loop dispatch.
  - `tests/render_golden_test.rs`: Headless smoke render tests capturing ratatui frames across all 6 rooms and all modal dialogs.
  - `src/demo.rs`: Realistic village fixture generator for offline demo mode and CI verification.

### Verify

```sh
cargo test -p hamlet-tui                               # 9 view-model and headless golden render tests
cargo test -p hamlet-cli                               # CLI test suite including TUI subcommand
cargo test -p hamlet-daemon --test local_api           # daemon local API & landmarks endpoints
./scripts/check-deps.sh                                # verify runtime allowlist
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Next Steps (Step 04)

- QA Engineer implements Step 04 (`04-qa-engineer.md`): Interface suite + scripted demo e2e (`scripts/e2e-village-demo.sh`), API boundary verification, CLI matrix, and keyboard operability tests.

## Step 04 — qa-engineer: Interface suite + scripted demo e2e

Status: **done**. Verdict: `GATE 08/04: PASS` (re-gated after remediation).

### Findings

#### QA08-01 (CRITICAL — Path Traversal in Vault Export Allows Arbitrary File Exfiltration Including `.hamlet/.env` Secrets)
- **File:Line**: `crates/hamlet-daemon/src/local_api.rs:764` and `crates/hamlet-daemon/src/local_api.rs:779`
- **Repro Command**:
  ```sh
  hamlet init /tmp/village
  hamlet daemon start --dir /tmp/village
  hamlet vault export ../../.env /tmp/stolen_env --dir /tmp/village
  cat /tmp/stolen_env # contains full .env secret keys!
  ```
  Also asserted in `crates/hamlet-cli/tests/adversarial_interface.rs::test_repro_qa08_01_vault_export_path_traversal`.
- **Expected vs Actual**:
  - *Expected*: Per SPEC §2 and §11.3, every artifact name or path received from clients or external input must be validated via `hamlet_core::sanitize::sanitize_name` and strictly confined within `keep/vault/<commission>/`. Any path traversal (`..`, `/`, `\`) or attempt to escape the vault tree must return HTTP 400 Bad Request with typed `ApiErrorResponse::bad_request` and CLI exit code 1.
  - *Actual*: `vault_export_handler` does `vault_root.join(comm.as_str()).join(&payload.artifact)` (line 764) or `vault_root.join(&payload.artifact)` (line 779) directly without sanitizing `payload.artifact` or asserting `source.canonicalize()?.starts_with(vault_root.canonicalize()?)`. Exporting `../../.env` or `../../charter.json` succeeds with HTTP 200 / CLI exit code 0, exfiltrating the entire `.hamlet/.env` secret key file or `charter.json`.
- **SPEC Citation**: SPEC §2 ("Name-safety: every directory/file name created from external or agent input must pass the path sanitizer (§11.3). Directory traversal is blocked at the API boundary, not by convention"), SPEC §4 ("Isolation invariants: ... cannot escape via path traversal in promoted filenames"), SPEC §11.3 ("Sanitizer: allowlist charset, length caps, no ., .., no path separators, no control chars"), SPEC §11.4 ("Secrets: .env (0600) holds model endpoints and API keys ... never echoed into logs, charter.json, archives, or test fixtures").
- **Remedy**: In `crates/hamlet-daemon/src/local_api.rs:753` (`vault_export_handler`), sanitize `payload.artifact` with `sanitize_name(&payload.artifact)` immediately upon request parsing, and reject path traversal with `StatusCode::BAD_REQUEST`.

#### QA08-02 (HIGH — Product Code Violation of Secrets Seam Ratchet: `census.rs:2021` Reads `.env` Directly)
- **File:Line**: `crates/hamlet-daemon/src/census.rs:2021`
- **Repro Command**:
  ```sh
  ./scripts/e2e-02-isolation.sh
  ```
- **Expected vs Actual**:
  - *Expected*: Per ADR-0020 cl. 6, ADR-0022 cl. 1, and SPEC §11.4, daemon `.env` reading is strictly confined to `crates/hamlet-daemon/src/dispatch.rs`'s single seam (`load_env_secrets`). Census and stasis operations must never open `env_file()` directly.
  - *Actual*: `census.rs:2021` performs `fs::File::open(self.tree.env_file())` directly when awakening a patron to pass `Secrets` to `sup.start_session(&parcel_ref, &brief, &secrets)`, violating the single-seam architecture rule and failing the `e2e-02-isolation.sh` ratchet (`FAIL: daemon touches .env outside the single seam: crates/hamlet-daemon/src/census.rs:2021`).
- **SPEC Citation**: SPEC §11.4, CONVENTIONS §2 ("Singular governor"), ADR-0020 cl. 6 ("Secrets resolution at send time"), ADR-0022 cl. 1 ("Single seam for .env hydration"), ratchet in `scripts/e2e-02-isolation.sh:56-66`.
- **Remedy**: Pass hydrated `Secrets` to `ProductionStasis` or delegate session startup through a seam that already holds `Secrets`, avoiding raw `env_file()` filesystem opens in `census.rs`.

### Committed Suite

1. **API Boundary**:
   - `crates/hamlet-cli/tests/adversarial_interface.rs::test_api_boundary_static_source_audit`: static grep over `hamlet-cli` and `hamlet-tui` crates verifying zero direct access to live tree landmark paths (`guild/`, `tavern/`, `courier/`, `market/`, `keep/`, `charter.json`, `sled`).
   - `crates/hamlet-cli/tests/adversarial_interface.rs::test_route_coverage_table`: table-driven verification that every mutating operator verb maps to an API command endpoint.
2. **CLI Matrix**:
   - `crates/hamlet-cli/tests/adversarial_interface.rs::test_cli_matrix_daemon_down_clean_errors_and_exit_code_3`: every client verb verified with clean error and exit code 3 when daemon is down (in human and strict `--json` formats).
   - `crates/hamlet-cli/tests/adversarial_interface.rs::test_cli_matrix_exit_codes_and_json_formatting`: exit codes 0 (success), 1 (domain error), 2 (usage error), 3 (daemon unreachable) verified in both human and JSON output modes.
3. **Auth & Token Lifecycle**:
   - `crates/hamlet-cli/tests/adversarial_interface.rs::test_auth_token_enforcement_and_mode_0600`: token file mode strictly 0600, missing token 401, forged token 401, bearer and custom `x-hamlet-local-token` authentication verified.
4. **TUI Honesty & Reconnect**:
   - `crates/hamlet-tui/tests/view_model_test.rs::test_ticker_state_and_event_mapping`: WS event stream FIFO order preserved.
   - `crates/hamlet-tui/tests/view_model_test.rs::test_reconnect_fresh_state_clears_ghosts`: reconnect refresh replaces state snapshot, eliminating ghost entities.
5. **e2e Demo Script**:
   - `scripts/e2e-village-demo.sh`: executable 4-tier e2e demo script (init → daemon start → TUI demo-mode frame capture → CLI verbs cycle → commission flow with fake agents → vault list shows artifact → vault export byte-identical deliverable → commission close → daemon stop clean unlinking).
6. **Keyboard Operability**:
   - `crates/hamlet-tui/tests/view_model_test.rs::test_keyboard_operability_complete_keymap`: table-driven verification that every room (1–6, Tab, BackTab, `[`, `]`) and modal (Steer `s`, Decree `d`, Escalation `e`, Mail `m`, Filter `f`, Refresh `r`, Help `?`, Quit `q`) is operable without mouse.
   - `crates/hamlet-tui/tests/render_golden_test.rs`: headless golden frames across all 6 rooms and 6 modals.

### Verify

```sh
cargo test -p hamlet-cli --test adversarial_interface
cargo test -p hamlet-tui --test view_model_test
cargo test -p hamlet-tui --test render_golden_test
./scripts/e2e-village-demo.sh
./scripts/e2e-commission.sh
./scripts/e2e-07-stasis.sh
./scripts/check-deps.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Remediation Verification & QA Re-gate

Status: **QA Verification Complete**. Verdict: `GATE 08/04: PASS`.

### Remediation Status of Prior Findings

1. **QA08-01 (Path Traversal in Vault Export)**: **RESOLVED & VERIFIED**.
   - Input artifact is sanitized with `sanitize_name` in `crates/hamlet-daemon/src/local_api.rs:762`.
   - Vault root confinement and symlink checks are strictly enforced.
   - Verified via `test_repro_qa08_01_vault_export_path_traversal` and `local_api.rs::test_vault_listing_and_export`. Traversal attempts now correctly return HTTP 400 Bad Request and CLI exit code 1.

2. **QA08-02 (Direct .env Read in census.rs)**: **RESOLVED & VERIFIED**.
   - Direct `self.tree.env_file()` open was removed, making `scripts/e2e-02-isolation.sh` pass.
   - Session startup fallback hydrates secrets via `crate::dispatch::load_secrets(&self.tree)` without direct file access.

3. **QA08-03 (Secret Unmasking Outside Allowed Seams in census.rs)**: **RESOLVED & VERIFIED**.
   - Added helper `pub fn load_secrets(tree: &HamletTree) -> Result<Secrets, DispatchPlaneError>` to `crates/hamlet-daemon/src/dispatch.rs` (an allowed unmasking seam per CONVENTIONS §6).
   - Removed `.expose()` from `crates/hamlet-daemon/src/census.rs`. `census.rs` now calls `crate::dispatch::load_secrets(&self.tree).unwrap_or_default()`.
   - Verified via `scripts/e2e-03-adversarial.sh` (ratchet T3 green: `.expose()` confined to `supervise.rs`, `dispatch.rs`, and `web.rs`).

### Re-gate Verification Results

| Test / Check | Result |
|---|---|
| `cargo fmt --all --check` | **PASS** |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** |
| `cargo test --workspace` | **PASS** |
| `scripts/check-deps.sh` | **PASS** |
| `scripts/e2e-village-demo.sh` | **PASS** |
| `scripts/e2e-commission.sh` | **PASS** |
| `scripts/e2e-07-stasis.sh` | **PASS** |
| `scripts/e2e-02-isolation.sh` | **PASS** |
| `scripts/e2e-03-adversarial.sh` | **PASS** |
| `scripts/ci-local.sh` | **PASS** |

### Next Steps (Step 05)

- Hand over to **CI Agent** for Step 05 (`05-ci-agent.md`) with precondition `GATE 08/04: PASS` verified.

## Step 05 — ci-agent: Sprint 08 verification battery & gate verdict (GATE 08/05: PASS)

Status: **done, gate PASS**. Precondition `GATE 08/04: PASS` verified. Strict role boundary maintained: **no product code was edited**.

### Verification Battery Results

| Check / Gate | Target / Scope | Result | Details |
|---|---|---|---|
| `cargo fmt --all --check` | Workspace-wide code formatting | **PASS** | Cleanly formatted across all workspace crates |
| `cargo clippy --workspace --all-targets -- -D warnings` | Workspace-wide static analysis | **PASS** | 0 warnings across all targets |
| `cargo test --workspace` | Full workspace unit & integration suites | **PASS** | All unit, integration, and golden tests green (100% pass) |
| `scripts/check-deps.sh` | Dependency graph & runtime allowlist | **PASS** | `clap`, `ratatui`, `crossterm` runtime promotions valid, 0 violations |
| Guarded Podman Suite (`scripts/guarded-entry.sh`) | 7 guarded podman suites (`HAMLET_PODMAN_TESTS=1`) | **PASS** | 14/14 tests green across `podman_guarded`, `bootstrap_guarded`, `provision_guarded`, `escape_guarded`, `freight_guarded`, `supervise_guarded`, `manor_guarded` |
| `scripts/e2e-01-safename-compile-fail.sh` | SafeName compile-time failure | **PASS** | T1 compile-fail boundary type-enforced |
| `scripts/e2e-01-topology-state.sh` | Topology & state invariant e2e | **PASS** | T1-T7 normative dirs, permissions (0600 .env), crash recovery green |
| `scripts/e2e-02-isolation.sh` | Subuid isolation e2e | **PASS** | T1-T3 isolation suite, source ratchets, and .env single seam green |
| `scripts/e2e-03-adversarial.sh` | Adversarial boundary harness | **PASS** | T1-T4 mock agent, tail-loss, and unmasking seam ratchets green |
| `scripts/e2e-04-comms.sh` | Inter-agent communications e2e | **PASS** | T1-T2 adversarial comms, mail parse, constant-time auth green |
| `scripts/e2e-05-mayor-adversarial.sh` | Mayor adversarial e2e harness | **PASS** | T1-T2 decree security, provenance, and wake queue deduplication green |
| `scripts/e2e-06-guard-mutations.sh` | Guard mutation testing e2e | **PASS** | All guard mutations killed |
| `scripts/e2e-07-ci-abort.sh` | CI abort & verdict backstop e2e | **PASS** | T1-T3 parse/source aborts and host lock concurrency guards green |
| `scripts/e2e-07-stasis.sh` | Stasis lifecycle flow & adversarial suite | **PASS** | T1-T3 CLI stasis flow, 26 adversarial tests, lineage and error ratchets green |
| `scripts/e2e-commission.sh` | Commission lifecycle flow & adversarial suite | **PASS** | T1-T3 CLI commission flow, 22 adversarial tests, git mediation ratchets green |
| `scripts/e2e-village-demo.sh` | Sprint 08 village demo & interface matrix | **PASS** | T1-T3 4-tier flow, TUI headless capture, CLI verbs, 18 interface/view tests green |
| `scripts/ci-local.sh` | Local full battery execution | **PASS** | 100% clean exit across all gates and e2e scripts |

### ADR-0014 (ADR-0041 / ADR-0042 / ADR-0043) Implementation Verification

Per the ADR renumbering index (`docs/adr/README.md`), Sprint 08 "ADR-0014" corresponds to **ADR-0041** (daemon local API), **ADR-0042** (CLI command tree and TUI architecture), and **ADR-0043** (TUI landmark rooms on SPEC §2 tree). Implementation fidelity was verified:

1. **ADR-0041 (Daemon Local API Contract)**:
   - Localhost HTTP+WS Axum server bound strictly to dynamic port `127.0.0.1:<port>`.
   - Discovery files `run/daemon.{endpoint,pid,token}` written with 0600 permissions; unlinked on clean shutdown.
   - Authentication enforced with constant-time equality over `Authorization: Bearer <token>` and `x-hamlet-local-token: <token>`.
   - Pure wire models and projections housed exclusively in `hamlet-core::api`.
   - Zero-orchestration projection routes (`/api/v1/status`, `/api/v1/census`, `/api/v1/commissions`, `/api/v1/decrees`, `/api/v1/vault`, `/api/v1/metrics`, `/api/v1/mail`, `/api/v1/landmarks`, `/api/v1/escalations`) and command endpoints (`commissions/new`, `commissions/close`, `agents/hibernate`, `agents/wake`, `agents/steer`, `mail/send`, `vault/export`, `escalations/:id/resolve`, `daemon/shutdown`) delegating directly to internal subsystems.
   - Live event WebSocket stream `/api/v1/events/ws` with query and header auth and command outcome broadcast.

2. **ADR-0042 (CLI Command Tree & Exit Code Contract)**:
   - Dependency promotions: `clap`, `ratatui`, `crossterm` verified within runtime allowlist.
   - Universal CLI surface matching SPEC §12.1: `init`, `daemon`, `commission`, `agent`, `decree`, `mail`, `vault`, `metrics`, `tui`.
   - Exit code contract: 0 = success, 1 = operational/domain error, 2 = usage error, 3 = daemon unreachable.
   - Strict `--json` machine-readable output across all commands with typed `{"error":..., "code":...}` errors.

3. **ADR-0043 (TUI Landmark Projections & Room Graph)**:
   - 1:1 mapping with SPEC §2 landmarks: Guild (`guild/`), Tavern (`tavern/`), Courier (`courier/`), Market (`market/`), Keep (`keep/`), Manor (`keep/census/manor/`).
   - Pure client over HTTP/WS with zero direct disk access to village state.
   - Keyboard operability (Tab, 1-6, h/j/k/l, ?, q, s, d, e, m, f, r) and mouse navigation verified.
   - Escalation inbox resolution and reconnect state refresh verified.

### Gate Verdict

`GATE 08/05: PASS`

### Next Steps (Step 06)

- Hand over to **Commit Agent** for Step 06 (`06-commit-agent.md`): `git diff main...HEAD` audit, squash-merge `sprint/08-cli-tui` → `main`, create tag `sprint/08-cli-tui`, run verification battery on merged main, and record merge commit SHA and tag.





