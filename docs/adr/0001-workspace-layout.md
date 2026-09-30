# ADR-0001 — Workspace layout: 5-crate split & purity seam

Status: Accepted
Supersedes: —
Sprint: 00-foundations · Prompt: 02-architect

## Context

SPEC §3 names five normative crates and locks a layering rule; CONVENTIONS §2
makes it review-failable (singular governor, purity seam). The Sprint 00
placeholder held only `hamlet-core`; the full skeleton must exist before any
logic lands so module ownership is unambiguous and `check-deps.sh` polices
real manifests.

## Decision

Cargo workspace under `crates/`, resolver 2, five members:

- **`hamlet-core`** — pure logic: typed `.hamlet/` tree layout (SPEC §2),
  sanitizer (§11.3), charter model + atomic serde (§11.1), UID slots (§4),
  config/blueprint/mount/session/mail/decree/commission/promotion/stasis
  schemas (§§4–10). **Purity seam:** no fs/process/network side effects;
  side-effectful operations are traits injected by the daemon; all core tests
  run without podman or agents.
- **`hamlet-daemon`** — the **singular governor**: only crate that spawns
  processes (podman, pi sessions) or mutates a live `.hamlet/` tree; owns
  watchers, mail routing, freight ACLs, dispatch, Mayor loop, supervision,
  sled telemetry, axum server.
- **`hamlet-cli`** — `hamlet` binary (§12.1); no orchestration state.
- **`hamlet-tui`** — MUD-style frontend over the daemon API (§12.2).
- **`hamlet-web`** — WASM RPG frontend + glue (§12.3); renders only
  daemon-pushed state.

Dependency direction: daemon/cli/tui/web may depend on core; core depends on
none of them. Shared versions live in `[workspace.dependencies]`, pinned to
the SPEC §3 stack; additions require their own ADR.

## Consequences

- Stub modules with `TODO(sprint NN)` markers fix file ownership; logic lands
  only in the crate that owns it.
- Any std fs/process/net use inside `hamlet-core` fails review.
- Presentation crates consume one daemon truth, so no stage can lag another.
