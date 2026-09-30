# hamlet — Containerized Multi-Agent Orchestration Engine — Specification v1.0 (LOCKED)

**Product:** A Rust host daemon + CLI that deploys long-horizon AI agent sessions
as mapped system users inside slim rootless-Podman containers, coordinating them
through a file-as-truth village topology (`.hamlet/`), wrapped — in later stages —
in a MUD-style TUI and finally an 8-bit pixel-RPG WASM frontend.

**Status:** LOCKED for handoff to the development team. Items marked **[DEV]** are
implementation-level details intentionally left to the dev team (resolved via ADR
inside the prompt that first needs them). `MANIFESTO.md` is the inspiration source;
this SPEC governs. Where they differ, SPEC.md wins.

---

## 1. Concept & pillars

hamlet is a **containerized sandbox and orchestration engine for long-horizon,
multi-agent lifecycles**. Each running agent session lives in its own mapped
system user inside a slim Linux container, with strategic host directories
mounted into a village-shaped dotfolder tree in the cwd.

Core design pillars:

- **File-as-truth**: all orchestration state is a declarative text/JSON file
  tree (`.hamlet/`). The daemon derives behavior from filesystem mutations,
  never from hidden in-process state. `charter.json` is the single serialized
  state map; everything else is human-readable markdown/JSON.
- **Singular governor**: agents never touch infrastructure sockets. The host
  daemon is the only process that talks to Podman, allocates UIDs, promotes
  files, and injects interrupts. Security lives out-of-band on the host.
- **Stateless executive**: the top-level orchestrator agent ("The Mayor") is a
  stateless event processor woken by context reconstitution — no ballooning
  chat thread, zero-token sleep between events (§7).
- **Permanent identity slots**: agents map to explicit, permanent UID slots
  (1001–2000) so permissions, ACLs, and mail spools stay coherent across
  reboots and hibernation cycles.
- **Dual-state convergence**: environment mutations (DEED.md) and intelligence
  mutations (PROPRIETOR.md) are tracked separately and promoted independently
  at commission end (§10).
- **Staged presentation**: daemon + CLI first (all programmatic function),
  then MUD-style TUI mock, then WASM 8-bit RPG frontend. No stage may lag
  another stage's functionality — every presentation layer renders the same
  file/WS truth.

---

## 2. Normative directory topology (`.hamlet/`)

The tree below is **normative**. Paths are contract; code must reference them
through a single typed layout module (never string-concatenated ad hoc).

```
.hamlet/
├── guild/
│   ├── register/              # Immutable agent descriptions + allowed scopes
│   ├── blueprints/            # Immutable environment declarations (Blueprint files)
│   └── ledger/                # All tracked goals
│       └── commission<n>/     # Individual project ("world tracking slice")
│           ├── git/           # [Mount: /var/git/] Central bare git repository
│           ├── COMMISSION.md  # Original prompt instructions (player-authored)
│           └── INDENTURE.md   # Mayor-decomposed per-agent bounded contracts
├── tavern/
│   ├── gossip/                # [Mount: /var/context/] Read-only source user input
│   └── patrons/               # Hibernated (unassigned) agent instances
│       └── PROPRIETOR<name>/
│           ├── home.uid.bak/  # Backed-up agent user home folder
│           └── PROPRIETOR.md  # Backed-up mutated agent definition
├── courier/
│   ├── mail/                  # [Mount: /var/mail/] Linux mail spools per UID
│   ├── freight/               # [Mount: /var/freight/] Shared folder, POSIX ACL sandboxes
│   └── dispatch/              # External webhook outbox/ inbox/ JSON files
├── market/
│   ├── skills/                # SKILL.md-defined functional capabilities
│   └── pkgcache/              # [Mount: /var/pkgcache/] Host-cached package mirrors
├── keep/
│   ├── vault/                 # Programmatically promoted final deliverables
│   ├── archives/              # Execution telemetry, log traces, cost metrics
│   ├── census/                # Active working subtask directories
│   │   ├── parcel<uid>/       # Individual agent lease environment
│   │   │   ├── home/          # [Mount: /home/<uid>/] Persistent user home
│   │   │   ├── TITLE.md       # Chain-of-title: lineage of triggers/agents
│   │   │   ├── DEED.md        # Mutable environment record (### Dynamic Alterations)
│   │   │   └── PROPRIETOR.md  # Mutable agent record (behavior/memory changes)
│   │   └── manor/
│   │       ├── home/          # [Mount: /home/1000/] Orchestrator user home
│   │       └── MAYOR.md       # Trunk system behavior prompt
├── .env                       # Global environment variables and secrets (0600)
├── charter.json               # Master state map (atomic writes, sorted keys)
└── config.json                # Orchestrator/engine settings
```

