#!/usr/bin/env bash
# guarded-entry.sh — the single podman-capable-job entry point (CI config).
#
# WHY THIS EXISTS. The guarded battery (ADR-0032 cl. 4) can only assert the
# SPEC §4 per-slot uid policy on a host that can *realise* it, and the shape
# that realises it is the dedicated service user running `podman unshare`.
# Until now every harness asked the same question in its own words —
# "is podman on PATH?" / "is HAMLET_PODMAN_TESTS=1?" — and answered "skip".
# A skip is not a pass (ADR-0032 cl. 5), but a skip whose *remedy* lives in a
# different script than the caller is a skip nobody runs. So this file is the
# one place that decides, and it decides by measurement:
#
#   1. are we already the service user?          -> run the battery here
#   2. does the CURRENT user satisfy --doctor?   -> run the battery here
#   3. can we become the service user without a  -> doctor the service user,
#      password prompt (sudo -n)?                   then run it there
#   4. otherwise                                   -> SKIP WITH THE EXACT
#      COMMAND, exit 3 (not 0: a vacuous green is the lie this repo keeps
#      refusing, CI04-01 doctrine)
#
# Step 3 is the point of the whole file: on a host where the developer is in
# `wheel` (as this one is), `sudo -n` is the difference between "the guarded
# battery was skipped" and "the guarded battery ran as hamlet".
#
# Exit codes: 0 = battery green (or legitimately, provably, not applicable);
# 1 = a suite genuinely failed; 2 = misuse; 3 = skipped, remedy printed.
# Host/CI residual only — no product code.
set -euo pipefail
cd "$(dirname "$0")/.."

# The lock and the decision helpers live with the other CI callers, so there is
# exactly one implementation of "one guarded job per host" (QA05-09).
. ./scripts/ci-guarded-job.sh

SERVICE_USER="${HAMLET_SERVICE_USER:-hamlet}"
SUITE_SCRIPT="$PWD/scripts/guarded-service-user.sh"
# The documented disk-backed work area (ADR-0032 cl. 1 / QA04-08). Only used
# when we hand off to another user; the caller keeps its own TMPDIR.
WORK_AREA="${HAMLET_WORK_AREA:-/var/hamlet/tmp}"

doctor_only=0
for a in "$@"; do
  case "$a" in
    --doctor) doctor_only=1 ;;
    -h|--help)
      sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "guarded-entry: unknown argument '$a'" >&2; exit 2 ;;
  esac
done

[ -x "$SUITE_SCRIPT" ] || { echo "guarded-entry: $SUITE_SCRIPT missing"; exit 2; }

yellow=$'\033[1;33m'; off=$'\033[0m'
note() { printf '  %s\n' "$*"; }
skip_with_remedy() { # skip_with_remedy <why> <command...>
  printf '\n%sSKIP: guarded battery not run — %s%s\n' "$yellow" "$1" "$off"
  note "The guarded suites therefore asserted NOTHING. This is a coverage hole,"
  note "not a pass (ADR-0032 cl. 5). The one command that closes it:"
  shift
  printf '\n    %s\n\n' "$*"
  exit 3
}

# --- 0. is there a battery to run at all ------------------------------------
if ! command -v podman >/dev/null 2>&1; then
  skip_with_remedy "podman is not on PATH on this host (nothing guarded can run)" \
    "install podman, then: sudo -H -u ${SERVICE_USER} env TMPDIR=${WORK_AREA} HAMLET_PODMAN_TESTS=1 ./scripts/guarded-service-user.sh"
elif ! podman info >/dev/null 2>&1; then
  skip_with_remedy "podman is on PATH but unusable as $(id -un) (QA04-04: PATH presence is not usability)" \
    "sudo -H -u ${SERVICE_USER} env TMPDIR=${WORK_AREA} HAMLET_PODMAN_TESTS=1 ./scripts/guarded-service-user.sh --doctor   # measures it"
fi

