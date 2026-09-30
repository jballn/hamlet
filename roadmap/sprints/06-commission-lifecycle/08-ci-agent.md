---
role: ci-agent
sprint: 06
step: 8
---

# 06/08 — Sprint 06 gate

Precondition: HANDOFF.md shows `GATE 06/07: PASS`. If FAIL, stop and report.

Run `scripts/ci-local.sh`; guarded podman suite if available; run
`scripts/e2e-commission.sh` (fake mode mandatory, guarded mode when podman
present). Verify ADR-0012 matches implementation.

PASS/FAIL table into HANDOFF.md ending `GATE 06/08: PASS|FAIL`.