**Name-safety**: every directory/file name created from external or agent input
must pass the path sanitizer (§11.3). Directory traversal is blocked at the API
boundary, not by convention.

---

## 3. Host daemon architecture (Rust)

Workspace crates (names normative; internals **[DEV]**):

| Crate | Responsibility |
|---|---|
| `hamlet-core` | Pure logic: tree layout types, path sanitizer, charter state model, UID slot allocation, blueprint schema, decree vocabulary, promotion parsing. No I/O side effects beyond injected traits. |
| `hamlet-daemon` | The governor: tokio runtime, podman exec layer, filesystem watchers (notify), mail routing, freight ACLs, Mayor loop, agent session supervision, sled telemetry, axum web/WS server, webhook dispatch. |
| `hamlet-cli` | `hamlet` binary: init, commission management, agent/commission status, decree inspection, vault listing, daemon control. |
| `hamlet-tui` | MUD-style terminal frontend over the daemon's local API/WS (stage 2). |
| `hamlet-web` | WASM 8-bit RPG frontend assets + Rust/JS glue served by axum (stage 3). |

Dependency stack (locked by MANIFESTO, versions **[DEV]** within these majors):

- **tokio 1.x** (`rt-multi-thread`, `macros`, `process`, `fs`) — runtime core;
  non-blocking pipes to `podman exec`; non-blocking I/O on the `.hamlet` tree.
- **axum 0.7.x** — host web server: RPG frontend asset layers + webhook endpoints.
- **tokio-tungstenite 0.21.x** — WebSockets: real-time canvas metrics, chat bubbles.
- **serde 1.0 / serde_json 1.0** (`derive`) — config/state marshalling; parses
  agent JSON-RPC streams; writes `charter.json`.
- **sled 0.34** — embedded KV store for high-velocity telemetry (token counts,
  latency spikes, cost, trace events). Telemetry never lives in charter.json.
- **dotenvy 0.15** — `.env` hydration (model endpoints, API keys) out-of-band.
- **regex 1.x** — path/directive sanitizer engine.
- **reqwest 0.11/0.12** (`json`, `rustls-tls`) — async HTTP client for dispatch.
- **notify 6.x** — inotify-backed filesystem watcher driving mail delivery,
  DEED.md monitoring, decree detection.

**Layering rule**: `hamlet-daemon` is the only crate that spawns processes or
mutates the tree at runtime. `hamlet-core` is pure and fully unit-testable
without podman, agents, or a filesystem.

---

## 4. Container & security model

- **Runtime**: rootless Podman only. The daemon shells out to `podman` via
  tokio process pipes — no libpod bindings, no Docker, no privileged mode.
- **UID policy**: UID 1000 is reserved for the orchestrator (manor). Subagent
  slots are 1001–2000, allocated **sequentially and permanently** per agent
  identity in `charter.json`; slots are never reused for a different identity
  while a hibernated profile references them. Slot exhaustion is a hard error
  surfaced to the operator, not a silent wrap.
- **Base image**: slim Alpine + uutils. Heavy system managers are excluded by
  design; user-space runtime multiplexers (**Pixi, NVM, Cargo**) are bundled so
  agents can self-install version-locked toolchains inside `/home/<uid>/`.
- **Mounts** (normative; every agent container receives exactly these):

