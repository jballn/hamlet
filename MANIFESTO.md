# hamlet - containerized sandbox and orchestration engine for long-horizon, multi-agent lifecycles wrapped in a pixel RPG

### Overview

The core conceit of the framework is deploying each running agent session within its own mapped system user in a slim Linux container, with strategic directories mounted to a dotfolder within the cwd:
  - /home/[uid]/ - UID 1000 is reserved for the orchestrator, additional UID for each deployed agent workspace
  - /var/mail/ - inter-agent communication is handled with the builtin mail spool
  - /var/freight/ - public file share
  - /var/git/ - central project repo, ro for agent users
  - /var/pkgcache/ - shared tool download cache
  - /var/context/ - ro mount of cwd dotfolder parent for reference

Workspace environments are codified in blueprints that define the tools and versions to bootstrap, agents are briefs on core principles and methodologies, and SKILLS are injected or called for task-specific use cases.

The daemon will be built in Rust:

[ APPLICATION LAYER ]
   ├── tokio (v1.x) ───────────────────► Master Multi-threaded Async Runtime Core
   │     ├── features: ["rt-multi-thread"] -> Spawns background worker pools per active Town
   │     ├── features: ["macros"]         -> Enables async #[tokio::main] lifecycle loops
   │     ├── features: ["process"]        -> Non-blocking stdin/stdout pipes to podman exec
   │     └── features: ["fs"]             -> Non-blocking I/O operations on the .hamlet tree
   │
   ├── axum (v0.7.x) ──────────────────► Lightweight Host Web Server
   │     └── Purpose: Serves the 8-bit RPG frontend asset layers & exposes webhook endpoints
   │
   └── tokio-tungstenite (v0.21.x) ────► High-Velocity WebSockets
         └── Purpose: Streams real-time canvas metrics & player chat bubbles over-the-head

 [ DATA & STATE PLANE ]
   ├── serde (v1.0.x) ─────────────────► Serialization / Deserialization Framework
   │     └── features: ["derive"]         -> Maps Rust structures smoothly to text configuration files
   │
   ├── serde_json (v1.0.x) ────────────► JSON Data Marshalling
   │     └── Purpose: Parses incoming pi_agent_rust JSON-RPC streams & writes keep/charter.json
   │
   ├── sled (v0.34.x) ─────────────────► Embedded High-Performance Key-Value Store
   │     └── Purpose: Stores real-time token counts, latency spikes, and system log traces
   │
   ├── dotenvy (v0.15.x) ──────────────► Host Environment Variable Injection
   │     └── Purpose: Hydrates model endpoints and secure secret API keys out-of-band from .env
   │
   └── regex (v1.x) ───────────────────► Path and Directive Sanitizer Engine
         └── Purpose: Enforces name-safety and blocks directory-traversal during file promotions

 [ NETWORKING & CARRIER FLOW ]
   ├── reqwest (v0.11.x / v0.12.x) ────► Async HTTP Client Matrix
   │     ├── features: ["json"]           -> Handles programmatic structured network requests
   │     └── features: ["rustls-tls"]     -> Avoids host-level OpenSSL library mismatches completely
   │
   └── notify (v6.x) ──────────────────► Local Kernel Filesystem Event Watcher
         └── Purpose: Hooks into Linux inotify to instantly trigger mail delivery routines

The orchestrator interfaces with the container runtime via rootless Podman execution layers, managing all process lifecycles, user namespace UIDs, and filesystem security isolation out-of-band on the host. Rather than allowing agents direct access to infrastructure sockets, the orchestrator acts as the singular, all-powerful system governor. It monitors directory mutations natively on the host filesystem to route workflows, parse runtime telemetry, and dynamically drive the progression of the multi-agent colony.

Presentation for the final product will be an 8-bit RPG village with interactable sprites and a pixel UI for managing work, but this will be developed at a later stage.  Initially we want a daemon and CLI that handle all programatic functionality, then a MUD-style TUI to mock the layout up, before finally working on a WASM frontend with full 8-bit pixel graphics

### Core Structural Topography (.hamlet/)

The framework relies on a declarative, text-based file tree layout that maps the unprivileged container topology straight to intuitive 8-bit RPG village landmarks. The layout is organized as follows:

