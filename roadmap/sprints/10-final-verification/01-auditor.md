---
role: auditor
sprint: 10
step: 1
---

# 10/01 — SPEC conformance audit

Read first: `roadmap/CONVENTIONS.md`, then `SPEC.md` **line by line**, all
ADRs (`docs/adr/`), all sprint HANDOFF.md files.

Produce `docs/audit/conformance-matrix.md`:

| SPEC §  | Requirement (one row per normative statement) | Implementation (file:line) | Proving test (name/path) | Verdict (CONFORMS / GAP / DIVERGENCE / N-A-stage) |

Hunt specifically for silent divergences:
- §2 tree paths vs. actual `hamlet-core::tree` accessors (every path).
- §3 crate boundaries: any I/O or process spawn leaking into hamlet-core;
  any orchestration state in sled; any tree access from cli/tui/web.
- §4 mount table + isolation invariants vs. guarded tests that exist.
- §6 sequential delivery + interrupt distinguishability.
- §7 zero-token sleep, single-turn packets, decrees-as-files-only.
- §9 promotion loops (DEED→blueprint, freight→vault) vs. SPEC wording.
- §10 dual-state independence; slot permanence.
- §11 atomicity, sanitizer coverage (every externally-derived name),
  secrets hygiene.
- §12 CLI verbs complete; staged-surface parity (CLI/TUI/web show the same
  projections).
- §14: every [DEV] item has a numbered ADR; ADRs match code.

Report gaps — do NOT fill them. Prioritized findings list into HANDOFF.md.
Verdict line: `AUDIT 10/01: CLEAN|<n> FINDINGS`.
