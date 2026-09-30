---
role: ci-agent
sprint: 10
step: 4
---

# 10/04 — Release gate

Precondition: HANDOFF.md shows `AUDIT 10/01` resolved (CLEAN or all
findings fixed + re-audited) and `GATE 10/02: PASS`.

Run the FULL battery, fresh clone if possible:
1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. `scripts/check-deps.sh`
5. `scripts/build-web.sh`
6. guarded podman suite (`HAMLET_PODMAN_TESTS=1`) where available
7. `scripts/e2e-commission.sh`, `scripts/e2e-village-demo.sh`,
   `scripts/e2e-village-web.sh`, `scripts/soak-village.sh` (short mode)
8. secrets grep battery over the repo (no real-looking keys, no .env
   committed, token files absent)

Report the full PASS/FAIL table into HANDOFF.md ending
`GATE 10/04: PASS|FAIL`. On any FAIL: minimal failing output captured;
do not fix product code.
