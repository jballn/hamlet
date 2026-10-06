#!/usr/bin/env bash
# ci-local.sh — local entry point for the verification battery.
# Runs the exact same steps as scripts/ci.sh (it delegates to it).
set -euo pipefail

# QA04-31: the gate must never die without a verdict. A syntax error in the
# FIRST lines of ci.sh is unreachable even by ci.sh's own self-check (bash has
# not read that far yet), so check the whole file here, before exec.
if ! bash -n "$(dirname "$0")/ci.sh" 2>/dev/null; then
  printf '\n\033[1m== CI battery ==\033[0m\n'
  printf '  \033[1;31mFAIL\033[0m     scripts/ci.sh has a syntax error (parse-corrupted)\n'
  printf '\n\033[1;31mCI battery: FAIL (1)\033[0m\n'
  exit 2
fi

exec "$(dirname "$0")/ci.sh" "$@"
