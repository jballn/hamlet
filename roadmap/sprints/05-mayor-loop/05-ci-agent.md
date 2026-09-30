---
role: ci-agent
sprint: 05
step: 5
---

# 05/05 — Sprint 05 gate

Precondition: HANDOFF.md shows `GATE 05/04: PASS`. If FAIL, stop and report.

Run `scripts/ci-local.sh`; guarded podman suite if available (incl. manor
provision smoke). Verify ADR-0011 matches implementation; verify
`defaults/MAYOR.md` ships and is referenced by provisioning.

PASS/FAIL table into HANDOFF.md ending `GATE 05/05: PASS|FAIL`.
