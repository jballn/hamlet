---
role: ci-agent
sprint: 02
step: 6
---

# 02/06 — Sprint 02 gate

Precondition: HANDOFF.md shows `GATE 02/05: PASS`. If FAIL, stop and report.

Run `scripts/ci-local.sh`; then, if podman is available on this host, run
the guarded suite: `HAMLET_PODMAN_TESTS=1 cargo test --workspace -- --ignored`.
Verify ADR-0006/0007 match implementation; verify no `--privileged` or
socket-mount string appears anywhere in the daemon crate
(`rg -n "privileged|podman.sock|docker.sock" crates/hamlet-daemon` must be
empty except tests asserting their absence).

Report PASS/FAIL table into HANDOFF.md ending `GATE 02/06: PASS|FAIL`.
