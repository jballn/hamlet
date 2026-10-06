#!/usr/bin/env bash
# ci.sh — the hamlet verification battery (no GitHub remote on this repo, so
# this script IS the CI; a git pre-push hook runs it. See HANDOFF.md).
# ci-local.sh is a thin alias so both entry points stay identical.
#
# ONE RULE, learned the hard way (QA04-08 carry-forward): no step may hide
# another. Every step records its own verdict, the loop always continues, and
# the battery prints one PASS/FAIL table at the end and exits non-zero if
# anything failed.
#
# QA04-31 — a gate that can die without printing a verdict is a lie. Every
# abort path in this script still ends in "CI battery: FAIL (n)":
#  * no `set -e` and NO `set -u`: both have death paths the ERR trap cannot
#    catch on bash 5.3 (verified 2026-10-04: an unbound-variable exit prints
#    nothing; a main-body syntax error exits with no trap at all). Instead,
#    every variable that can be absent has an explicit default below.
#  * the self `bash -n` check before anything else: a syntax error anywhere in
#    this file (the QA04-31 repro was one mid-run, from a concurrent edit) is
#    caught here; the e2e phase still runs and the verdict still prints, even
#    though steps 1-5 cannot. The check block stays self-contained on purpose:
#    nothing it calls may be defined later in this file.
#  * the ERR backstop (`on_err`): any OTHER unexpected failure — a sourced
#    helper dying, step 5 aborting — is recorded with its line number and the
#    battery CONTINUES to the e2e phase, which runs unconditionally after
#    step 5 and accumulates its failures into the same verdict.
# Regression-pinned by scripts/e2e-07-ci-abort.sh (both abort shapes).
set -o pipefail

cd "$(dirname "$0")/.." || {
  printf '\n\033[1m== CI battery ==\033[0m\n'
  printf '  \033[1;31mFAIL\033[0m     could not cd to the repo root\n'
  printf '\n\033[1;31mCI battery: FAIL (1)\033[0m\n'
  exit 1
}

# --- QA04-31: parse-death guard ---------------------------------------------
# Bash reads this file incrementally, so a syntax error in a later region
# (step 5, the e2e loop) kills the script with no verdict. Check the whole
# file first. When it is corrupted, steps 1-5 cannot run — but the e2e phase
# and the verdict still must; that is the QA04-31 claim.
if ! parse_err="$(bash -n "${BASH_SOURCE[0]:-$0}" 2>&1)"; then
  printf '\n\033[1;31mci.sh is parse-corrupted — steps after the error cannot run:\033[0m\n'
  printf '%s\n' "$parse_err" | sed 's/^/  /' | head -5
  rows=("FAIL|scripts/ci.sh (syntax error — steps 1-5 not run)")
  n=1
  shopt -s nullglob
  for s in scripts/e2e-*.sh; do
    printf '\n\033[1m==> [e2e] %s\033[0m\n' "$s"
    if ./"$s"; then
      rows+=("PASS|$s"); printf '  \033[32mPASS\033[0m %s\n' "$s"
    else
      rows+=("FAIL|$s"); n=$((n + 1)); printf '  \033[1;31mFAIL\033[0m %s\n' "$s"
    fi
  done
  printf '\n\033[1m== CI battery ==\033[0m\n'
  for r in "${rows[@]}"; do
    case "$r" in
      FAIL*) printf '  \033[1;31mFAIL\033[0m     %s\n' "${r#*|}" ;;
      *)     printf '  \033[32mPASS\033[0m     %s\n' "${r#*|}" ;;
    esac
  done
  printf '\n\033[1;31mCI battery: FAIL (%d)\033[0m\n' "$n"
  exit 2
fi

