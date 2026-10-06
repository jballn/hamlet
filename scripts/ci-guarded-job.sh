#!/usr/bin/env bash
# ci-guarded-job.sh — sourceable CI helper: "can this host run the guarded
# battery, and if not, who can?" (CI config; no product code).
#
# WHY A HELPER. Four callers want the same decision — scripts/ci.sh (step 5)
# and the e2e-02 / e2e-04 / e2e-05 harnesses, each of which carries one
# guarded real-podman section. They used to each ask the question in their own
# words and answer "skip", so the remedy (ADR-0032 cl. 4:
# scripts/guarded-service-user.sh, run as the dedicated service user) lived in
# a different file than the caller that needed it, and a skip nobody could
# action looked exactly like a skip nobody cared about.
#
# The decision, once, lives in scripts/guarded-entry.sh (which measures with
# guarded-service-user.sh --doctor and hands off via `sudo` when it can).
# These helpers are the thin caller-side wrapper so no harness re-implements
# the ladder — and so the *verdict vocabulary* is shared:
#
#   ci_guarded_preflight     is podman usable from here? (0/3)
#   ci_guarded_direct_viable could THIS user run a guarded suite directly?
#   ci_guarded_delegate      run the battery through the entry point.
#                           0 = green, 1 = failure, 3 = skipped-with-remedy.
#   ci_guarded_disk_audit    the disk-leak policy (scripts/village-reclaim.sh):
#                           stranded subuid-owned village trees, which only
#                           `podman unshare rm -rf` can reclaim.
#   ci_guarded_lock/_release one guarded job per host (see below).
#
# Source it:  . "$(dirname "$0")/ci-guarded-job.sh"   (callers cd to the root)
: "${HAMLET_CI_ENTRY:=scripts/guarded-entry.sh}"
: "${HAMLET_CI_RECLAIM:=scripts/village-reclaim.sh}"
: "${HAMLET_CI_DOCTOR:=scripts/guarded-service-user.sh}"

_ci_yellow=$'\033[1;33m'; _ci_off=$'\033[0m'

# Are we already positioned to run the battery without a user switch?
# 0 = yes (callers may run their own cargo test for their own suite),
# 3 = no, and the entry point has already printed the exact remedy.
ci_guarded_preflight() {
  command -v podman >/dev/null 2>&1 || return 3
  podman info >/dev/null 2>&1 || return 3
  return 0
}

# Could THIS user run THIS harness's guarded suite directly (the fast path)?
# Same three preconditions the suites themselves apply (ADR-0032 cl. 1/2):
# QA04-07 (podman usable + a subuid range), QA04-08 (disk-backed work area
# with >= 4 GiB free — the RAM-backed TMPDIR is what made these sections skip
# forever), and the explicit HAMLET_PODMAN_TESTS=1 opt-in.
# 0 = yes; 3 = no, and it prints which precondition failed.
ci_guarded_direct_viable() {
  local wa="${HAMLET_WORK_AREA:-${TMPDIR:-/tmp}}"
  [ "${HAMLET_PODMAN_TESTS:-0}" = "1" ] || { echo "HAMLET_PODMAN_TESTS != 1"; return 3; }
  command -v podman >/dev/null 2>&1 || { echo "podman not on PATH"; return 3; }
  podman info >/dev/null 2>&1 || { echo "podman on PATH but unusable as $(id -un)"; return 3; }
  grep -q "^$(id -un):" /etc/subuid 2>/dev/null \
    || { echo "no /etc/subuid entry for $(id -un) (ADR-0011 slot chown cannot work)"; return 3; }
  case "$(df --output=fstype -- "$wa" 2>/dev/null | tail -1)" in
    tmpfs|ramfs) echo "work area $wa is RAM-backed (QA04-08: the toolchain extract is OOM-killed there)"; return 3 ;;
  esac
  local avail; avail="$(df -B1 --output=avail -- "$wa" 2>/dev/null | tail -1 | tr -d ' ')"
  [ -n "$avail" ] && [ "$avail" -ge $((4 * 1024 * 1024 * 1024)) ] 2>/dev/null \
    || { echo "work area $wa has less than 4 GiB free (QA04-08)"; return 3; }
  return 0
}