| Host path | Container path | Mode |
|---|---|---|
| `keep/census/parcel<uid>/home/` | `/home/<uid>/` | rw |
| `courier/mail/` | `/var/mail/` | rw (spool per UID; cross-UID read blocked by spool perms) |
| `courier/freight/` | `/var/freight/` | rw (POSIX ACL sandboxes inside) |
| `guild/ledger/commission<n>/git/` | `/var/git/` | **ro** |
| `market/pkgcache/` | `/var/pkgcache/` | rw (cache semantics) |
| `tavern/gossip/` (+ reference parent) | `/var/context/` | **ro** |
| `keep/census/manor/home/` | `/home/1000/` | rw (Mayor container only) |

- **Isolation invariants** (QA-enforced): agents cannot write `/var/git/` or
  `/var/context/`; cannot read another UID's home or mail spool; cannot reach
  the podman socket or any host infrastructure socket; cannot escape via path
  traversal in promoted filenames.
- **Freight ACLs**: agents secure shared artifacts by creating directories in
  `/var/freight/` wrapped in POSIX ACLs (`setfacl`) tailored to the recipient
  UID. The daemon provides/permits the ACL tooling inside the container but
  never relaxes ACLs from the host on an agent's behalf.

---

## 5. Blueprints & skills

- **Blueprints** (`guild/blueprints/`): immutable environment declarations
  listing tools + versions to bootstrap a workspace (e.g. pixi packages, node
  versions, cargo toolchains, apt/apk additions). Format: TOML, schema
  **[DEV]**, versioned; parsed by `hamlet-core`, executed by the daemon at
  parcel provisioning.
- **Skills** (`market/skills/`): `SKILL.md`-rule directories injected into or
  callable by agents for task-specific use cases. Discovery is by convention
  (directory name = skill name); injection mechanism **[DEV]**.
- **Agent briefs** (`guild/register/`): immutable core-principles documents per
  agent archetype. The register entry + blueprint + skills fully define a
  deployable agent; runtime mutation is only ever recorded in the parcel's
  PROPRIETOR.md / DEED.md (§10), never by editing the register.
- **Blueprint evolution**: at commission end, DEED.md `### Dynamic Alterations`
  blocks are parsed and written back into the parent blueprint (§9.4). This is
  the only mutation path for blueprints, and it is daemon-performed.

---

## 6. Agent engine (session model)

- Agents run as **pi coding-agent sessions** inside their container, driven by
  the daemon through `podman exec` with piped stdin/stdout (JSON-RPC stream).
- **Delivery**: mail messages are delivered **sequentially to the agent's
  stdin**, one task boundary at a time — never interleaved mid-message.
- **The Divine Interruption**: for high-priority system events or manual human
  intervention, the daemon bypasses the queue and injects an out-of-band
  JSON-RPC `interrupt_and_steer` packet directly down the active pipe. Wire
  schema **[DEV]** (must be distinguishable from queued mail by the session).
- **Supervision**: the daemon owns process lifecycle — spawn, health, restart
  policy, graceful stop, hard kill on hibernation. Crash telemetry goes to
  sled; crash lineage goes to TITLE.md.
- **Telemetry**: the daemon parses the JSON-RPC stream for token counts, cost,
  latency, and trace events → sled (§11.2). Agents do not self-report metrics.

---

## 7. The Mayor (executive control loop)

- The Mayor is the Trunk Agent under static identity lock **UID 1000**
  (`keep/census/manor/`), prompted by `MAYOR.md`.
- **Stateless event processor**: the Mayor process sleeps at zero token cost.
  On a wake event the daemon builds a **pristine single-turn context packet**:
  `MAYOR.md` + current `charter.json` state grid + the targeted event artifact
  (fresh git diff, inbound mail, decree confirmation request, COMMISSION.md on
  new project). The Mayor evaluates, emits **decrees via filesystem changes**,
  and returns to sleep. No continuous chat thread is ever maintained.
- **Decree vocabulary** (normative set; encodings **[DEV]**): provision agent,
  decompose commission → INDENTURE.md entries, approve/reject deliverable
  promotion, hibernate agent, reassign task, escalate to human.
