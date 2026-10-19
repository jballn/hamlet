# ADR-0038 — DEED `### Dynamic Alterations` grammar and promotion request schema

Status: Accepted
Sprint: 06-commission-lifecycle · Prompt: 01/05-architect (step 01)
Refs: SPEC §9.4 (promotion), §5 (blueprint immutability); CONVENTIONS §2
(purity seam — parsers in core, execution in daemon), §3 r1/r4; ADR-0009
(blueprint schema + validators), ADR-0019 cl. 1 (LF-only text at boundaries),
ADR-0026 cl. 5 (ApprovePromotion decree fields)

## Context

SPEC §9.4: a commission's DEED.md may carry `### Dynamic Alterations` — tool
changes the agents earned during execution — and convergence can promote them
into the blueprint. `blueprint.rs` already reserved this path explicitly: a
loaded blueprint is immutable, and "the only mutation path is … the daemon
parses DEED.md `### Dynamic Alterations` and constructs a *new* `Blueprint`
value with a bumped `meta.version`. In-place mutation is unrepresentable by
construction."

This ADR pins what step 06 executes: (1) the grammar of the alterations block,
parseable in pure core; (2) how parsed alterations compose with an existing
blueprint (upsert semantics, version bump); (3) the `PromotionRequest` shape
that names *what* gets promoted and where it must come from — mirroring the
freight rules the decree side already enforces.

## Decision

1. **Grammar.** A DEED.md contains at most one section headed exactly
   `### Dynamic Alterations`; zero sections is a legal no-op (not an error),
   two is an error. Inside the section (until the next heading or EOF) only
   blank lines and alteration lines are legal:

   ```
   - <kind> <name>[ = <version_spec>]
   ```

   where `kind ∈ {pixi, nvm, cargo, apk}` and `name` / `version_spec` reuse the
   blueprint's own charset validators (`tool_name_re`, `version_spec_re`) and
   caps (64 / 128) — one vocabulary for "what counts as a tool", no second
   standard. A duplicate `(kind, name)` inside one section is an error with the
   line number; errors are line-numbered throughout (the file is model-written;
   strict parsing beats tolerant guessing). LF-only, as at every text boundary.

2. **Upsert semantics.** Applying alterations to a blueprint: an existing
   `(kind, name)` gets its `version_spec` replaced; unknown tools are appended
   in declaration order; `meta.version` increments by one (refused at
   `u32::MAX`). The result is a *fresh* immutable `Blueprint` value built
   through a new internal constructor — the immutability invariant of
   ADR-0009 holds, promotion is replacement not mutation.

3. **PromotionRequest.** `{ commission: u64, freight_path: TreeRelPath,
   artifact_name: SafeName }` — the transient request that names a deliverable
   for vault promotion. Validation mirrors the decree side's freight rules so a
   request cannot out-run an `ApprovePromotion` decree: path parses as a tree
   rel-path, sits under `courier/freight/`, has ≥ 3 components (a file, not the
   directory), and the artifact name sanitizes. The request itself is transient
   executor input — not charter state, no serde persistence.

4. **Execution stays daemon-side.** Core parses and validates; step 06's
   executor copies the artifact into the vault and executes the
   `ApprovePromotion` decree (ADR-0026 cl. 5). No new wake kind: promotion
   rides the existing decree pipeline.

## Consequences

- New core module `promote`: DEED section parser, alteration upsert, request
  type — all pure, all unit-tested with adversarial inputs.
- `blueprint.rs` gains one internal constructor (private fields, pub(crate)
  path); public API unchanged.
- The DEED file remains model-owned text; the daemon never writes it, only
  reads it at convergence.
