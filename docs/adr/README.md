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
| [0012](0012-blueprint-execution-derived-images.md) | Blueprint execution: derived parcel images & pinned toolchains | Accepted (cl. 2 amended by ADR-0024, ADR-0025) |
| [0013](0013-pkgcache-trust-model.md) | pkgcache trust model: shared mirror, digest-verified staging | Accepted |
| [0014](0014-parcel-env-policy.md) | Parcel env policy: bootstrap-derived prefixes, refused keys | Accepted |
| [0015](0015-session-protocol.md) | Session protocol: pi RPC binding, envelopes, interrupt_and_steer | Accepted (Sprint 03 prompts cite it as "ADR-0008") |
| [0016](0016-stream-telemetry-extraction.md) | Stream metric extraction: spike definition & usage accounting | Accepted |
| [0017](0017-telemetry-sink-and-query-surface.md) | Telemetry sink: non-blocking extraction, bounded evidence, query surface | Accepted |
| [0018](0018-metrics-liveness.md) | Metrics liveness: bounded staleness for the live query surface | Accepted (refines ADR-0017 cl. 2) |
| [0019](0019-mail-format-and-watcher-vocabulary.md) | Mail format, spool lifecycle & watcher vocabulary | Accepted (Sprint 04 prompts cite it as "ADR-0009") |
| [0020](0020-dispatch-protocol.md) | Dispatch protocol: outbox/inbox schemas, allowlist, retry policy | Accepted (Sprint 04 prompts cite it as "ADR-0010") |
| [0021](0021-freight-acl-sandboxes.md) | Freight ACL sandboxes: audit, quarantine, promotion-read | Accepted (amends ADR-0011 cl. 3 freight row) |
| [0022](0022-inbound-webhooks.md) | Inbound webhook endpoints: auth, routing table, event envelope | Accepted (refines ADR-0020 for daemon-originated inbox files) |
| [0023](0023-dispatch-runtime-checkpoints.md) | Dispatch outbox runtime: attempt checkpoints & idempotent recovery | Accepted (refines ADR-0020 cl. 4–5 runtime contract) |
| [0024](0024-musl-runtime-libraries-in-the-apk-floor.md) | Musl runtime libraries belong in the daemon apk floor | Accepted (amends ADR-0012 cl. 2; fixes QA04-05) |
| [0025](0025-derived-image-clears-parent-body.md) | The derived image CLEARS its parent's ENTRYPOINT/CMD | Accepted (amends ADR-0012 cl. 2; fixes QA04-06) |
| [0026](0026-decree-protocol.md) | Decree protocol: vocabulary, encodings, acks, replay ids | Accepted (amends ADR-0019 cl. 5 decree row) |
| [0027](0027-context-packet-schema.md) | Context packet: MAYOR.md + state grid + one event artifact | Accepted |
| [0028](0028-mayor-session-lifecycle.md) | Mayor session lifecycle: fresh invocation per wake, merged wakes, bounded queue | Accepted (amends ADR-0027 cl. 2 additively) |
| [0029](0029-decree-execution-and-wake-triggers.md) | Decree execution and mayor wake triggers: config-driven wakes, honest seams, escalation notices | Accepted |
| [0030](0030-decree-boundary-hardening.md) | Decree boundary hardening: provenance checks, unshare writes, stable-file quarantine, wake identity | Accepted (fixes QA05-01/02/03/05/06; amends ADR-0029 cl. 1, ADR-0028 cl. 3) |
| [0031](0031-acl-listing-keeps-owner-header.md) | The freight ACL listing keeps its `# owner:` header (`-c` is never re-added) | Accepted (amends ADR-0021 cl. 1; fixes QA04-12) |
| [0032](0032-guarded-host-preconditions.md) | Guarded-suite host preconditions: skip with a remedy, never a vacuous green | Accepted (amends CONVENTIONS §5; resolves QA04-07/08/09) |
| [0033](0033-promotion-read-kind-check-escalation.md) | Promotion-read's pre-read kind check escalates through the userns | Accepted (amends ADR-0021 cl. 6; fixes QA04-13) |
| [0034](0034-manor-home-brace-is-recursive.md) | The manor home brace is recursive: a private home owns its whole subtree | Accepted (amends ADR-0011 cl. 2; fixes QA04-23; cl. 5 pins the brace's no-dereference traversal policy — QA04-29; cl. 6 records QA04-28 fix) |
| [0035](0035-publish-arm-injection-seam.md) | Publish-arm fault-injection seam for the mirror-warm branch | Accepted (amends ADR-0013 cl. 3; fixes QA04-32) |
| [0036](0036-commission-state-machine.md) | Commission state machine: Draft→Active→WindingDown→Closed (+Failed), charter v4 records, INDENTURE grammar | Accepted (amends ADR-0027 cl. 2 state grid; retypes the charter v3 commission placeholder) |
| [0037](0037-git-push-mediation.md) | Git push mediation: bundle spool, validation ladder, conflict policy, `GitDiffReady` wake | Accepted (resolves SPEC §14 git mediation) |
| [0038](0038-deed-alterations-promotion.md) | DEED `### Dynamic Alterations` grammar and promotion request schema | Accepted (blueprint write-back upsert + vault promotion naming) |

Keep this index updated in the same commit that adds/changes an ADR.

## Renumbering note (binding on later sprint prompts)

Roadmap prompts were written before numbers were claimed, and numbers are never
reused. Where a later prompt cites one of these, read the ADR actually written:

| Prompt cites | Subject it means | Claimed by | Use instead |
|---|---|---|---|
| Sprint 02 "ADR-0006" | blueprint schema | 0006 (atomic writes) | **ADR-0009** |
| Sprint 02 "ADR-0007" | podman invocation policy | 0007 (bootstrap fs boundary) | **ADR-0010** (+0011) |
| Sprint 03 "ADR-0008" | session protocol + interrupt schema | 0008 (slot identity uniqueness) | **ADR-0015** |
| Sprint 04 "ADR-0009" | mail format + watcher vocabulary | 0009 (blueprint schema) | **ADR-0019** |
| Sprint 04 "ADR-0010" | dispatch protocol + retry policy | 0010 (podman invocation) | **ADR-0020** |
| Sprint 05 "ADR-0011" | decree protocol + context packet | 0011 (host permission model) | **ADR-0026** (decrees) + **ADR-0027** (packet) |
| Sprint 06 "ADR-0012" | commission state machine + git mediation | 0012 (derived images) | **ADR-0036** (state machine) + **ADR-0037** (git mediation) + **ADR-0038** (DEED promotion) |
| Sprint 07 "ADR-0013" | stasis formats + resource policy | 0013 (pkgcache trust) | next free number (0016+) |
| Sprint 08 "ADR-0014" | CLI surface + local API contract | 0014 (parcel env policy) | next free number (0016+) |
| Sprint 09 "ADR-0015" | WS protocol + asset architecture | 0015 (session protocol) | next free number (0016+) |