- **Validation loop**: deliverables land in `/var/freight/`; the Mayor
  evaluates against commission goals in a single-turn validation module; on
  approval the daemon catches the confirmation signal, sanitizes names, copies
  the artifact to `keep/vault/`, and spins down the session.
- The Mayor never executes podman or host commands itself — it only writes
  files; the daemon interprets.

---

## 8. Communication planes

1. **Async signalling (mail)**: file-backed text spool at `/var/mail/`
   (host: `courier/mail/`). The daemon watches spool additions via notify and
   routes messages to recipient stdin sequentially (§6). Mail format: plain
   text with a minimal header (From/To/Subject) **[DEV]**.
2. **Divine interruption**: out-of-band JSON-RPC injection (§6).
3. **Ad-hoc data interchange (freight)**: large binaries/dataframes/code move
   through `/var/freight/` with per-directory POSIX ACL sandboxes (§4).
4. **External webhook dispatch** (`courier/dispatch/`): JSON files dropped in
   `outbox/` are POSTed by the daemon via reqwest; responses land in
   `inbox/`. Endpoint auth via `.env`. Retry/backoff policy **[DEV]**.
5. **inbound webhooks**: axum endpoints accept external events, write them as
   dispatch inbox files / mail, and wake the Mayor when they match decree-
   relevant triggers.

---

## 9. Commission lifecycle (guild/ledger)

1. **Creation**: operator (CLI now, RPG UI later) creates `commission<n>/`
   with `COMMISSION.md` (baseline prompts) and an initialized **bare git
   repo** at `git/`.
2. **Decomposition**: on wake, the Mayor reads COMMISSION.md, breaks down
   multi-step goals, and writes bounded per-agent contracts to `INDENTURE.md`.
3. **Execution**: subagents scan their INDENTURE.md entries, build local
   checklists, spawn git **work branches** in `/var/git/` (ro mount — pushes
   go through the daemon-mediated git path **[DEV]**), commit work.
4. **Convergence**: on completion the daemon parses DEED.md
   `### Dynamic Alterations` blocks → updates parent blueprints in
   `guild/blueprints/`; deliverables follow the §7 validation loop into
   `keep/vault/`.
5. **Multi-tenancy**: each commission is an independent world slice. The
   charter tracks per-commission state; parcels record which commission they
   serve in TITLE.md.

---

## 10. Census, hibernation & dual-state convergence

