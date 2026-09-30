---
role: qa-engineer
sprint: 06
step: 7
---

# 06/07 — Adversarial lifecycle suite + e2e script

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §9, ADR-0012, HANDOFF.md
sprint 06 (all steps).

## Deliverable 1 — committed adversarial suite

1. **Git mediation**: corrupt/truncated bundles, foreign-namespace branches,
   forged author identity (author != slot UID), oversized bundles, hook
   smuggling in bundles, non-fast-forward attempts — all rejected; bare repo
   history never corrupted (fsck after every rejection round).
2. **DEED promotion**: injection lines (shell metachars, newlines, path
   separators), unknown tool kinds, dup-set idempotence, DEED rewritten
   wholesale by a rogue agent (daemon detects non-append mutation →
   reject + trace), kill -9 mid-promotion → blueprint intact.
3. **Vault loop**: approve decree referencing nonexistent freight path;
   artifact name traversal attempts; checksum mismatch on copy (simulate);
   double-approve replay; reject-loop bound → escalation fires exactly once.
4. **State machine**: illegal transitions refused (closed → active etc.);
   two commissions never cross-contaminate (rosters, repos, vaults,
   archives).
5. **Restart resumability**: full commission mid-flight (indenture posted,
   one bundle pending, one submission pending) → daemon kill → restart →
   everything resumes, nothing duplicated.

## Deliverable 2 — `scripts/e2e-commission.sh`

init tempdir village → daemon start → commission new → FakeAgent mayor
decomposes → FakeAgent workers (branch + bundle + DEED + freight
deliverable) → approval → vault artifact asserted → commission close →
archives asserted. Deterministic, podman-optional (guarded variant runs the
same flow with real containers + scripted pi or FakeAgent-in-container).

Findings → HANDOFF.md. No product fixes. Verdict: `GATE 06/07: PASS|FAIL`.
