---
role: ci-agent
sprint: 01
step: 6
---

# 01/06 — Sprint 01 gate

Precondition: HANDOFF.md shows `GATE 01/05: PASS`. If FAIL, stop and report.

Run the full battery: `scripts/ci-local.sh` (fmt, clippy -D warnings,
workspace tests, check-deps). Verify ADRs 0002–0005 exist and match the
code they ratify. Verify the QA suite from step 5 is committed and green.

Report a single PASS/FAIL table into HANDOFF.md ending with
`GATE 01/06: PASS` or `GATE 01/06: FAIL` + minimal failing output.
