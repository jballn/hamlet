# Hamlet v0.1.0-rc1 Release Notes

**Release Candidate 1**  
**Date:** 2026-10-21  
**Specification:** `SPEC.md` (v1.0 LOCKED)  
**Conformance:** 100% (60+ normative clauses verified in `docs/audit/conformance-matrix.md`)

---

## 1. Executive Summary

Hamlet is an autonomous, containerized village runtime for multi-agent software engineering. Agents live in isolated rootless Podman containers, interact via human-inspectable local filesystem structures (`.hamlet/`), communicate across explicit mail and freight planes, and operate under the oversight of an automated Mayor executive.

Version `0.1.0-rc1` marks the completion of the 10-sprint engineering roadmap, implementing Stages 1, 2, and 3 with full SPEC v1.0 conformance, extensive adversarial test coverage, and a marathon multi-commission soak run.

---

## 2. Stage Completion Status

| Stage | Subsystem | Scope | Status |
| :--- | :--- | :--- | :---: |
| **Stage 1** | **Host Daemon & CLI** | Singular governor (`hamlet-daemon`), rootless Podman runtime, typed village filesystem tree (`.hamlet/`), atomic `charter.json`, SafeName sanitizer, pi session supervisor, sequential mail delivery, out-of-band divine interrupts, Mayor single-turn executive loop, decree processor, commission state machine, Git push mediation, freight ACL sandboxes, vault promotions, dual-state convergence (DEED/PROPRIETOR), patron hibernation/stasis, sled telemetry, local REST API, and full normative CLI command surface (`hamlet init`, `daemon`, `commission`, `agent`, `mail`, `vault`, `escalation`, `tui`, `web`). | **COMPLETE** |
| **Stage 2** | **Terminal UI (TUI)** | MUD-style terminal user interface (`hamlet-tui`) connecting over localhost REST/WS with zero direct disk access to village state. Renders 6 SPEC §2 landmark rooms (Guild Hall, Tavern, Courier Post, Market Stalls, Keep & Vault, Mayor's Manor), real-time agent status roster, active escalations drawer, and interactive command publication. | **COMPLETE** |
| **Stage 3** | **WASM 8-Bit Village** | Browser-based 8-bit RPG village frontend (`hamlet-web`) powered by HTML5 2D Canvas driven by a pure-Rust scene-graph view-model compiled to WebAssembly (`hamlet_web.wasm` < 2MB). Features Kenney roguelike sprite sheets, bidirectional WebSocket event streaming (`/api/v1/events/ws`), chat bubbles with honesty enforcement (private mail bodies strictly suppressed on spectator wire), mini sparklines, and a slide-out pixel management drawer supporting live operator interventions. | **COMPLETE** |

---

## 3. Architecture Decision Records (ADRs 0001–0044)

All 10 deferred `[DEV]` items from SPEC §14 and critical architectural invariants are governed by numbered ADRs in `docs/adr/`:

| ADR | Title | Scope / Resolution |
| :--- | :--- | :--- |
| **0001** | Workspace layout: 5-crate split & purity seam | 5 workspace crates (`core`, `daemon`, `cli`, `tui`, `web`); `hamlet-core` has zero I/O side effects. |
| **0002** | Path/name sanitizer: §4 rules + SafeName newtype | Boundary name allowlist `[A-Za-z0-9._-]` preventing path traversal. |
| **0003** | Tree layout API: accessor-only path policy | Strongly typed paths; no ad-hoc `format!`/`push`. |
| **0004** | Charter schema v1, canonical JSON & atomic persistence | Sorted keys, atomic temp-file fsync rename, monotonic versioning. |
| **0005** | sled telemetry: location, key layout & retention | Telemetry sink in `keep/telemetry.sled/`; never orchestration state. |
| **0006** | Concurrency-safe atomic writes: unique temp per attempt | PID + thread + timestamp unique tempfiles to prevent collisions. |
| **0007** | Bootstrap FS boundary: never follow, never adopt | Refuse symlink traversals and non-empty foreign directories. |
| **0008** | Slot identity uniqueness is a load-time invariant | Static mapping of agent identity to permanent UID slots (1001–2000). |
| **0009** | Blueprint TOML schema & immutability encoding | Pinned toolchains, base container images, and hash integrity. |
| **0010** | Podman invocation policy: CLI-only, argv-only, rootless | Process pipes only; zero libpod bindings; rootless user namespaces. |
| **0011** | Host permission model & subuid userns mapping | Posix ownership mapping for rootless container execution. |
| **0012** | Blueprint execution: derived parcel images & pinned toolchains | Per-blueprint base image derivation and cached toolchain bootstrap. |
| **0013** | pkgcache trust model: shared mirror, digest-verified staging | Content-addressed archive caching matching `guild/pins/`. |
| **0014** | Parcel env policy: bootstrap-derived prefixes, refused keys | Strict environment hygiene; refusal of dangerous host variables. |
| **0015** | Session protocol: pi RPC binding, envelopes, interrupt_and_steer | Agent session stdio JSON-RPC protocol and out-of-band steering. |
| **0016** | Stream metric extraction: spike definition & usage accounting | Daemon parsing of token counts, latency, and cost from RPC stream. |
| **0017** | Telemetry sink: non-blocking extraction, bounded evidence | Fast-path metric ingestion and bounded ring buffers. |
| **0018** | Metrics liveness: bounded staleness for the live query surface | Real-time sliding window projections for CLI/TUI/web. |
| **0019** | Mail format, spool lifecycle & watcher vocabulary | RFC-822-style spools in `courier/mail/` and notify event lexicon. |
| **0020** | Dispatch protocol: outbox/inbox schemas, allowlist, retry policy | Outbound webhook requests and inbound HTTP payload dispatch. |
| **0021** | Freight ACL sandboxes: audit, quarantine, promotion-read | Shared filesystem deliverables under strict POSIX ACL boundaries. |
| **0022** | Inbound webhook endpoints: auth, routing table, event envelope | Local token authentication and envelope normalization. |
| **0023** | Dispatch outbox runtime: attempt checkpoints & idempotent recovery | Checkpointed delivery attempts and exponential backoff. |
| **0024** | Musl runtime libraries belong in the daemon apk floor | Alpine container runtime compatibility floor. |
| **0025** | Derived image CLEARS parent body ENTRYPOINT/CMD | Clean container exec execution for agent processes. |
| **0026** | Decree protocol: vocabulary, encodings, acks, replay ids | Strongly typed Mayor decree schemas and confirmation tracking. |
| **0027** | Context packet: MAYOR.md + state grid + one event artifact | Single-turn pristine context assembly for stateless Mayor wake. |
| **0028** | Mayor session lifecycle: fresh invocation per wake | Zero-token sleep; fresh ephemeral evaluation per wake trigger. |
| **0029** | Decree execution and mayor wake triggers: config-driven wakes | Event-driven wake bridges and escalation routing. |
| **0030** | Decree boundary hardening: provenance checks, unshare writes | Strict validation of decree origin and file integrity. |
| **0031** | ACL listing keeps owner header | Preservation of owner metadata in freight sandbox listings. |
| **0032** | Guarded-suite host preconditions: skip with a remedy | Skip-with-remedy semantics for guarded container tests. |
| **0033** | Promotion-read pre-read kind check escalates through userns | Safe permission escalation for freight deliverable inspection. |
| **0034** | Manor home brace is recursive | Recursive ownership enforcement for Mayor container tree. |
| **0035** | Publish-arm fault-injection seam for mirror-warm branch | Resilient retry handling for pkgcache warming. |
| **0036** | Commission state machine: Draft→Active→WindingDown→Closed | Lifecycle states, INDENTURE.md contracts, and closure gates. |
| **0037** | Git push mediation: bundle spool, validation ladder, GitDiffReady | Agent git bundle pushes, bare repo validation, and Mayor diff wakes. |
| **0038** | DEED Dynamic Alterations grammar and promotion request schema | Blueprint write-back and deliverable promotion workflow. |
| **0039** | Stasis formats and census: patron schema, PROPRIETOR grammar | Dual-state convergence, patron stasis, and TITLE.md audit trail. |
| **0040** | Resource bounds & pressure-triggered hibernation policy | Memory and token limits triggering automated stasis. |
| **0041** | Daemon local API contract: localhost HTTP+WS, 0600 token auth | REST and WebSocket projections; zero-orchestration UI principle. |
| **0042** | CLI command tree and TUI architecture | Clap CLI surface and Ratatui MUD client architecture. |
| **0043** | TUI rooms on the SPEC §2 tree: landmark + escalation projections | TUI room navigation and operator escalation handling. |
| **0044** | WS protocol framing and web architecture: bidirectional frames, 2D canvas | Bidirectional WS frames, 2D Canvas rendering, and asset pipeline. |

*Note on Roadmap Renumbering*: Early sprint prompts referenced temporary placeholder ADR numbers (e.g., Sprint 09 referenced ADR-0015 for WS protocol, which was claimed in Sprint 03 for session protocol). The definitive canonical index is maintained in [`docs/adr/README.md`](docs/adr/README.md).

---

## 4. SPEC Conformance Matrix

Full normative compliance against `SPEC.md` v1.0 was audited and verified in Sprint 10. The complete matrix is located at:
[`docs/audit/conformance-matrix.md`](docs/audit/conformance-matrix.md)

### Audit Highlights:
- **60+ Normative Requirements Evaluated**: 100% evaluated as **CONFORMS**.
- **Every Path in SPEC §2**: Checked against `hamlet-core::tree` typed accessors.
- **Architectural Seams (§3)**: Confirmed zero I/O leaks into `hamlet-core`; zero orchestration state in sled (telemetry only); zero direct `.hamlet/` disk mutations from `hamlet-cli`, `hamlet-tui`, or `hamlet-web`.
- **Mount Table & Container Isolation (§4)**: Verified closed mount table, read-only git/context mounts, rootless user namespaces, and unshare slot chown boundaries.
- **Sequential Delivery & Steering (§6)**: Verified FIFO stdin delivery and distinguishable out-of-band JSON-RPC steering.
- **Stateless Mayor Loop (§7)**: Verified single-turn pristine context assembly, zero-token sleep, decree verification, and escalation routing.
- **Commission Lifecycle & Convergence (§9, §10)**: Verified INDENTURE task assignment, Git push mediation, vault promotion, DEED dynamic alterations promotion, and patron stasis hibernation/awakening.
- **Audit Findings Remediation**: All findings identified during Step 01 (`10/01-01` production governor orchestration, `10/01-02` git archive topology conformance, `10/01-03` CLI mail read verb) remediated and verified green with dedicated proving tests.

---

## 5. Soak Marathon & Resilience Summary

To guarantee operational stability under heavy load and adversarial conditions, Sprint 10 executed an exhaustive soak marathon (`crates/hamlet-daemon/tests/soak_village.rs` and `scripts/soak-village.sh`):

- **Multi-Tenant Concurrency**: 3 concurrent commissions (`commission-alpha`, `commission-beta`, `commission-gamma`) running simultaneously across 12 agent parcel containers.
- **Communication Storm**: Interleaved 60-message mail storm, git bundle pushes, Posix freight deliveries, and vault promotion evaluations.
- **Lifecycle Churn**: 3 complete hibernate/awaken cycles for all 12 agents (36 hibernations, 36 awakenings) with patron re-matching and blueprint version progression (v1 → v2 → v3) driven by DEED alterations.
- **Chaos Injection**: 20 deterministic pseudo-random `kill -9` process interruptions across the daemon lifecycle (seeded PRNG: `0x50A4_2026_1020_0001`), coupled with a 10,000-operation filesystem watcher event storm and WebSocket client flapping.
- **Resilience Invariants Asserted After Every Fault Point**:
  1. `charter.json` loads cleanly and is byte-stable on restart with no activity.
  2. Zero orphan containers, UID slots, or parcel leases.
  3. `TITLE.md` append-only chain-of-title remains unbroken with valid SHA-256 digest chains.
  4. Zero secret or token leaks across filesystem trees, sled databases, or execution logs.
  5. Vault deliverables maintain valid checksums.
  6. All bare Git repositories remain `git fsck` clean.
- **Duration**: 3,596 seconds simulated village time (exceeding the 1,800-second target), completing 1,800 discrete events with zero invariant violations.
- **Remediated Soak Findings**: All soak findings (`10/02-01` vault stasis pipeline delegation, `10/02-02` adversarial agent test race, `10/02-03` secrets exposure seam, `10/02-04` unmanaged session hibernation) remediated and proven green in Step 02b.

---

## 6. Known Limitations & Future Work

Items intentionally excluded from v1.0 are documented in [`docs/BACKLOG.md`](docs/BACKLOG.md) and scheduled for post-1.0 development:

1. **Multi-Host Village Clustering & Federation**: Hamlet currently operates on a single Linux workstation. Post-1.0 will introduce Raft-based charter replication and mTLS cross-node freight synchronization.
2. **Operator Identity Federation & Web RBAC**: Local token authentication (`0600` secret token) is currently used. Enterprise OIDC/OAuth2 and role-based access control (Observer, Commissioner, Mayor Administrator) are planned.
3. **GPU Passthrough & Container Device Interface (CDI)**: Agent parcels run CPU-only toolchains; host GPU passthrough for local LLM inference engines (Ollama, vLLM) inside parcels is queued for post-1.0.
4. **Dynamic cgroup v2 Throttling & Adaptive Hibernation**: Real-time memory pressure monitoring (`memory.pressure`) for predictive hibernation under high host load.
5. **Extended Toolchains**: Current blueprints support Rust, Python, and Node.js. Pinned toolchains for Go, Java, OCaml, and Ruby are slated for future releases.
6. **Hierarchical Mayor Context Packet Distillation**: Recursive summarization and semantic indexing for long-running commissions exceeding token context bounds.
7. **Interactive Terminal Streaming in Web UI**: Real-time browser-based container shell access via `xterm.js` over WebSocket.

---

## 7. Verification Battery

The release candidate has passed the complete test and verification battery:

- `cargo fmt --all --check`: **PASS**
- `cargo clippy --workspace --all-targets -- -D warnings`: **PASS**
- `cargo test --workspace` (318+ tests): **PASS**
- `scripts/check-deps.sh` (locked dependency allowlist): **PASS**
- `scripts/build-web.sh` (WASM package < 2MB, magic header, sha256 manifest): **PASS**
- Guarded Podman Test Suite (`HAMLET_PODMAN_TESTS=1`): **PASS** (14/14 tests green across 7 suites)
- End-to-End Test Battery (`scripts/e2e-*.sh`): **PASS** (13/13 suites green)
- Secrets Repository Audit: **PASS** (zero keys or live tokens committed)
