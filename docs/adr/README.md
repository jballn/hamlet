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

Keep this index updated in the same commit that adds/changes an ADR.
