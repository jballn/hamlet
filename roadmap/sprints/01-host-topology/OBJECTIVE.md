# Sprint 01 — Host Topology

**Goal:** The `.hamlet/` tree exists as typed code: layout module, sanitizer,
`hamlet init` bootstrap, config/.env loading, atomic charter.json state model
with UID slot allocation, and the sled telemetry store. This sprint makes the
file-as-truth paradigm real and restart-resumable.

**Scope boundary:** No podman, no agents, no watchers. Pure core + CLI init +
daemon telemetry skeleton.

**Exit criteria (SPEC §2, §11):**
- `hamlet-core::{tree,sanitize,charter,slots,config}` implemented with golden
  vectors; sanitizer per CONVENTIONS.md §4.
- `hamlet init <dir>` creates the full normative tree including empty
  charter.json (versioned) and config.json; `.env` template at 0600.
- Atomic charter writes (temp+fsync+rename); BTreeMap-only state; kill/restart
  resumability test passes.
- UID slots: 1000 reserved; 1001–2000 sequential permanent allocation;
  exhaustion = hard error.
- sled opens under the daemon's data dir; metrics API (tokens/cost/latency/
  trace) compiles with unit tests; no orchestration state in sled.
- ADRs: 0002 (sanitizer), 0003 (tree layout API), 0004 (charter schema),
  0005 (sled key layout).
- Branch merged, tag `sprint/01-host-topology`.

## Prompt sequence
1. `01-architect.md` — tree layout + sanitizer (ADR-0002)
2. `02-systems-engineer.md` — init bootstrap + config/.env
3. `03-systems-engineer.md` — charter state model + UID slots (ADR-0004)
4. `04-systems-engineer.md` — sled telemetry store (ADR-0005)
5. `05-qa-engineer.md` — adversarial suite (traversal, atomicity, exhaustion)
6. `06-ci-agent.md` — gate
7. `07-commit-agent.md` — merge & tag
