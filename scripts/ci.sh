#!/usr/bin/env bash
# ci.sh — the hamlet verification battery (no GitHub remote on this repo, so
# this script IS the CI; a git pre-push hook runs it. See HANDOFF.md).
# ci-local.sh is a thin alias so both entry points stay identical.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

step "[1/5] cargo fmt --all --check"
cargo fmt --all --check

step "[2/5] cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

step "[3/5] cargo test --workspace"
cargo test --workspace

step "[4/5] scripts/check-deps.sh"
./scripts/check-deps.sh

step "[5/5] guarded podman tests"
if command -v podman >/dev/null 2>&1; then
  # Runs #[ignore]d tests matching "podman"; a no-op (exit 0) until such
  # tests exist, which is fine — absence must not fail the battery.
  HAMLET_PODMAN_TESTS=1 cargo test --workspace -- --ignored podman
else
  echo "SKIP: podman not available on this host."
fi

# E2E scripts (CONVENTIONS §9): run every scripts/e2e-*.sh that exists.
shopt -s nullglob
e2e=(scripts/e2e-*.sh)
if [ ${#e2e[@]} -gt 0 ]; then
  for s in "${e2e[@]}"; do
    step "[e2e] $s"
    "$s"
  done
fi

printf '\n\033[1;32mCI battery: PASS\033[0m\n'
