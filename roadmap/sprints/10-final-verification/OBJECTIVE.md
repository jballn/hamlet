# Sprint 10 — Final Verification

**Goal:** Prove the implementation conforms to SPEC.md v1.0 end to end,
soak the governor under a full multi-commission lifecycle, clean the tree,
and cut the release candidate.

**Scope boundary:** Fixes only for audit/soak findings. No new features,
no new [DEV] decisions without a superseding ADR.

**Exit criteria:**
- Conformance matrix complete: every SPEC requirement → implementation
  location → proving test → verdict; zero unexplained gaps; all [DEV] items
  from SPEC §14 resolved by numbered ADRs (0002–0015 + any additions).
- Findings from the audit fixed and re-gated (fixes may be fed as an extra
  build prompt appended to this sprint — record in HANDOFF).
- Soak: multi-commission, multi-agent, hibernate/awaken cycles, daemon
  restarts at random points, watcher storms — zero state corruption, zero
  secret leaks, charter byte-stable across restart-with-no-activity.
- Cleanup pass: no dead code, no dbg/println leftovers, docs consistent.
- Full battery green; release candidate tagged `v0.1.0-rc1`.
- Branch merged, tag `sprint/10-final-verification`.

## Prompt sequence
1. `01-auditor.md` — spec conformance audit
2. `02-qa-engineer.md` — soak + resumability marathon
3. `03-cleanup-agent.md` — cleanup pass
4. `04-ci-agent.md` — release gate
5. `05-commit-agent.md` — merge, tag rc, release notes
