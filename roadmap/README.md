# hamlet — Development Roadmap

Adapts `SPEC.md` (v1.0, LOCKED) into the `~/Development/hamlet` repository
(single repo: spec + Cargo workspace). `MANIFESTO.md` is inspiration only.

## Structure

```
roadmap/
  README.md          ← you are here (sequencing + feeding protocol)
  CONVENTIONS.md     ← shared ground rules; every prompt inherits these
  ROLES.md           ← individualized role instructions for every agent type
  sprints/
    00-foundations/          CI gate, cargo workspace scaffolding, ADR dir, branch model
    01-host-topology/        .hamlet tree module, sanitizer, init bootstrap, charter.json, UID slots, sled
    02-container-runtime/    blueprints schema, rootless podman exec layer, mounts, toolchain bootstrap
    03-agent-engine/         pi session supervision, stdin mail delivery, interrupt_and_steer, telemetry parse
    04-communication/        notify watchers, mail routing, freight ACLs, dispatch webhooks, axum endpoints
    05-mayor-loop/           event queue, context-packet reconstitution, decree vocabulary + executor
    06-commission-lifecycle/ commissions, bare git repos, INDENTURE flow, DEED→blueprint, vault promotion
    07-hibernation-census/   parcel leases, TITLE chain-of-title, hibernate/awaken, dual-state convergence
    08-cli-tui/              full CLI surface consolidation, MUD-style TUI village mock
    09-wasm-frontend/        axum asset serving, WS metrics/chat streams, 8-bit pixel RPG village
    10-final-verification/   spec conformance audit, soak, cleanup, docs, release gate
```

## Sprint anatomy

Every sprint folder contains:

- `OBJECTIVE.md` — goal, scope boundaries, exit criteria (the contract for the sprint)
- `NN-role.md` — one hyperscoped prompt per step, **fed sequentially**, one per agent session

Prompts are numbered in strict execution order. Do not parallelize within a
sprint unless the prompt explicitly says it is parallel-safe.

## Feeding protocol

1. Start each agent session with: the role preamble from `ROLES.md` for that
   prompt's role, then the prompt file verbatim.
2. Every prompt inherits `CONVENTIONS.md` — the agent must read it first
   (the prompts say so explicitly).
3. Each prompt ends by updating `sprints/<sprint>/HANDOFF.md` (created on first
   use) with: what was done, decisions made (ADR lines), open issues, and the
   exact commands to verify. The next prompt in the chain reads HANDOFF.md first.
4. Gate prompts (QA / CI / cleanup / commit) must pass before the next build
   prompt is fed. If a gate fails, re-feed the failing build prompt with the
   gate's findings appended — never skip a gate.
5. When a sprint's exit criteria are met, tag it: `git tag sprint/NN-<name>`.

Automation: `run_roadmap.py` (repo root) feeds every prompt chronologically to
headless `pi --print` workers, enforcing the gate/resume protocol above.

## Global sequencing rules

- **Locked dependency stack** (SPEC §3): no new runtime crates without an ADR
  that justifies against the locked list. `cargo deny`-style baseline lives in
  `scripts/check-deps.sh`.
- **File-as-truth contract is non-negotiable** (CONVENTIONS.md §3). Any prompt
  touching charter/state code must cite the state checklist in its commit.
- **Singular-governor contract**: only `hamlet-daemon` spawns processes or
  mutates the tree at runtime; `hamlet-core` stays pure (no I/O).
- **Design decisions for [DEV] items** are made inside the prompt that first
  needs them and recorded as an ADR in `docs/adr/`. Later prompts treat ADRs
  as binding.
- SPEC.md is the single source of truth. If a prompt and SPEC.md conflict,
  SPEC.md wins and the agent must flag the conflict in HANDOFF.md instead of
  silently resolving it.

## The eleven sprints at a glance

| # | Sprint | Delivers | Spec sections |
|---|--------|----------|---------------|
| 00 | Foundations | CI battery, cargo workspace skeleton (5 crates, stubs), ADR dir, branch model | §3, §13 |
| 01 | Host topology | Typed tree layout, path sanitizer, `hamlet init`, config/.env, atomic charter.json, UID slots, sled telemetry | §2, §11 |
| 02 | Container runtime | Blueprint schema (ADR), podman exec layer, UID mapping, normative mounts, Alpine+uutils+Pixi/NVM/Cargo bootstrap | §4, §5 |
| 03 | Agent engine | pi session supervision over podman exec, sequential stdin delivery, `interrupt_and_steer`, JSON-RPC telemetry→sled | §6 |
| 04 | Communication | notify watchers, mail spool routing, freight POSIX ACLs, dispatch outbox/inbox, axum webhook endpoints | §8 |
| 05 | Mayor loop | Event queue + wake triggers, single-turn context reconstitution, decree vocabulary (ADR), decree executor | §7 |
| 06 | Commission lifecycle | commission new/list/close, bare git repos, INDENTURE decomposition flow, DEED→blueprint promotion, vault promotion | §9, §7 |
| 07 | Hibernation & census | parcel leases, TITLE chain-of-title, hibernate/quick-awaken, dual-state convergence, resource bounds | §10 |
| 08 | CLI & TUI | Full normative CLI surface (§12.1), MUD-style TUI village mock over daemon API | §12.1, §12.2 |
| 09 | WASM frontend | axum asset layers, WS metrics/chat streams, 8-bit RPG village with interactable sprites + pixel work UI | §12.3 |
| 10 | Final verification | Full spec conformance audit, restart-resumability soak, cleanup, docs, release candidate | all |

72 sequenced prompts total (build/test + gate/merge). Sprints 01→07 are the
correctness heart (the daemon governor); do not rush their QA gates.
