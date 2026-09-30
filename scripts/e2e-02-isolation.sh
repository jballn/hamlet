#!/usr/bin/env bash
# e2e-02-isolation.sh — QA prompt 02/05, isolation & escape harness for the
# Sprint 02 container runtime (SPEC §4 isolation invariants, §11.4 secrets;
# CONVENTIONS §5 containers, §9 testing doctrine).
#
# Layers:
#   T1  FakeRuntime adversarial suite (always runs; no podman needed):
#         crates/hamlet-daemon/tests/adversarial_isolation.rs
#   T2  Source-level ratchets: no container-API crates in the lockfile, the
#       daemon never touches .env, forbidden podman flags exist ONLY in the
#       podman.rs audit/test sections.
#   T3  Committed golden vectors are privilege-free and secret-free.
#   T4  Guarded real-podman escape suite (only when HAMLET_PODMAN_TESTS=1
#       AND podman is on PATH; subuid preflight first). Skips with a
#       recorded reason otherwise — ci.sh step 5 runs the full guarded
#       battery separately, so this harness never double-runs the cold
#       (~200 MB) bootstrap integrity test unless the operator asks.
set -euo pipefail
cd "$(dirname "$0")/.."

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  ok: %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
skip()  { printf '  SKIP: %s\n' "$*"; }

# --- T1: FakeRuntime adversarial isolation suite ----------------------------
step "T1: adversarial isolation suite (FakeRuntime; SPEC §4/§11.4)"
if T1_OUT="$(cargo test -p hamlet-daemon --test adversarial_isolation --quiet 2>&1)"; then
  printf '%s\n' "$T1_OUT" | tail -3
  pass "adversarial_isolation: all tests green"
else
  printf '%s\n' "$T1_OUT"
  fail "adversarial_isolation suite"
fi

# --- T2: source-level ratchets ----------------------------------------------
step "T2: source-level infrastructure ratchets"

# No container-API/bindings crates anywhere in the dependency graph
# (CONVENTIONS §5: podman CLI through tokio pipes only — no libpod, no Docker).
API_CRATES="$(grep -E '^name = "(bollard|libpod|podman|podman-api|docker|docker_api|shiplift|bocker)"' Cargo.lock || true)"
if [ -z "$API_CRATES" ]; then
  pass "Cargo.lock: no podman/docker API crates"
else
  fail "Cargo.lock contains container-API crates: $API_CRATES"
fi

# The daemon never reads .env (SPEC §11.4: secrets are injected at session
# start by the daemon's env layer, never by provisioning/bootstrap).
ENV_HITS="$(grep -rn 'env_file()' crates/hamlet-daemon/src || true)"
if [ -z "$ENV_HITS" ]; then
  pass "hamlet-daemon src: zero env_file() references"
else
  fail "daemon touches .env: $ENV_HITS"
fi

# Forbidden-flag ratchet: privilege/host-namespace literals may only exist
# in the podman.rs audit + injection-matrix sections (and tests). A new
# product file mentioning them fails this gate until reviewed.
PRIV_HITS="$(grep -rln -- '--privileged\|--userns=host\|--pid=host\|--network=host\|--ipc=host' crates/*/src || true)"
for f in $PRIV_HITS; do
  if [ "$f" = "crates/hamlet-daemon/src/podman.rs" ]; then
    pass "forbidden-flag literals confined to podman.rs (audit/ratchet)"
  else
    fail "forbidden-flag literal outside podman.rs: $f"
  fi
done
if [ -z "$PRIV_HITS" ]; then
  fail "ratchet broken: podman.rs audit no longer mentions the forbidden flags"
fi

# --- T3: golden vectors are privilege-free and secret-free -------------------
step "T3: committed golden vectors"
GOLDEN_BAD="$(grep -rn -- '--privileged\|--userns\|\.sock\|BEGIN [A-Z ]*PRIVATE KEY\|sk-[A-Za-z0-9]\{8\}' crates/*/tests/golden || true)"
if [ -z "$GOLDEN_BAD" ]; then
  pass "goldens: no privilege flags, sockets, or secret material"
else
  fail "golden vectors contain forbidden material:"
  printf '%s\n' "$GOLDEN_BAD"
fi

# --- T4: guarded real-podman escape suite ------------------------------------
step "T4: guarded podman escape suite (SPEC §4)"
if [ "${HAMLET_PODMAN_TESTS:-0}" != "1" ]; then
  skip "HAMLET_PODMAN_TESTS != 1 (export it on a podman-capable host)"
elif ! command -v podman >/dev/null 2>&1; then
  skip "podman not on PATH"
else
  # Preflight: ADR-0011 needs a subuid range covering all 1000 slots, and
  # podman must be rootless. Fail loudly here — a guarded suite that fails
  # mid-run wastes a ~cold image pull.
  USER_NAME="$(id -un)"
  SUBUID_LINE="$(grep "^${USER_NAME}:" /etc/subuid 2>/dev/null || true)"
  if [ -z "$SUBUID_LINE" ]; then
    fail "no /etc/subuid entry for ${USER_NAME} — ADR-0011 subuid chown cannot work"
  else
    SUBUID_COUNT="$(echo "$SUBUID_LINE" | head -1 | cut -d: -f3)"
    if [ "$SUBUID_COUNT" -ge 2001 ]; then
      pass "/etc/subuid range for ${USER_NAME}: ${SUBUID_COUNT} ids (>= 2001)"
    else
      fail "/etc/subuid range too small: ${SUBUID_COUNT} < 2001 (slots 1001-2000 must map)"
    fi
  fi
  ROOTLESS="$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null || echo unknown)"
  if [ "$ROOTLESS" = "true" ]; then
    pass "podman is rootless"
  else
    fail "podman reports rootless=${ROOTLESS} (SPEC §4: rootless only)"
  fi
  if cargo test -p hamlet-daemon --test escape_guarded -- --ignored podman --nocapture; then
    pass "escape_guarded suite"
  else
    fail "escape_guarded suite"
  fi
fi

if [ "$FAILURES" -eq 0 ]; then
  printf '\n\033[1;32me2e-02-isolation: PASS\033[0m\n'
else
  printf '\n\033[1;31me2e-02-isolation: FAIL (%d)\033[0m\n' "$FAILURES"
  exit 1
fi