# --- single-verdict plumbing -------------------------------------------------
RESULTS=()
failures=()
skipped=()
record()    { failures+=("$1"); }
note_skip() { skipped+=("$1"); }
step()      { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# on_err — the ERR backstop (QA04-31). Fires for failures no explicit rc check
# caught: a sourced helper dying, step 5 aborting, anything unexpected. It
# records the failure and CONTINUES — the e2e phase still runs, and the
# summary at the end is the single verdict. Expected failures never fire it:
# they sit inside `|| rc=$?` checks or if-conditions (AND-OR lists).
on_err() {
  local rc=$? line=${BASH_LINENO[0]}
  [ "$rc" -eq 0 ] && return 0
  if [ -n "${CI_ERR_EXPECTED:-}" ]; then return 0; fi
  record "unexpected failure at ci.sh line $line (rc=$rc) — the ERR backstop recorded it and continued"
  printf '  \033[1;31mABORT\033[0m ci.sh:%s rc=%s — unexpected; recorded, continuing to the e2e phase\n' "$line" "$rc"
}
trap on_err ERR

# The podman-capable job has one entry point and one decision (measured, not
# guessed): scripts/guarded-entry.sh -> guarded-service-user.sh --doctor ->
# run as the service user. See scripts/ci-guarded-job.sh for the caller-side
# helpers and the reason a bare "podman is on PATH" probe is not enough.
if ! . ./scripts/ci-guarded-job.sh; then
  record "scripts/ci-guarded-job.sh failed to load (syntax error?) — continuing with fallbacks"
fi
: "${HAMLET_CI_ENTRY:=scripts/guarded-entry.sh}"
: "${HAMLET_CI_RECLAIM:=scripts/village-reclaim.sh}"

# verdict <label> <cmd...> — run one gate, record PASS/FAIL, keep going.
verdict() {
  local label="$1"; shift
  step "$label"
  local rc=0
  "$@" || rc=$?
  if [ "$rc" -eq 0 ]; then
    RESULTS+=("PASS|$label"); printf '  \033[32mPASS\033[0m %s\n' "$label"
  else
    RESULTS+=("FAIL|$label"); printf '  \033[1;31mFAIL\033[0m %s\n' "$label"
    record "$label"
  fi
}

verdict "[1/5] cargo fmt --all --check" cargo fmt --all --check
verdict "[2/5] cargo clippy --workspace --all-targets -- -D warnings" \
  cargo clippy --workspace --all-targets -- -D warnings
verdict "[3/5] cargo test --workspace" cargo test --workspace
verdict "[4/5] scripts/check-deps.sh" ./scripts/check-deps.sh

# ---------------------------------------------------------------------------
# [5/5] The guarded podman battery — the podman-capable job.
#
# Two policy steps live inside it, both previously invisible:
#
#  5a. The disk-leak policy (scripts/village-reclaim.sh). An ABORTED guarded
#      run strands its village tree: the tree is built with tempfile under
#      $TMPDIR and then handed to SUBUIDS (`podman unshare chown`, ADR-0011
#      cl. 2), and a subuid-owned file cannot be unlinked by the owner
#      outside the userns — not even by root, because the parent dirs are
#      0700 and subuid-owned. Measured cost of an aborted cold-bootstrap run
#      on this workload: ~18 GB. `podman unshare rm -rf` is the ONLY reclaim.
#      So free space and stranded trees are read BEFORE the run, into the gate
#      log, where "the disk filled up" is diagnosable instead of mysterious.
#  5b. The battery itself, through the entry point — not the old
#      `cargo test -- --ignored podman` as whatever user happens to be here.
#      That form could only ever skip (QA04-08: the invoking user's TMPDIR is
#      a tmpfs, the wrong work area) and its remedy lived in another file, so
#      the guarded suites asserted nothing while the battery reported PASS.
#      The entry point measures with `guarded-service-user.sh --doctor` and
#      runs as the service user when this user cannot (ADR-0032 cl. 4).
#
# Every command in step 5 is rc-checked: an abort here records a FAIL and
# falls through to the e2e phase — it can never kill the script (QA04-31).
# ---------------------------------------------------------------------------
step "[5/5a] village-tree disk policy (stranded subuid trees are reclaim-only via podman unshare)"
disk_rc=0
ci_guarded_disk_audit || disk_rc=$?
if [ "$disk_rc" -ne 0 ]; then
  RESULTS+=("FAIL|[5/5a] village-tree disk policy")
  record "[5/5a] village-tree disk policy: work area below the QA04-08 free-space floor"
else
  RESULTS+=("PASS|[5/5a] village-tree disk policy")
fi

step "[5/5b] guarded podman battery (via ${HAMLET_CI_ENTRY})"
if ! guarded_log="$(mktemp)" 2>/dev/null; then
  RESULTS+=("FAIL|[5/5] guarded podman battery (could not even start — no log file)")
  record "[5/5] cannot create a log file for the guarded battery"
else
  # Streamed AND captured (tee): the battery is tens of minutes of cold image
  # pulls and a ~1 GB toolchain extract, so a silent half hour reads as a hang.
  # The entry point's exit code is classified below; shield that expected
  # failure from the ERR backstop so it is counted once, with its real label.
  CI_ERR_EXPECTED=1
  ./"${HAMLET_CI_ENTRY}" 2>&1 | tee "$guarded_log"
  # PIPESTATUS must be read IMMEDIATELY: the next simple command (even a bare
  # assignment) overwrites it with its own status — measured: `CI_ERR_EXPECTED=`
  # after the pipeline turned a real rc=3 into a 0 and classified a SKIP as
  # "all seven suites ran". The vacuous-PASS hazard, live.
  guarded_rc=${PIPESTATUS[0]}
  CI_ERR_EXPECTED=
  guarded_out="$(cat "$guarded_log")"; rm -f "$guarded_log"

  # The mirror-image hazard is a vacuous PASS: a suite set that skips on this
  # host exits 0 having asserted nothing (CI04-01 doctrine). So the transcript
  # is scanned for the suites' recorded skip lines and they are printed — a
  # coverage hole must be visible in the gate log, not indistinguishable from
  # coverage. A skip is recorded as a SKIP (not a FAIL) because a host
  # precondition is not a product defect — but it is counted and listed, so
  # "PASS with skips" can never be read as a clean gate.
  if [ "$guarded_rc" -eq 0 ]; then
    if printf '%s\n' "$guarded_out" | grep -q '^SKIP'; then
      printf '\n\033[1;33mNOTE: guarded suites skipped on this host (preconditions unmet):\033[0m\n'
      printf '%s\n' "$guarded_out" | grep '^SKIP' | sed 's/^/  - /'
      note_skip "[5/5] guarded podman battery: suites skipped (remedy printed above)"
      RESULTS+=("PASS+SKIP|[5/5] guarded podman battery (suites skipped)")
    else
      RESULTS+=("PASS|[5/5] guarded podman battery (all seven suites ran)")
    fi
  elif [ "$guarded_rc" -eq 3 ]; then
    # Not applicable here / already running elsewhere; remedy printed by the
    # entry point. Never a pass — and never a FAIL: it is a recorded refusal,
    # not a broken gate (QA04-26).
    note_skip "[5/5] guarded podman battery not runnable on this host (remedy printed above)"
    RESULTS+=("SKIP|[5/5] guarded podman battery not runnable on this host")
  else
    record "[5/5] guarded podman battery (see the log above; check QA04-07/08/09 before reading this as a product defect)"
    RESULTS+=("FAIL|[5/5] guarded podman battery")
  fi
fi

# --- the e2e phase (QA04-31: runs unconditionally, after step 5) ------------
# E2E scripts (CONVENTIONS §9): run every scripts/e2e-*.sh that exists.
# Failures accumulate into the same verdict as everything above — a red step 5
# never suppresses them, and an aborted step 5 never skips them.
shopt -s nullglob
for s in scripts/e2e-*.sh; do
  verdict "[e2e] $s" "$s"
done

# --- the table --------------------------------------------------------------
printf '\n\033[1m== CI battery ==\033[0m\n'
for row in "${RESULTS[@]}"; do
  case "$row" in
    PASS*)   printf '  \033[32mPASS\033[0m     %s\n' "${row#*|}" ;;
    FAIL*)   printf '  \033[1;31mFAIL\033[0m     %s\n' "${row#*|}" ;;
    SKIP*|PASS+SKIP*) printf '  \033[1;33mSKIP\033[0m     %s\n' "${row#*|}" ;;
  esac
done

if [ ${#failures[@]} -gt 0 ]; then
  printf '\n\033[1;31mCI battery: FAIL (%d)\033[0m\n' "${#failures[@]}"
  for f in "${failures[@]}"; do printf '  - %s\n' "$f"; done
  if [ ${#skipped[@]} -gt 0 ]; then
    printf '\n\033[1;33mskipped (coverage holes, not passes):\033[0m\n'
    for s in "${skipped[@]}"; do printf '  - %s\n' "$s"; done
  fi
  exit 1
fi

printf '\n\033[1;32mCI battery: PASS\033[0m\n'
if [ ${#skipped[@]} -gt 0 ]; then
  printf '\033[1;33mwith %d skipped step(s) that asserted nothing (see above)\033[0m\n' "${#skipped[@]}"
fi