if [ "$doctor_only" = "1" ]; then
  # Pure diagnosis: always as the user the battery would actually run as.
  if [ "$(id -un)" = "$SERVICE_USER" ]; then
    exec "$SUITE_SCRIPT" --doctor
  fi
  if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
    exec sudo -n -H -u "$SERVICE_USER" env "TMPDIR=${WORK_AREA}" "$SUITE_SCRIPT" --doctor
  fi
  skip_with_remedy "cannot reach ${SERVICE_USER} non-interactively; doctoring from $(id -un) would measure the wrong user" \
    "sudo -H -u ${SERVICE_USER} env TMPDIR=${WORK_AREA} ./scripts/guarded-service-user.sh --doctor"
fi

# --- 0b. one battery per host (QA05-09) ------------------------------------
# Container names in the guarded suites are GLOBAL per podman user (QA04-20:
# two parallel tests in one binary collided on `podman run --name`, and the
# second one's cleanup deletes the first one's container). The same collision
# happens between two CONCURRENT batteries on one host — and it is worse,
# because the victim sees a container evaporate mid-assertion:
#
#   podman exec qa-escape-integrity ... -> "no container with ID <id> found in
#   database: no such container"
#
# which reads exactly like a product defect and is not one. Measured on this
# host on 2026-10-04: two batteries launched 90 s apart produced precisely
# that red in `escape_guarded`, while both batteries' other suites were green.
# So the battery takes the shared lock, and a second one declines to start with
# the holder named — a skip with a remedy, never a mystery red.
battery_lock() {
  ci_guarded_lock && return 0
  ci_guarded_lock_or_report || true
  return 3
}

# --- 1. already the service user -------------------------------------------
if [ "$(id -un)" = "$SERVICE_USER" ]; then
  note "running as the service user ${SERVICE_USER} — doctor first, then the battery:"
  "$SUITE_SCRIPT" --doctor || note "doctor reported blockers above; the battery will record them as SKIPs"
  battery_lock || skip_with_remedy "a guarded battery is already running on this host (see above)" \
    "wait for it to finish, then re-run this script"
  exec env HAMLET_PODMAN_TESTS=1 "$SUITE_SCRIPT"
fi

# --- 2. can the current user do it without switching? -----------------------
# Ask the doctor, not a guess: a developer user with a subuid range, cgroup v2
# and a disk-backed TMPDIR is a perfectly good battery host (ADR-0032 cl. 1
# records the skip for the user who CANNOT do it, not for everyone who is not
# `hamlet`).
if doctor_out="$("$SUITE_SCRIPT" --doctor 2>&1)"; then
  printf '%s\n' "$doctor_out" | sed 's/^/  /'
  note "current user $(id -un) satisfies every precondition — running the battery here"
  battery_lock || skip_with_remedy "a guarded battery is already running on this host (see above)" \
    "wait for it to finish, then re-run this script"
  exec env HAMLET_PODMAN_TESTS=1 "$SUITE_SCRIPT"
fi

printf '%s\n' "$doctor_out" | sed 's/^/  /'
note "current user $(id -un) cannot run the guarded battery (blockers above)."

# --- 3. become the service user without a password prompt ------------------
if [ "$(id -u)" != "0" ] && command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
  note "sudo -n works — handing the battery to ${SERVICE_USER}:"
  battery_lock || skip_with_remedy "a guarded battery is already running on this host (see above)" \
    "wait for it to finish, then re-run this script"
  exec sudo -n -H -u "$SERVICE_USER" env "TMPDIR=${WORK_AREA}" \
    HAMLET_PODMAN_TESTS=1 HAMLET_TARGET_DIR="${HAMLET_TARGET_DIR:-$PWD/target/debug}" \
    "$SUITE_SCRIPT"
fi

# --- 4. the honest skip ----------------------------------------------------
if [ "$(id -u)" = "0" ]; then
  skip_with_remedy "running as root; the battery is rootless-only" \
    "sudo -H -u ${SERVICE_USER} env TMPDIR=${WORK_AREA} HAMLET_PODMAN_TESTS=1 ./scripts/guarded-service-user.sh"
fi
skip_with_remedy "sudo needs a password for ${SERVICE_USER}, and a CI job cannot type one" \
  "sudo -H -u ${SERVICE_USER} env TMPDIR=${WORK_AREA} HAMLET_PODMAN_TESTS=1 ./scripts/guarded-service-user.sh"