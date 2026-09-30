#!/usr/bin/env bash
# ci-local.sh — local entry point for the verification battery.
# Runs the exact same steps as scripts/ci.sh (it delegates to it).
set -euo pipefail
exec "$(dirname "$0")/ci.sh" "$@"
