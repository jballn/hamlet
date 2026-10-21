#!/usr/bin/env bash
# ci-local.sh — local entry point for the verification battery.
# Runs the exact same steps as scripts/ci.sh (it delegates to it),
# preceded by the WASM web distribution build (Sprint 09).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Disk-backed work area for guarded podman battery (QA04-08)
if [ -d "/var/hamlet/tmp" ]; then
  export TMPDIR="${TMPDIR:-/var/hamlet/tmp}"
fi
export HAMLET_PODMAN_TESTS="${HAMLET_PODMAN_TESTS:-1}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-1}"

# Sprint 09: build-web.sh wasm build
if ! "$ROOT/scripts/build-web.sh"; then
  printf '\n\033[1m== CI battery ==\033[0m\n'
  printf '  \033[1;31mFAIL\033[0m     scripts/build-web.sh failed\n'
  printf '\n\033[1;31mCI battery: FAIL (1)\033[0m\n'
  exit 1
fi

# QA04-31: the gate must never die without a verdict. A syntax error in the
# FIRST lines of ci.sh is unreachable even by ci.sh's own self-check (bash has
# not read that far yet), so check the whole file here, before exec.
if ! bash -n "$ROOT/scripts/ci.sh" 2>/dev/null; then
  printf '\n\033[1m== CI battery ==\033[0m\n'
  printf '  \033[1;31mFAIL\033[0m     scripts/ci.sh has a syntax error (parse-corrupted)\n'
  printf '\n\033[1;31mCI battery: FAIL (1)\033[0m\n'
  exit 2
fi

exec "$ROOT/scripts/ci.sh" "$@"