- **parcel<uid>/** is the structural tenant lease: `home/` (persistent),
  `TITLE.md` (chronological chain-of-title — which upstream agent or player
  prompt triggered this workspace; auditable lineage of every mutation),
  `DEED.md` (environment layer), `PROPRIETOR.md` (intelligence layer).
- **Hibernation routine** (on subtask finish or resource pressure): daemon
  stops the container process, copies `home/` → `tavern/patrons/PROPRIETOR<name>/home.uid.bak/`,
  serializes the mutated agent profile into that folder's `PROPRIETOR.md`,
  frees the compute (the UID slot mapping persists in charter.json).
- **Quick awakening**: for a matching skill configuration, the daemon reads
  the patron's `PROPRIETOR.md`, provisions the next available UID, skips clean
  template re-downloads, drops `home.uid.bak/` into the new
  `parcel<uid>/home/`, and starts the session.
- **Dual-state convergence** at commission end (two independent promotions):
  - DEED.md environment states → `guild/blueprints/` (the land lease).
  - PROPRIETOR.md intelligence profile → `tavern/patrons/` (the person).
- **Separation rule**: tool installs log to DEED.md; prompt/behavior/memory
  changes log to PROPRIETOR.md. Agents are prompted (via register brief) to
  keep these distinct; the daemon parses them independently.

---

## 11. State, telemetry & secrets

### 11.1 System state map
- In-memory `BTreeMap` serialized **atomically** (temp file + rename) to
  `.hamlet/charter.json` (root-level: system-global, not a keep artifact).
  Sorted keys,
  canonical JSON. Human-readable at all times; no binary state.
- Charter contents (normative groups): UID slot table, agent registry refs,
  commission states, parcel leases, decree queue status, webhook config refs.
  Exact schema **[DEV]** (versioned field `charter_version`).

### 11.2 High-velocity metrics flow
- sled (embedded in the daemon) captures token tallies, API costs, latency
  spikes, system event traces — out-of-band, never in charter.json.
- Exportable summaries land in `keep/archives/` on commission close.

### 11.3 Sanitizer
- Every name derived from agent/Mayor/webhook input passes the regex sanitizer
  before touching the filesystem: allowlist charset, length caps, no `.`,
  `..`, no path separators, no control chars. Promotion copies are
  daemon-performed only.

### 11.4 Secrets
- `.env` (0600) holds model endpoints and API keys, hydrated via dotenvy into
  the daemon and injected into agent sessions out-of-band — never written into
  charter.json, archives, or logs.

---

## 12. Interfaces (staged presentation)

### 12.1 CLI (stage 1 — with daemon)
Normative command surface (flags **[DEV]**):
`hamlet init`, `hamlet daemon {start|stop|status}`, `hamlet commission
{new|list|show|close}`, `hamlet agent {list|hibernate|wake|steer}`,
`hamlet decree {list|show}`, `hamlet vault {list|export}`, `hamlet metrics
{show|export}`, `hamlet mail {send|read}`.

### 12.2 MUD-style TUI (stage 2)
Terminal frontend mocking the village layout: rooms = tree landmarks
(guild/tavern/courier/market/keep), agent sprites as text avatars, mail and
decree streams, commission board. Reads the same daemon API — no new state.

### 12.3 WASM 8-bit RPG frontend (stage 3)
- axum serves asset layers; WebSocket streams real-time canvas metrics and
  player chat bubbles over-the-head.
- Interactable sprites map 1:1 to the village landmarks of §2; the pixel UI
  manages work (commissions, decrees, vault) through daemon endpoints only.
- The frontend renders exclusively from daemon-pushed state — it never
  mutates `.hamlet/` directly.

---

## 13. Delivery stages & framework impact

| Stage | Delivers | Sprints |
|---|---|---|
| 1 | Daemon + CLI with all programmatic functionality (§2–§11) | 00–08 |
| 2 | MUD-style TUI mock (§12.2) | 08 |
| 3 | WASM 8-bit RPG frontend (§12.3) | 09 |
| — | Audit, soak, docs, release | 10 |

- All orchestration logic is file-driven; every daemon behavior must be
  observable from the tree alone (restart the daemon on a foreign host with
  the same `.hamlet/` → identical resumed behavior, modulo live processes).
- Portability: Linux host with rootless podman is the only supported target
  for v1. macOS/Windows hosts are out of scope.

---

## 14. Deferred & open (handed off)

| Item | Owner |
|---|---|
| Blueprint TOML schema; skill injection mechanism | **[DEV]** (Sprint 02) |
| Charter JSON schema + `charter_version` migration policy | **[DEV]** (Sprint 01) |
| Mail header format; delivery ordering guarantees beyond per-recipient FIFO | **[DEV]** (Sprint 04) |
| `interrupt_and_steer` wire schema | **[DEV]** (Sprint 03) |
| Decree file encodings (names, atomicity, acknowledgment) | **[DEV]** (Sprint 05) |
| Git push mediation for ro-mounted `/var/git/` | **[DEV]** (Sprint 06) |
| Dispatch retry/backoff policy; webhook auth scheme | **[DEV]** (Sprint 04) |
| Podman exec supervision details (restart limits, OOM policy) | **[DEV]** (Sprint 02/03) |
| WS protocol schema for canvas metrics/chat | **[DEV]** (Sprint 09) |
| Resource bounds policy (parcel RAM/CPU limits, hibernation pressure triggers) | **[DEV]** (Sprint 07) |

---

*Working history: `DESIGN.md` (design derivation log; superseded by this spec).
Inspiration source: `MANIFESTO.md` (non-normative).*
