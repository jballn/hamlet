# Architecture Decision Records (ADRs)

An ADR is a short, immutable record of one architectural decision in this
repo: the context that forced it, the decision itself, and its consequences.
ADRs are **binding** on all later work (roadmap/CONVENTIONS.md §10).

## When an ADR is required

Write an ADR before/while implementing any of:

- any decision resolving a `[DEV]` item in `SPEC.md` (schemas, wire formats,
  encodings, thresholds, mappings, retry policies — see SPEC §14);
- any change to the locked dependency stack (SPEC §3 / CONVENTIONS §2),
  including moving a crate from the dev-allowlist to runtime use;
- any state-contract detail (CONVENTIONS §3): charter schema steps, UID slot
  policy, canonical JSON rules;
- anything that changes the crate layout, the purity seam, or the
  singular-governor rule (CONVENTIONS §2).

If in doubt, write one. An ADR is cheaper than an unwritten assumption.

## Format

Copy `0000-template.md`. Three sections: **Context / Decision /
Consequences**. Hard cap: **40 lines** of body (excluding the title/status
header). One decision per file — split coupled decisions into two ADRs.

## Numbering

- Sequential, zero-padded to 4 digits: `NNNN-kebab-case-title.md`.
- Numbers are never reused. `0000` is the template and is never a decision.
- Reserved: `0002` — path/name sanitizer (locked by Sprint 01 per
  CONVENTIONS §4). Check existing files and the sprint HANDOFFs before
  claiming a number.

## Supersede rule

ADRs are immutable. To change a decision:

1. Write a **new** ADR with the next free number.
2. Set its header `Supersedes: ADR-NNNN` and explain the delta in Context.
3. Edit the old ADR's status line only: `Status: Superseded by ADR-MMMM`.
   No other change to the old file is permitted.

## Index

| ADR | Title | Status |
|---|---|---|
| [0001](0001-workspace-layout.md) | Workspace layout: 5-crate split & purity seam | Accepted |
| [0002](0002-path-sanitizer.md) | Path/name sanitizer: §4 rules + SafeName newtype | Accepted |
| [0003](0003-tree-layout-api.md) | Tree layout API: accessor-only path policy | Accepted |
| [0004](0004-charter-schema.md) | Charter schema v1, canonical JSON & atomic persistence | Accepted |
| [0005](0005-sled-key-layout.md) | sled telemetry: location, key layout & retention | Accepted |
| [0006](0006-concurrency-safe-atomic-writes.md) | Concurrency-safe atomic writes: unique temp per attempt | Accepted (supersedes ADR-0004 cl. 3 naming) |
| [0007](0007-bootstrap-fs-boundary.md) | Bootstrap FS boundary: never follow, never adopt | Accepted (amends ADR-0003) |
| [0008](0008-slot-identity-uniqueness.md) | Slot identity uniqueness is a load-time invariant | Accepted (extends ADR-0004 cl. 4) |
| [0009](0009-blueprint-schema.md) | Blueprint TOML schema & immutability encoding | Accepted (Sprint 02 prompts cite it as "ADR-0006") |
| [0010](0010-podman-invocation-policy.md) | Podman invocation policy: CLI-only, argv-only, rootless | Accepted, cl. 3 amended by ADR-0011 (Sprint 02 prompts cite it as "ADR-0007") |
| [0011](0011-host-permission-model.md) | Host permission model & subuid userns mapping | Accepted (amends ADR-0010 cl. 3) |
| [0012](0012-blueprint-execution-derived-images.md) | Blueprint execution: derived parcel images & pinned toolchains | Accepted |
| [0013](0013-pkgcache-trust-model.md) | pkgcache trust model: shared mirror, digest-verified staging | Accepted |
| [0014](0014-parcel-env-policy.md) | Parcel env policy: bootstrap-derived prefixes, refused keys | Accepted |

Keep this index updated in the same commit that adds/changes an ADR.

## Renumbering note (binding on later sprint prompts)

Roadmap prompts were written before numbers were claimed, and numbers are never
reused. Where a later prompt cites one of these, read the ADR actually written:

| Prompt cites | Subject it means | Claimed by | Use instead |
|---|---|---|---|
| Sprint 02 "ADR-0006" | blueprint schema | 0006 (atomic writes) | **ADR-0009** |
| Sprint 02 "ADR-0007" | podman invocation policy | 0007 (bootstrap fs boundary) | **ADR-0010** (+0011) |
| Sprint 06 "ADR-0012" | commission state machine + git mediation | 0012 (derived images) | next free number (0015+) |
| Sprint 07 "ADR-0013" | stasis formats + resource policy | 0013 (pkgcache trust) | next free number (0015+) |
| Sprint 08 "ADR-0014" | CLI surface + local API contract | 0014 (parcel env policy) | next free number (0015+) |
| Sprint 09 "ADR-0015" | WS protocol + asset architecture | — | next free number (0015+) |
