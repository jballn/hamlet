# ADR-0003 — Tree layout API: accessor-only path policy

Status: Accepted (`ensure`'s `.env` clause amended by ADR-0007)
Supersedes: —
Sprint: 01-host-topology · Prompt: 01-architect

## Context

SPEC §2 declares the `.hamlet/` topology normative and forbids ad-hoc
string-concatenated paths; CONVENTIONS §2 makes "typed paths" review-
failable. The `[DEV]` surface: the concrete API shape of
`hamlet-core::tree`, where the one permitted I/O exception to the purity
seam lives, and which locations are eager vs lazy.

## Decision

We will expose the whole normative tree through `tree::HamletTree`:

- **Accessor-only policy**: `HamletTree { root }` has one accessor per
  normative location returning an owned `PathBuf`. Path composition happens
  only in a private `join`; directory literals exist exactly once as private
  constants. No other crate may `format!`/`push` village paths — if an
  accessor is missing, add one here (with tests), never inline a path.
- **Typed parameters**: identity-scoped locations take integers
  (`commission(n: u64)`, `parcel(uid: u32)`) or `&SafeName` for
  externally-derived names (`patron`, `vault_artifact`,
  `courier_mail_recipient`; ADR-0002). Raw `&str` is never accepted.
- **`ensure(root) -> io::Result<HamletTree>`** is the single sanctioned I/O
  point in `hamlet-core` (a narrow, recorded exception to ADR-0001's purity
  seam: it is idempotent, root-injected, and tempdir-testable). It creates
  all static directories and `.env`, and forces `.env` to mode **0600**
  even if it already exists (contents preserved).
- **Eager vs lazy**: `ensure` creates only identity-independent dirs.
  Per-UID/per-name locations (mail spools, parcels, patron dirs, vault
  artifacts, commissions) are lazy — daemon-created on allocation.

## Consequences

- The tree module is the sole place SPEC §2 is encoded; a spec change means
  one diff, one review, one golden path-shape test update.
- `hamlet init` (step 02) and daemon watchers compose `ensure` + accessors
  and add no path logic of their own.
- Restart-resumability tests can rebuild any village by calling `ensure`
  against the same root — idempotence is a contract, not a convenience.
- Any `std::fs` use elsewhere in `hamlet-core` fails review; future
  exceptions require a superseding ADR.
