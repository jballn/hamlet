---
role: ci-agent
sprint: 09
step: 6
---

# 09/06 — Sprint 09 gate

Precondition: HANDOFF.md shows `GATE 09/05: PASS`. If FAIL, stop and report.

Run `scripts/ci-local.sh` (now including `scripts/build-web.sh` wasm build);
guarded podman suite; all e2e scripts (`e2e-commission.sh`,
`e2e-village-demo.sh`, `e2e-village-web.sh`). Verify ADR-0015 matches
implementation; record wasm artifact size in the report.

PASS/FAIL table into HANDOFF.md ending `GATE 09/06: PASS|FAIL`.