# Run the WHOLE guarded battery as whoever can actually run it (ADR-0032
# cl. 4). Used where a harness would otherwise skip for lack of a capable
# user. Never converts a skip into a pass: rc 3 stays a skip.
# guarded_job_delegate <label-for-the-log>
ci_guarded_delegate() {
  local label="${1:-guarded battery}"
  local out rc=0
  # Captured, not streamed: ci.sh streams this same battery live, and a
  # harness reaching this path is by definition the rare one (the direct route
  # was closed), so a transcript is worth more here than a live tail.
  out="$(./"${HAMLET_CI_ENTRY}" 2>&1)" || rc=$?
  printf '%s\n' "$out" | sed 's/^/  /'
  case "$rc" in
    0)
      if printf '%s\n' "$out" | grep -q '^SKIP '; then
        printf '%s\n' "$out" | grep '^SKIP ' | sed 's/^/  SKIP: /'
        return 3   # ran, but some suites proved nothing — a coverage hole
      fi
      printf '  ok: %s green via scripts/guarded-entry.sh\n' "$label"
      return 0 ;;
    3) return 3 ;;
    *) return 1 ;;
  esac
}

# The disk-leak policy, as a gate line: stranded trees and free space in the
# work area. 0 = enough room to run; 1 = below the QA04-08 floor (report, do
# not abort — the caller's own accounting decides).
ci_guarded_disk_audit() {
  ./"${HAMLET_CI_RECLAIM}" --audit || return 1
}
# --- one guarded job per host (QA05-09) -------------------------------------
# Container names in the guarded suites are GLOBAL per podman user. QA04-20
# already collided two parallel tests inside one binary; the same collision
# between two CONCURRENT jobs on one host is worse, because the victim's
# container vanishes mid-assertion and the error reads like a product defect:
#
#   "Error: can only create exec sessions on running containers: container
#    state improper"   /   "no container with ID <id> found in database"
#
# Measured on this host (2026-10-04): a battery and a concurrent
# `cargo test --test bootstrap_guarded` produced exactly that red, and the same
# suites were green when run alone. So every CI-driven guarded invocation takes
# this lock. A human running `cargo test -- --ignored podman` by hand still
# bypasses it — that residual is recorded in HANDOFF (QA05-09).
: "${HAMLET_GUARDED_LOCK:=/tmp/hamlet-guarded-$(id -u).lock}"
ci_guarded_lock() {
  command -v flock >/dev/null 2>&1 \
    || { printf '  note: flock is not on this host — cannot serialise, proceeding unserialised (QA04-26 residual)\n'; return 0; }
  # Append mode: never truncate another job's record. Refuse rather than risk
  # a collision when the lock file itself is unreachable (QA04-26): an unlocked
  # second run is exactly what destroys the first — a refusal with a recorded
  # reason beats a silent proceed.
  exec 9>>"$HAMLET_GUARDED_LOCK" 2>/dev/null \
    || { printf '  cannot open the guarded-job lock file %s — refusing rather than risk a collision (QA04-26)\n' "$HAMLET_GUARDED_LOCK"; return 3; }
  flock -n 9 || return 3
  # $BASHPID, not $$: inside a subshell (e.g. the e2e-07 test holder) $$ is
  # the parent script's pid — it would name the wrong holder in the refusal.
  printf 'pid=%s user=%s work_area=%s started=%s\n' \
    "$BASHPID" "$(id -un)" "${TMPDIR:-<unset>}" "$(date -Is)" >&9
  return 0
}
ci_guarded_lock_release() {
  flock -u 9 2>/dev/null || true
  exec 9>&- 2>/dev/null || true
}
# Verbose form for callers that must explain a refusal. Only LIVE holders are
# named: the lock file is append-only and outlives the jobs that wrote it, so
# printing it raw would blame a pid that exited an hour ago.
ci_guarded_lock_or_report() {
  ci_guarded_lock && return 0
  printf '  another guarded job already holds %s:\n' "$HAMLET_GUARDED_LOCK"
  awk '{print $1}' "$HAMLET_GUARDED_LOCK" 2>/dev/null | sed 's/pid=//' \
    | while read -r p; do
        [ -n "$p" ] && kill -0 "$p" 2>/dev/null && grep "^pid=$p " "$HAMLET_GUARDED_LOCK" | tail -1
      done | sed 's/^/    /'
  printf '  guarded container names are global per user: a second job deletes the\n'
  printf "  first's containers mid-assertion (QA05-09). Not running concurrently.\n"
  return 3
}
