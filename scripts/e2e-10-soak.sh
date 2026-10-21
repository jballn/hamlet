#!/usr/bin/env bash
# e2e-10-soak.sh — E2E harness hook for Sprint 10 Step 02 Soak & Resumability
# Marathon (SPEC §13; CONVENTIONS §2, §3, §9).
# Sourced and executed by scripts/ci.sh in the e2e battery phase.
set -euo pipefail
cd "$(dirname "$0")/.."

exec ./scripts/soak-village.sh
