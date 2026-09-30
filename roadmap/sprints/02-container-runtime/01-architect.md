---
role: architect
sprint: 02
step: 1
---

# 02/01 — Blueprint schema + mount spec types

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §4 (mount table — normative)
and §5 (blueprints), HANDOFF.md (sprint 01: tree/sanitize/charter/slots API).

## Deliverable 1 — `hamlet-core::blueprint`

Blueprint TOML schema (resolve SPEC §14 [DEV] here → ADR-0006):
`[meta]` name (SafeName), version, base image ref; `[tools]` list of
{kind: pixi|nvm|cargo|apk, name, version-spec}; `[env]` non-secret defaults;
`[skills]` references into `market/skills/`. Parser → typed `Blueprint`,
validation errors are precise (field path + reason). Blueprints are immutable
once loaded by the daemon — mutation only via the Sprint 06 promotion path;
encode this (no `&mut` API surface).

Ship a committed default blueprint `defaults/village-alpine.toml`: Alpine +
uutils + Pixi + NVM + Cargo per SPEC §4.

## Deliverable 2 — `hamlet-core::mount`

Typed mount spec covering the SPEC §4 table exactly: struct per mount kind
(home(uid), mail, freight, git(commission) ro, pkgcache, context ro,
manor-home) with host path derived from `HamletTree` and container path fixed
by spec. `MountSet::for_agent(uid, commission)` and
`MountSet::for_mayor()` constructors; a `to_podman_args()` renderer (pure —
string args only, no execution). Any mount not in the table is unrepresentable.

## Deliverable 3 — ADRs

- `docs/adr/0006-blueprint-schema.md`
- `docs/adr/0007-podman-invocation-policy.md`: CLI-only rootless podman via
  tokio pipes, argument construction rules (no shell interpolation ever —
  Vec<String> args), userns/uid mapping flags, no privileged mode.

Verify: golden TOML parse tests + mount-arg renderer tests. Finish:
HANDOFF.md. Commit: `feat(core): blueprint schema + typed mount specs
(ADR-0006, ADR-0007)`.
