# hamlet — Design Working Log (SUPERSEDED)

> **NOTE:** This document is the design-derivation working log from
> `MANIFESTO.md`. The locked, submittable specification is **`SPEC.md`**
> (v1.0). Where the two differ, `SPEC.md` governs.

Status: **concept → spec phase complete.** Decisions recorded below are agreed
unless marked OPEN.

---

## 1. Derivation from the manifesto

### 1.1 Core identity
- Containerized sandbox + orchestration engine for **long-horizon multi-agent
  lifecycles**, wrapped in a pixel RPG *eventually* — presentation is the last
  layer, never the driver.
- Each agent = one mapped system user in a slim rootless-Podman container.
- Host daemon (Rust/tokio) is the **singular all-powerful governor**: it owns
  podman, UIDs, mounts, promotion, interrupts. Agents own nothing
  infrastructure-wise.
- Orchestration is **file-driven**: the daemon watches the host filesystem
  (notify/inotify) to route workflows, parse telemetry, and drive colony
  progression.

### 1.2 Village topology → normative tree
The manifesto's `.hamlet/` layout (guild/tavern/courier/market/keep) is adopted
verbatim as the normative tree (SPEC §2), with one naming fix: the manifesto's
`keep/charter.json` reference vs. root `charter.json` — **DECIDED: root
`.hamlet/charter.json`** (it is system-global, not a keep artifact). Manor
home is `/home/1000/`; subagent slots `1001–2000`.

### 1.3 Mount contract
Six strategic mounts adopted as normative (SPEC §4 table): home (rw), mail
(rw), freight (rw+ACL), git (**ro**), pkgcache (rw), context (**ro**). The
ro-git mount forces push-mediation through the daemon — flagged OPEN →
resolved as [DEV] in Sprint 06 (candidates: daemon-side `git push` proxy over
a unix socket in the parcel, or a commit-drop spool the daemon applies).

### 1.4 The Mayor
- Stateless event processor, UID 1000, zero-token sleep. Wake → pristine
  single-turn packet (MAYOR.md + charter.json + event artifact) → decrees via
  filesystem writes → sleep.
- **DECIDED**: decrees are *files*, not podman commands. The Mayor never
  executes; the daemon interprets. This keeps the singular-governor pillar
  airtight and makes every decision auditable from the tree.

### 1.5 Agent engine
- pi coding-agent sessions inside containers; daemon attaches via
  `podman exec` + piped stdio (tokio `process` feature).
- Mail → sequential stdin delivery (clear task boundaries).
- **Divine interruption**: out-of-band JSON-RPC `interrupt_and_steer` injected
  directly down the active pipe, bypassing the queue.
- Telemetry is **daemon-parsed** from the JSON-RPC stream into sled; agents
  never self-report (trust boundary).

### 1.6 Dual-state convergence
- DEED.md = land lease (environment: pixi/go/npm installs logged under
  `### Dynamic Alterations`). PROPRIETOR.md = the person (prompt behaviors,
  memory weightings, methods).
- Commission end → two independent daemon promotions: DEED → blueprint
  evolution; PROPRIETOR → patron stasis profile.
- TITLE.md = chain-of-title lineage (which prompt/agent triggered which lease).
  **DECIDED**: append-only, daemon-stamped.

### 1.7 Hibernation / stasis pool
- `tavern/patrons/PROPRIETOR<name>/` holds `home.uid.bak/` + serialized
  PROPRIETOR.md. Awakening skips clean template re-downloads and drops the
  cached home straight into the new parcel.
- **DECIDED**: UID *slot* mappings persist in charter.json even while
  hibernated (permission cohesion across reboots); compute is freed, identity
  is not.

### 1.8 State vs. telemetry split
- charter.json: in-memory BTreeMap → atomic serialize (temp+rename). Sorted
  keys, human-readable, versioned schema.
- sled: token tallies, costs, latency spikes, traces. Out-of-band, embedded
  in the daemon. Archives exported to `keep/archives/` on commission close.

### 1.9 Tech stack
Adopted exactly as the manifesto's dependency diagram (tokio, axum,
tokio-tungstenite, serde/serde_json, sled, dotenvy, regex, reqwest, notify).
No additional runtime dependencies without an ADR.

---

## 2. Decisions made during spec drafting

- **Single repo**: spec + code live together in `~/Development/hamlet`
  (unlike cai-shens-cord, which targeted an existing engine repo). Cargo
  workspace under `crates/`. MANIFESTO.md is non-normative inspiration;
  SPEC.md is law; roadmap prompts cite SPEC sections.
- **Crate split**: `hamlet-core` (pure) / `hamlet-daemon` (governor) /
  `hamlet-cli` / `hamlet-tui` / `hamlet-web`. Purity of core is the
  testability seam — no podman needed for logic tests.
- **Staging**: Stage 1 daemon+CLI (sprints 00–08), Stage 2 TUI (08), Stage 3
  WASM RPG (09), audit/release (10). CLI grows incrementally with each
  feature sprint; sprint 08 consolidates the full surface + TUI.
- **Security invariants are QA-enforced test targets**, not aspirations
  (SPEC §4 isolation invariants): ro mounts, spool perms, no socket access,
  traversal-blocked promotion.
- **Restart-resumability contract**: daemon restarted on a foreign host with
  the same `.hamlet/` tree resumes identically (SPEC §13). This forces all
  state through charter/files early — decided in Sprint 01, policed ever
  after.

## 3. OPEN — handed to [DEV] via ADRs

| Open item | Resolved in |
|---|---|
| Charter JSON schema + migration policy | Sprint 01 (ADR) |
| Blueprint TOML schema; skill injection | Sprint 02 (ADR) |
| Podman exec supervision (restart limits, OOM) | Sprint 02/03 (ADR) |
| `interrupt_and_steer` wire schema | Sprint 03 (ADR) |
| Mail header format; watcher debounce policy | Sprint 04 (ADR) |
| Dispatch retry/backoff; webhook auth | Sprint 04 (ADR) |
| Decree file encodings + acknowledgment | Sprint 05 (ADR) |
| Git push mediation for ro `/var/git/` | Sprint 06 (ADR) |
| Resource bounds + hibernation pressure triggers | Sprint 07 (ADR) |
| WS protocol for canvas metrics/chat | Sprint 09 (ADR) |

## 4. Explicitly out of scope for v1

- Non-Linux hosts (macOS/Windows podman machines).
- Cross-commission agent identity federation.
- Encrypted at-rest state (charter/mail are plaintext by design —
  human-readable transparency pillar).
- The full RPG art pipeline (stage 3 delivers a functional pixel village,
  not a finished game).
