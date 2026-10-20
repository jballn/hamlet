# ADR-0039 — Stasis formats and census: patron schema, PROPRIETOR grammar, home manifest, TITLE lineage

Status: Accepted
Sprint: 07-hibernation-census · Prompt: 01-architect (step 01)
Refs: SPEC §10 (census, hibernation, convergence), §14; CONVENTIONS §2 (purity seam), §3 r1/r4 (lineage timestamps); ADR-0002 (SafeName), ADR-0007 (symlink refusal), ADR-0019 cl. 1 (LF-only boundaries), ADR-0038 (DEED promotion)

## Context

SPEC §10 establishes stasis pools (`tavern/patrons/`) and tenant leases (`keep/census/parcel<uid>/`), defining the dual-state convergence at commission close: DEED environment changes evolve blueprints, while PROPRIETOR intelligence changes persist as hibernated patrons.
SPEC §14 delegates stasis formats and schema definitions to Sprint 07.
Prior sprints established the `Stasis` trait seam (ADR-0029) and `parcels[uid].stasis` charter flag. This ADR locks normative schemas for patron records, PROPRIETOR.md metadata frontmatter, home integrity manifests, append-only TITLE.md lineage, and dual-state convergence tracking.

## Decision

1. **Patron record layout.** Each hibernated patron occupies `tavern/patrons/PROPRIETOR<name>/` (`<name>` is a `SafeName`). It contains: (a) `PROPRIETOR.md` (intelligence profile), (b) `home.uid.bak/` (backed-up user home), and (c) `manifest.json` (backup integrity manifest).
2. **PROPRIETOR.md grammar.** Strictly LF-only, size-capped (≤4 KiB frontmatter, ≤64 KiB body). Begins with `---` frontmatter containing canonical key-values (`patron`, `register`, `blueprint`, `blueprint_version`, `source_uid`, `commission`, `tags`, `stasis_version`, `created_at`), followed by closing `---`, followed by freeform markdown intelligence profile.
3. **Backup manifest.** Canonical sorted-key JSON at `manifest.json` under `PROPRIETOR<name>/`. Records top-level entries of `home.uid.bak/` with `entry_type` (file/directory/symlink), `digest` (FNV-1a/64 content digest), `size_bytes`, and POSIX `mode`. On awakening, the daemon validates top-level entries against this manifest before mounting.
4. **TITLE.md grammar.** Append-only markdown headed `# TITLE — parcel<uid>` (or `manor`). Each stamped entry is a single line: `- [<timestamp>] <kind> [key=val ...]`. Normative entry kinds: `trigger`, `provision`, `session`, `crash`, `steer`, `promotion`, `hibernate`, `awaken`. Timestamps are lineage data only (never state inputs, CONVENTIONS §3.4). Non-append mutations (edited/deleted prior entries) are detected via prefix matching and content digests.
5. **Dual-state convergence.** Environment promotion (DEED → blueprint) and intelligence promotion (PROPRIETOR → patron) are tracked independently via `DualConvergenceReport`. A failure in one branch does not block or poison the other; failures are tracked and surfaced for Mayor/human escalation.

## Consequences

- Pure types and parsers implemented in `hamlet-core::stasis` with byte-exact golden tests.
- Symlinks and unparseable entries fail closed during patron awakening.
- Manor home is exempt from hibernation; its lineage is recorded at `keep/census/manor/TITLE.md`.