.hamlet/
├── guild/
│   └── register/              # Immutable agent descriptions and allowed scopes
│   └── blueprints/            # Immutable environment declarations
│   └── ledger/                # All tracked goals
│   │   └── commission[n]/     # Individual projects
│   │   |   └── git/           # [Mount: /var/git/] Central Bare Git Repository
│   │   |   └── COMMISSION.md  # Original prompt instructions
│   │   |   └── INDENTURE.md   # Decomposed per-agent instructions
├── tavern/
│   └── gossip/                # [Mount: /var/context/] Read-Only source user input folder
│   └── patrons/               # Unassigned agent instances
│   │   └── PROPRIETOR[name]/  # Per agent directories
│   │   |   └── /home/uid.bak/ # Backed up agent user home folder
│   │   |   └── PROPRIETOR.md  # Backed up mutated agent definition
├── courier/
│   ├── mail/                  # [Mount: /var/mail/] Multi-user text Linux mail spools
│   ├── freight/               # [Mount: /var/freight/] Shared folder with POSIX ACL sandboxes
│   └── dispatch/              # External webhook connection outbox/inbox JSON files
├── market/
│   ├── skills/                # Functional capabilities defined via SKILL.md rules
│   └── pkgcache/              # [Mount: /var/pkgcache/] Host-cached package mirrors (Pixi/Pip/Go/Node)
├── keep/
│   ├── vault/                 # Programmatically promoted, out-of-band final deliverables
│   ├── archives/              # Transactional execution telemetry, log traces, and cost metrics
│   ├── census/                # Active working subtask directories
│   │   └── parcel[uid]/       # Individual agent lease environments
│   │   |   ├── home/          # [Mount: /home/uid/] Persistent user home directories
│   │   |   ├── TITLE.md       # Historical chain-of-title logs for execution lineage
│   │   |   └── DEED.md        # Instanced, mutable environment record tracking runtime alterations
│   │   |   └── PROPRIETOR.md  # Instanced, mutable agent record tracking runtime alterations
│   │   └── manor/
│   │   |   ├── home/          # [Mount: /home/1000/] Persistent orchestrator user directory
│   |   |   ├── MAYOR.md       # Top-level Trunk system behavior prompt
└── .env                       # Global environment variables and secrets
└── charter.json               # Master in-memory BTreeMap state serialization map
└── config.json                # Orchestrator and engine settings

### Lifecycle and Execution Planes

The orchestrator shifts runtime complexities entirely into a file-driven state model to achieve extreme performance, security, and portability.

1. The Executive Control Loop (The Mayor)

The top-level orchestration layer is handled by the Trunk Agent (The Mayor) running under a static identity lock of UID 1000. The Mayor is a Stateless Event Processor driven by context reconstitution. Instead of maintaining a continuous chat thread that balloons token consumption, the Mayor process sleeps.

When an event triggers a wake-up, the host orchestrator dynamically builds a pristine, single-turn context packet compiling keep/MAYOR.md, the current keep/charter.json state grid, and the targeted event file (such as a fresh git diff or inbound mail). The Mayor runs its evaluation, fires off structural decrees via filesystem changes, and instantly returns to a zero-token sleep state.

2. The Sandbox Environment (Dynamic Subagents)
Dynamic subagents are provisioned on demand in response to the Mayor's decrees. The orchestrator maps them sequentially to explicit, permanent Subtask UID Slots (UID 1001-2000) inside keep/charter.json, ensuring absolute permission cohesion across reboots.

To maximize agent agency, the container baseline skips heavy, rigid system managers in favor of a slim runtime environment (Alpine with uutils) bundled with user-space runtime multiplexers (Pixi, NVM, and Cargo). Subagents have full freedom to dynamically install, alter, and configure their own version-locked tool chains directly inside their /home/uid/ directories.

3. Communication and File Coordination

- Asynchronous Signalling: Inter-agent communication uses the container's native, file-backed text mail spool (/var/mail/). The host-side orchestrator uses high-performance, non-blocking asynchronous event loops to monitor spool additions. Messages are delivered sequentially to each agent's standard input stream (stdin), preserving clear task boundaries.

- The Divine Interruption: For high-priority system events or manual human intervention, the orchestrator can bypass the file queue, injecting an immediate out-of-band JSON-RPC interrupt_and_steer packet directly down the active process pipe to steer behavior mid-thought.

