# Hamlet

*Autonomous, unprivileged multi-agent software engineering village for Linux.*

Hamlet is an orchestration system and runtime harness that coordinates autonomous AI coding agents within isolated, rootless Podman containers ("parcels") on a Linux workstation. Guided by an elected Mayor agent and supervised by a singular daemon governor, agents collaborate to deliver software commissions while maintaining strict security isolation, predictable resource bounds, and atomic file-driven workflows.

---

## Architectural Principles

Hamlet is built around five core architectural invariants:

1. **Singular Governor**  
   Only the `hamlet-daemon` process mutates village state, executes container operations, routes communications, and authorizes promotions. Agents and external interfaces are unprivileged and cannot directly alter the governance or sandbox boundary.
2. **File-as-Truth**  
   The village filesystem tree (`.hamlet/` or operator-specified directory) is the single source of truth. All agent states, charters, mail spools, freight work areas, and decrees are persisted on disk as human-readable, canonical files.
3. **Zero-Orchestration Interfaces**  
   The Command-Line Interface (`hamlet-cli`), Terminal UI (`hamlet-tui`), and Web UI (`hamlet-web`) maintain no private state machines or orchestration loops. They operate strictly as reactive, read-mostly projections over the daemon's local HTTP and WebSocket APIs.
4. **Rootless Subuid Isolation**  
   Every agent runs in a dedicated non-root user namespace (`subuid`/`subgid`) with its capabilities dropped (`--cap-drop=ALL`), a read-only container root, and an isolated private home directory (`0700`). Host-privilege escalation and cross-agent filesystem tampering are structurally prevented by the Linux kernel.
5. **Pure Protocol & Local Channels**  
   Agents collaborate through local, filesystem-mediated communication primitives:
   - **Mail**: Asynchronous point-to-point and broadcast message passing via spool directories (`keep/census/parcel<uid>/mail/`).
   - **Freight**: Shared temporary workspaces governed by POSIX access control lists (`setfacl`) and actively audited by the daemon.
   - **Decrees**: Mayor instructions written as atomic JSON files (`keep/census/manor/home/decrees/`) and interpreted exclusively by the daemon governor.

---

## Village Tree Anatomy

When initialized, a Hamlet village forms a normative directory hierarchy:

```text
.hamlet/
├── hamlet.toml             # Village configuration (ports, timeouts, resource caps)
├── guild/                  # Blueprints, pinsets, and role definitions
│   ├── blueprints/         # Immutable container toolchain definitions (pixi, cargo, nvm, apk)
│   ├── commissions/        # Commission definitions and status records
│   ├── pins/               # Exact sha256 package and binary digest manifests
│   └── roles/              # Role descriptions and capability constraints
├── keep/                   # Live state, census, and telemetry
│   ├── census/             # Agent parcel homes, spool dirs, and Mayor manor
│   │   ├── manor/          # Mayor workspace, decrees, context packets, escalations
│   │   └── parcel<uid>/    # Per-agent container homes, mail spools, and work areas
│   ├── repo/               # Central Git mediation repository
│   └── telemetry.sled/     # Sled embedded database for token, cost, and latency traces
├── market/                 # Shared skills and external tool declarations
│   └── skills/             # Modular agent skill definitions
└── vault/                  # Encrypted/restricted credentials injected into parcels by daemon
```

---

## Quickstart

### Prerequisites

