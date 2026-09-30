---
role: architect
sprint: 01
step: 1
---

# 01/01 — Typed tree layout + path sanitizer

Read first: `roadmap/CONVENTIONS.md` (§2, §4), `SPEC.md` §2 and §11.3,
HANDOFF.md from sprint 00.

## Deliverable 1 — `hamlet-core::tree`

Typed layout for the entire normative `.hamlet/` tree (SPEC §2). Design:
a root `HamletTree { root: PathBuf }` with accessor methods returning owned
typed paths for EVERY normative location (guild register/blueprints/ledger,
commission dirs by index, tavern gossip/patrons + patron dirs by name,
courier mail/freight/dispatch out+inbox, market skills/pkgcache, keep
vault/archives/census, parcel dirs by uid, manor, charter/config/env paths).
Include `ensure(root)` creating the full empty tree with correct modes
(`.env` 0600; mail spool dirs per-UID lazy). No string-concatenation of
village paths anywhere else in the workspace ever again — make the API the
only ergonomic option.

## Deliverable 2 — `hamlet-core::sanitize`

Implement CONVENTIONS.md §4 verbatim (allowlist `[A-Za-z0-9._-]`, length
1–64, reject `.`/`..`/leading `-`/control chars/separators/non-ASCII).
Regex-based. API: `sanitize_name(&str) -> Result<SafeName, SanitizeError>`
where `SafeName` is a newtype that is the ONLY accepted input for tree
accessors taking external names (patron names, commission slugs, vault
artifact names, mail recipients). Commit golden test vectors (accept list +
reject list, ≥ 30 cases incl. unicode, traversal, NUL, overlength).

## Deliverable 3 — ADRs

- `docs/adr/0002-path-sanitizer.md` — ratify §4 rules + SafeName newtype.
- `docs/adr/0003-tree-layout-api.md` — accessor-only path policy.

## Guardrails

- `hamlet-core` stays pure: `ensure()` is the single sanctioned I/O point and
  takes the root as a parameter (testable against tempdirs).
- No charter/slots/config logic yet (steps 2–3 own those).

Verify: `cargo test -p hamlet-core` green with golden vectors;
`scripts/ci-local.sh` green. Finish: update HANDOFF.md. Commit:
`feat(core): typed tree layout + SafeName sanitizer (ADR-0002, ADR-0003)`.
