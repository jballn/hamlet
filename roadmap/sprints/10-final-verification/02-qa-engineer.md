---
role: qa-engineer
sprint: 10
step: 2
---

# 10/02 — Soak + resumability marathon

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §13 (restart-resumability
contract), HANDOFF.md (audit findings — include them as soak assertions
where applicable).

Build and run `scripts/soak-village.sh` + a committed Rust soak harness:

1. **Multi-tenant load**: 3 concurrent commissions, 12+ agents (FakeAgent
   or real pi where available), interleaved mail storms, bundle pushes,
   submissions, promotions.
2. **Lifecycle churn**: hibernate/awaken cycles for every agent ≥ 3×;
   patron re-matching; blueprint version growth from DEED promotions.
3. **Chaos**: daemon kill -9 at 20 deterministic pseudo-random points
   (seeded); watcher event storms (10k file ops); WS client flapping;
   dispatch endpoint blackholes (retry/dead-letter exercised).
4. **Invariants asserted after every chaos point**: charter loads + is
   byte-stable on restart-with-no-activity; no orphan containers/slots/
   parcels; TITLE append-only unbroken; no secret material anywhere in
   tree/sled/logs (grep battery); vault artifacts checksum-valid; git repos
   fsck-clean.
5. **Duration target**: ≥ 30 minutes simulated village time (event-count
   based, deterministic) with zero invariant violations.

Findings → HANDOFF.md (file:line, repro seed, expected vs actual, SPEC
cite). No product fixes. Verdict: `GATE 10/02: PASS|FAIL`.