- Ad-Hoc Data Interchanges: Massive binary files, dataframes, and collaborative code avoid the mail system, moving instead through the public Bazaar (/var/freight/). Subagents secure these files dynamically by spinning up directories wrapped in strict POSIX Access Control Lists (setfacl) tailored exclusively to the target recipient's UID.

4. Upstream Convergence and Deliverable Promotion

When an agent mutates its environment by adding a library or a binary dependency, it logs the change to its local DEED.md file under a ### Dynamic Alterations markdown block. The host orchestrator monitors this file out-of-band. Upon task completion, the orchestrator parses these blocks and writes them back into the parent template inside tavern/blueprints/, ensuring the project's infrastructure blueprints dynamically evolve alongside the code.

Final deliverables follow a strict validation loop. Because the reference parent directory is mounted read-only (:ro), agents cannot corrupt the host workspace. They write finished products to /var/freight/. The Mayor evaluates the output against the core goals inside its single-turn validation module. If approved, the host orchestrator catches the confirmation signal, strips any malicious characters to ensure name-safety, programmatically copies the artifact into keep/vault/, and safely spins down the execution session.

5. Data Management and Multi-Verse Routing

The system separates operational state from telemetry processing to protect the file-as-truth paradigm:

- The System State Map: Handled via an in-memory BTreeMap serialized atomically to keep/charter.json on the host file tree, keeping the core system human-readable and completely transparent.

- The High-Velocity Metrics Flow: Handled via an embedded transactional key-value store (sled) compiled into the host daemon. sled captures token tallies, API costs, latency spikes, and system event trace streams out-of-band.

6. The Fiduciary & Project Tracking Plane (guild/ledger/)

The Guild Ledger shifts the framework from a single-task sandbox into a multi-tenant project manager.
- commission[n]/ Lifecycle: Each project behaves like an independent world tracking slice. The central repository (git/) acts as the project’s permanent history.

- COMMISSION.md & INDENTURE.md Interaction: When the player adds a project, COMMISSION.md stores the baseline text prompts. The Mayor (manor/MAYOR.md) reads this file during a reconstitution wake-up, breaks down the multi-step goals, and writes individual, bounded target contracts directly to INDENTURE.md. Subagents scan their respective entries in INDENTURE.md to establish their local execution checklists before spawning git work branches.

7. The Hibernate and Cold-Storage Plane (tavern/patrons/)

The introduction of patrons/ acts as the village’s Agent Stasis Pool. It decouples active compute allocations (RAM/CPU) from the durable structural profiles of your agents.

- The Stasis Archive: When a subtask finishes, or the orchestrator needs to free up system resource bounds (parcel[uid] limits), it executes a Hibernation Routine.

- PROPRIETOR[name]/ Architecture: The host daemon stops the active container thread, copies the user’s home workspace to /home/uid.bak/, and serializes their modified metadata profile (gained from their active run) directly into PROPRIETOR.md.

- The Quick Awakening: When a new commission demands an identical skill configuration, the orchestrator reads PROPRIETOR.md, provisions the next available unprivileged system UID, skips clean template re-downloads, drops the /home/uid.bak/ cache straight into the new parcel[uid]/home/ volume path, and breathes life into the workshop instantly.

8. The Structural Tenant Leasing Plane (keep/census/)

Subtask slot directories to parcel[uid]/ maps to how rootless Podman slices host storage boundaries.

- TITLE.md & Historical Accountability: This acts as your cryptographic or text chain-of-title. It tracks the chronological lineage of which specific upstream agent or player prompt triggered this workspace parcel, giving you an auditable trace of every agent that mutated data within that filesystem lease boundary.

- Dual-State Convergence (DEED.md vs. PROPRIETOR.md): This cleanly separates the environment layer from the intelligence layer.
	- If a subagent installs a binary tool via Pixi or Go, the change logs to DEED.md (the land lease).
	- If the subagent alters its system prompt behaviors, memory weightings, or operational methods, it commits those changes to PROPRIETOR.md (the individual person).
	- When a commission ends, the orchestrator handles deep promotion checks independently: updating guild/blueprints/ with the code environment states from DEED.md, and saving the updated intelligence profile back to tavern/patrons/ for the next task run.