- **Operating System**: Modern Linux distribution (Kernel 5.10+, cgroup v2, user namespaces enabled).
- **Container Engine**: [Podman](https://podman.io/) (rootless configured with `/etc/subuid` and `/etc/subgid`).
- **Development Toolchain**: Rust 1.79+ (stable), `cargo`, and [`wasm-pack`](https://rustwasm.github.io/wasm-pack/) (for Web UI compilation).

### 1. Initialize a Village

Create a new Hamlet village directory tree:

```bash
# Initialize a village in the current directory (or specify a target directory)
hamlet init .

# Initialize with a specific environment blueprint
hamlet init my-village --blueprint village-alpine
```

### 2. Start the Daemon Governor

Start the background daemon to manage container lifecycles, mail routing, and telemetry extraction:

```bash
# Start daemon in background or run in foreground
hamlet daemon start

# Check daemon health and census status
hamlet daemon status
```

### 3. Launch the Terminal Operator UI (TUI)

Monitor active agents, system telemetry, and village landmarks in your terminal:

```bash
hamlet tui
```

The TUI provides interactive navigation between village landmarks:
- **`TownSquare`**: Village status, agent census, and active commissions.
- **`Manor`**: Mayor activity, issued decrees, and escalation queue.
- **`Workshop`**: Per-agent parcel states, live session output, and tool invocations.
- **`Vault`**: Monitored secrets and credential status.
- **`Dispatch`**: Mail routing activity and external webhook deliveries.

### 4. Open the Interactive Web Dashboard

Launch the WebAssembly-powered visual dashboard:

```bash
# Build the Web UI assets if not already compiled
./scripts/build-web.sh

# Start the web interface server and open in browser
hamlet web --open
```

The Web UI provides a spatial 2D view of the village, real-time agent status indicators, telemetry charts, and interactive decree tracking.

---

## Workspace Architecture

Hamlet is structured as a Cargo workspace with distinct purity boundaries:

| Crate | Purpose | Dependencies / Purity |
|---|---|---|
| [`hamlet-core`](crates/hamlet-core) | Normative schemas, tree layout accessors, SafeName sanitizers, decree grammar, and charter state machines. | Pure, synchronous logic. Zero network, zero subprocess, zero IO side-effects. |
| [`hamlet-daemon`](crates/hamlet-daemon) | Singular governor daemon. Podman container supervisor, sled telemetry sink, mail/freight router, webhook engine, and HTTP/WS local API. | Tokio async runtime, Axum, Podman CLI runner, Sled, Notify. |
| [`hamlet-cli`](crates/hamlet-cli) | Operator command-line interface. Commands: `init`, `daemon`, `agent`, `commission`, `tui`, `web`. | Clap, Tokio, Reqwest. Stateless client. |
| [`hamlet-tui`](crates/hamlet-tui) | Terminal user interface for operators, featuring landmark-based spatial navigation and streaming updates. | Ratatui, Crossterm, WebSocket client. |
| [`hamlet-web`](crates/hamlet-web) | Operator web dashboard compiled to WebAssembly. Features interactive 2D canvas, event inspection, and live telemetry. | Leptos (CSR), Web-Sys, Canvas 2D, WebSocket. |

---

## Verification & Testing

Hamlet is verified by a strict test battery enforcing behavioral immutability and adversarial safety:

- **Local CI Pipeline**:
  ```bash
  ./scripts/ci-local.sh
  ```
  Runs formatting, clippy with all warnings denied, workspace unit/integration tests, dependency audits, disk leak policy checks, the guarded rootless Podman test battery, and all end-to-end adversarial scenarios.

- **Adversarial & E2E Test Suite**:
  Individual test harnesses located in `scripts/`:
  - `scripts/e2e-01-topology-state.sh`: Village tree layout, permissions, and charter state verification.
  - `scripts/e2e-02-isolation.sh`: Rootless container isolation, subuid sandboxing, and escape defense.
  - `scripts/e2e-03-adversarial.sh`: Cross-UID boundary protection, freight tampering, and malicious payloads.
  - `scripts/e2e-04-comms.sh`: Mail delivery, spool integrity, and watcher notifications.
  - `scripts/e2e-05-mayor-adversarial.sh`: Mayor decree replay defense, ACK handling, and crash recovery.
  - `scripts/e2e-07-stasis.sh`: Hibernation, agent stasis snapshotting, and restoration.
  - `scripts/e2e-village-demo.sh`: Full multi-agent software engineering commission lifecycle.

- **Soak Testing**:
  ```bash
  ./scripts/soak-village.sh
  ```
  Executes multiple commission lifecycles under continuous churn to verify long-term stability and zero resource leaks.

---

## Documentation

- **[Specification (`SPEC.md`)](SPEC.md)**: Normative specification covering village layout, security model, and protocol encodings.
- **[Architecture Decision Records (`docs/adr/`)](docs/adr/)**: 44 accepted ADRs documenting architectural decisions and rationale.
- **[Developer Conventions (`roadmap/CONVENTIONS.md`)](roadmap/CONVENTIONS.md)**: Coding standards, purity rules, and engineering practices.
- **[Documentation Index (`docs/README.md`)](docs/README.md)**: Directory overview and guide to repository documentation.
- **[Backlog & Future Work (`docs/BACKLOG.md`)](docs/BACKLOG.md)**: Tracked future improvements and post-1.0 roadmap items.
