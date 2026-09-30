---
role: ci-agent
sprint: 08
step: 5
---

# 08/05 — Sprint 08 gate

Precondition: HANDOFF.md shows `GATE 08/04: PASS`. If FAIL, stop and report.

Run `scripts/ci-local.sh`; guarded podman suite; both e2e scripts
(`e2e-commission.sh`, `e2e-village-demo.sh`). Verify ADR-0014 matches
implementation.

PASS/FAIL table into HANDOFF.md ending `GATE 08/05: PASS|FAIL`.
