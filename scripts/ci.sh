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
# QA04-04 residual: PATH presence is not usability. A half-working podman
# (present but with no runtime/userns support) turned a legitimate SKIP into an
# environmental RED. Probe it for real, and record why we skip.
podman_ready=""
if command -v podman >/dev/null 2>&1; then
  if podman info --format '{{.Host.OCIRuntime.Name}}' >/dev/null 2>&1; then
    podman_ready=yes
  else
    echo "SKIP: podman is on PATH but not usable here (podman info failed):"
    podman info 2>&1 | sed 's/^/    /' | head -20
  fi
else
  echo "SKIP: podman not available on this host."
fi
failures=()
record() { failures+=("$1"); printf '\n\033[1;31mFAIL: %s\033[0m\n' "$1"; }

if [ -n "$podman_ready" ]; then
  # Runs #[ignore]d tests matching "podman"; a no-op (exit 0) until such
  # tests exist, which is fine — absence must not fail the battery.
  #
  # Deliberately NOT fatal (QA04-08 carry-forward): under `set -e` a red
  # step 5 aborted the script here, so the e2e loop below NEVER RAN and a
  # host problem in the guarded container battery silently suppressed the
  # entire non-container gate. Each step records its own verdict and the
  # script aggregates, so a host-shaped RED can no longer hide product
  # regressions (or vice versa). The battery still exits non-zero.
  if HAMLET_PODMAN_TESTS=1 cargo test --workspace -- --ignored podman; then
    :
  else
    record "[5/5] guarded podman tests (see the log above; check QA04-07/08/09 before reading this as a product defect)"
  fi
fi

# E2E scripts (CONVENTIONS §9): run every scripts/e2e-*.sh that exists.
shopt -s nullglob
e2e=(scripts/e2e-*.sh)
if [ ${#e2e[@]} -gt 0 ]; then
  for s in "${e2e[@]}"; do
    step "[e2e] $s"
    if ! "$s"; then
      record "[e2e] $s"
    fi
  done
fi

if [ ${#failures[@]} -gt 0 ]; then
  printf '\n\033[1;31mCI battery: FAIL (%d)\033[0m\n' "${#failures[@]}"
  for f in "${failures[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi

printf '\n\033[1;32mCI battery: PASS\033[0m\n'
