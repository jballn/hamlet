---
role: architect
sprint: 00
step: 2
---

# 00/02 — Cargo workspace skeleton + ADR scaffolding

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §2–§3, and `HANDOFF.md` from
step 1. Replace any placeholder workspace from step 1.

## Deliverable 1 — ADR scaffolding

- `docs/adr/README.md`: what an ADR is here, when one is required (any
  `[DEV]` decision, schema, encoding, threshold), numbering rule, supersede
  rule.
- `docs/adr/0000-template.md`: Context / Decision / Consequences, ≤ 40 lines.
- `docs/adr/0001-workspace-layout.md`: ratify the 5-crate split and the
  purity-seam rule (CONVENTIONS.md §2): core pure / daemon governor / cli /
  tui / web. Status: Accepted.

## Deliverable 2 — workspace skeleton

Root `Cargo.toml` workspace with resolver 2, shared `[workspace.dependencies]`
pinned to the SPEC §3 stack. Create:

```
crates/
  hamlet-core/src/
    lib.rs        — crate map, re-exports, pointer to SPEC.md
    tree.rs       — typed .hamlet/ layout (SPEC §2)          // TODO(sprint 01)
    sanitize.rs   — path/name sanitizer (SPEC §11.3)          // TODO(sprint 01)
    charter.rs    — state model, atomic serde (SPEC §11.1)    // TODO(sprint 01)
    slots.rs      — UID slot allocation 1000/1001-2000        // TODO(sprint 01)
    config.rs     — config.json + .env schemas                // TODO(sprint 01)
    blueprint.rs  — blueprint schema (SPEC §5)                // TODO(sprint 02)
    mount.rs      — normative mount table (SPEC §4)           // TODO(sprint 02)
    session.rs    — agent session protocol types (SPEC §6)    // TODO(sprint 03)
    mail.rs       — mail format + routing types (SPEC §8)     // TODO(sprint 04)
    decree.rs     — decree vocabulary (SPEC §7)               // TODO(sprint 05)
    commission.rs — commission state machine (SPEC §9)        // TODO(sprint 06)
    promote.rs    — DEED/vault promotion parsing (SPEC §9.4)  // TODO(sprint 06)
    stasis.rs     — hibernation/census types (SPEC §10)       // TODO(sprint 07)
  hamlet-daemon/src/
    lib.rs + main.rs — stub binary; module stubs with TODO(sprint NN):
    podman.rs (02) supervise.rs (03) watch.rs (04) mailroute.rs (04)
    freight.rs (04) dispatch.rs (04) web.rs (04/09) mayor.rs (05)
    ledger.rs (06) census.rs (07) telemetry.rs (01)
  hamlet-cli/src/main.rs  — stub `hamlet` binary (clap), prints version // TODO(sprint 08)
  hamlet-tui/src/lib.rs   — stub // TODO(sprint 08)
  hamlet-web/src/lib.rs   — stub // TODO(sprint 09)
```

Every stub: doc comment stating responsibility + SPEC sections it implements
+ `// TODO(sprint NN)` marker. NO logic yet. Only real dependencies added
where a stub genuinely needs them (likely none beyond serde derive).

## Guardrails

- Zero orchestration logic. Resist scope creep — later prompts own those files.
- `scripts/check-deps.sh` stays green.

Verify: `scripts/ci-local.sh` green. Finish: update HANDOFF.md (decisions →
ADR-0001). Commit:
`feat(core): workspace skeleton, 5-crate layout, ADR-0001`.
