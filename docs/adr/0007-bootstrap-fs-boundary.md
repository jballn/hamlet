# ADR-0007 — Bootstrap filesystem boundary: never follow, never adopt

Status: Accepted
Date: Sprint 01 (QA fix pass)
Amends: ADR-0003 (`ensure`'s `.env` clause)
Refs: SPEC §2, §11.4; CONVENTIONS §6; QA findings F01-02/F01-03/F01-05/F01-06

## Context

QA found `hamlet init` / `HamletTree::ensure` operating THROUGH hostile
`.hamlet/` entries: (F01-02, security) a symlinked `.env` turned init into
a mode oracle — an external 0644 file was chmod'd 0600 through the link —
and a dangling link made `File::create` write OUTSIDE the tree; (F01-06) a
directory squatting `.env` was silently adopted (`len > 0` = "has
content"), leaving the village without a usable secrets file; (F01-03) a
`.hamlet/` populated by a foreign app was silently adopted as a village;
(F01-05) existing JSON artifacts kept tampered modes across re-init,
contradicting the step-02 binding note ("canonical modes on every run").

## Decision

1. `ensure` classifies `.env` via `symlink_metadata`: regular file → chmod
   0600 (content preserved); absent → `OpenOptions::create_new`
   (`O_CREAT|O_EXCL` — never follows a link planted mid-call); ANY other
   file type (symlink/dir/device) → typed refusal. It never opens,
   creates, or chmods through a non-regular path.
2. `hamlet init` classifies ALL THREE artifacts the same way (lstat →
   Missing | Empty | Regular): non-regular → typed `InitError::Io`
   refusal. Regular artifacts are never rewritten — only chmod'd to their
   canonical mode (0644) on every run; `.env` stays 0600 via ensure.
3. A present village root must contain ONLY normative top-level entries —
   the allowlist is derived from the typed tree API (`static_dirs()` first
   components + the three artifact file names), never hand-maintained.
   Foreign entries → typed refusal BEFORE anything is created inside;
   foreign content is never touched. A root that is not a real directory
   (regular file or symlink) is refused likewise.

## Consequences

- Init is safe against hostile-prepared `.hamlet/` state: refusals are
  loud, typed, and non-destructive. Partial trees of purely normative
  entries still complete; idempotent re-init is unchanged (pinned by
  golden + adversarial suites).
- Stale temp litter inside the village root counts as foreign → init
  refuses until it is cleaned (fail-hard; charter saves sweep their own
  legacy temp per ADR-0006).
- Runtime code inherits the duty: lazy per-identity destinations must
  still be lstat'd before writes (QA forward note, sprints 06/07).
