#!/usr/bin/env bash
# e2e-07-ci-abort.sh — regression: the gate must not die without a verdict.
#
# QA04-31: scripts/ci.sh once aborted mid-run (a syntax error from a
# concurrent edit), and because of it (a) every e2e harness was silently
# skipped and (b) no "CI battery: FAIL" line was ever printed. This harness
# pins both properties by injecting the two historical abort shapes into a
# SANDBOXED copy of the CI scripts — the real tree is never modified:
#
#   T1  parse-death in ci.sh's step-5 region (the exact QA04-31 shape):
#       the e2e phase must still run and "CI battery: FAIL (n)" must still
#       be printed, even though steps 1-5 cannot.
#   T2  source-death of scripts/ci-guarded-job.sh (a sourced helper dying
#       mid-battery): same assertions, via the ERR-backstop path.
#
# The sandbox runs ci.sh against an empty tree: steps 1-4 and the e2e
# harnesses fail fast there, and that is fine — what is asserted is not
# their outcome but that they RAN and were COUNTED in the verdict.
#
# QA04-26: the guarded battery's run lock. A second invocation while the
# host lock is held must be refused with a recorded reason (exit 3 = SKIP,
# holder named) — never a suite FAIL, which is how interference used to read
# as a product defect. Graded in T3 against the real lock:
#
#   T3a ci_guarded_lock refuses (rc=3) while held; ci_guarded_lock_or_report
#       names the live holder's pid (the recorded reason).
#   T3b scripts/guarded-entry.sh under the lock exits 3 with a SKIP line.
set -euo pipefail
cd "$(dirname "$0")/.."

FAILURES=0
step() { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass() { printf '  ok: %s\n' "$*"; }
fail() { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
note() { printf '  note: %s\n' "$*"; }

# The lock helpers (QA04-26) live in the CI helper; T3 needs them directly.
. ./scripts/ci-guarded-job.sh

WORK="$(mktemp -d "${TMPDIR:-/tmp}/qa-ci-abort.XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# make_sandbox <name> — a disposable copy of the CI scripts. The guarded entry
# point is stubbed to exit 3 (SKIP) so nothing in the sandbox can start a real
# (~50 min) battery: an unreachable host, not a running one.
#
# SELF-EXCLUSION: this harness never copies itself into the sandbox. A battery's
# e2e phase runs every scripts/e2e-*.sh it finds — including, in production,
# this very file — and that is correct; but a sandboxed copy of THIS harness in
# the battery's e2e phase would build its own sandbox from the corrupted tree
# and recurse forever (measured: ~700 generations and a /tmp fill before the
# first kill). The assertions do not need it: they read the [e2e] markers of
# the OTHER harnesses, which are all present.
make_sandbox() {
  local d="$WORK/$1" f
  mkdir -p "$d/scripts"
  for f in scripts/*.sh; do
    [ "$(basename "$f")" = "e2e-07-ci-abort.sh" ] && continue
    cp "$f" "$d/scripts/"
  done
  printf '#!/usr/bin/env bash\necho "SKIP: stub entry point (e2e-07 regression sandbox)"\nexit 3\n' \
    > "$d/scripts/guarded-entry-stub.sh"
  chmod +x "$d/scripts/guarded-entry-stub.sh"
}

# assert_battery <log> <why> — the two QA04-31 properties: (1) the e2e phase
# ran, (2) a final FAIL verdict was printed. Neither may be missing after an
# abort inside step 5.
assert_battery() {
  local log="$1" why="$2"
  if ! grep -q '\[e2e\] scripts/e2e-0' "$log"; then
    fail "$why: the e2e phase did not run (no [e2e] step markers in the log)"
  else
    pass "$why: the e2e phase ran despite the abort"
  fi
  if ! grep -q 'CI battery: FAIL (' "$log"; then
    fail "$why: no 'CI battery: FAIL (n)' verdict was printed"
  else
    pass "$why: the final verdict was printed"
  fi
}

# --- T1: parse-death in ci.sh's step-5 region --------------------------------
step "T1: QA04-31 — a syntax error in ci.sh's step-5 region"
make_sandbox t1
if ! grep -q 'step "\[5/5a\]' scripts/ci.sh; then
  fail "injection anchor not found: ci.sh has no '[5/5a]' step line — update this harness's anchor"
else
  # Corrupt the step-5 region, exactly the way a concurrent edit did (QA04-31):
  # a syntax error on the line after the step-5 marker.
  sed '/step "\[5\/5a\]/a\if then fi' "$WORK/t1/scripts/ci.sh" > "$WORK/t1/scripts/ci.sh.new" \
    && mv "$WORK/t1/scripts/ci.sh.new" "$WORK/t1/scripts/ci.sh"
  if bash -n "$WORK/t1/scripts/ci.sh" 2>/dev/null; then
    fail "the injected corruption did not break the sandbox ci.sh (sed anchor drifted)"
  else
    rc=0
    env HAMLET_CI_ENTRY=scripts/guarded-entry-stub.sh \
      bash "$WORK/t1/scripts/ci.sh" >"$WORK/t1.log" 2>&1 || rc=$?
    if [ "$rc" -eq 0 ]; then
      fail "T1: a parse-corrupted ci.sh reported success (rc=0)"
    fi
    assert_battery "$WORK/t1.log" "T1 (parse-death in step 5)"
    if grep -q 'parse-corrupted' "$WORK/t1.log"; then
      pass "T1: the corruption was named in the log, not hidden"
    else
      fail "T1: the log does not name ci.sh's syntax error"
    fi
  fi
fi

# --- T2: source-death of the CI helper ---------------------------------------
step "T2: QA04-31 — scripts/ci-guarded-job.sh dies (parse error) mid-battery"
make_sandbox t2
printf 'if then fi\n' >> "$WORK/t2/scripts/ci-guarded-job.sh"
rc=0
env HAMLET_CI_ENTRY=scripts/guarded-entry-stub.sh \
  bash "$WORK/t2/scripts/ci.sh" >"$WORK/t2.log" 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
  fail "T2: a battery with a dead CI helper reported success (rc=0)"
fi
assert_battery "$WORK/t2.log" "T2 (source-death of ci-guarded-job.sh)"

# --- T3: QA04-26 — the second guarded invocation is refused, never red -------
step "T3: QA04-26 — a guarded battery under a held host lock is refused with a recorded reason"
# Holder: takes the real host lock via the same helper every CI caller uses,
# records its own pid in the lock file, and holds for 60 s. The sleep must
# NOT inherit fd 9 (9>&-): a child holding the open file description keeps the
# flock alive after the holder dies — measured: the lock survived the kill
# until the orphaned sleep expired.
( . ./scripts/ci-guarded-job.sh; ci_guarded_lock && { sleep 60 9>&-; } || exit 3 ) &
HOLDER=$!
sleep 1
if ! kill -0 "$HOLDER" 2>/dev/null; then
  note "a guarded job already holds the host lock — T3 records a SKIP (cannot grade refusal deterministically)"
else
  # T3a: the mechanism. ci_guarded_lock must refuse (rc=3); the verbose form
  # must name the live holder's pid — that naming IS the recorded reason.
  lrc=0
  ci_guarded_lock || lrc=$?
  if [ "$lrc" -eq 3 ]; then
    pass "T3a: ci_guarded_lock refuses while held (rc=3)"
  else
    fail "T3a: ci_guarded_lock returned $lrc while the lock is held (must be 3)"
  fi
  rep=""; rrc=0
  rep="$(ci_guarded_lock_or_report 2>&1)" || rrc=$?
  if [ "$rrc" -eq 3 ] && printf '%s\n' "$rep" | grep -q "pid=$HOLDER "; then
    pass "T3a: the refusal names the live holder (pid $HOLDER) — a recorded reason, not a mystery"
  else
    fail "T3a: the refusal did not name the live holder (rc=$rrc): ${rep}"
  fi

  # T3b: the entry point. With the lock held, guarded-entry.sh must decline
  # with exit 3 + a SKIP line — never start a second battery, never red.
  # Gated on T3a's probe: if we could not verify the holder is holding, a run
  # here might start a REAL battery, so we skip it instead of risking that.
  if [ "$lrc" -eq 3 ]; then
    erc=0
    env TMPDIR=/var/hamlet/tmp ./scripts/guarded-entry.sh >"$WORK/t3.log" 2>&1 || erc=$?
    if [ "$erc" -eq 3 ] && grep -q 'SKIP' "$WORK/t3.log"; then
      pass "T3b: guarded-entry.sh under the lock exited 3 with a SKIP (refusal, not failure)"
    else
      fail "T3b: guarded-entry.sh rc=$erc — expected 3 + SKIP (log below)"
      sed 's/^/    /' "$WORK/t3.log" | tail -10
    fi
  else
    note "T3b skipped: the mechanism probe above did not confirm a held lock — refusing to risk starting a real battery"
  fi

  kill "$HOLDER" 2>/dev/null || true
  wait "$HOLDER" 2>/dev/null || true
  if flock -n "$HAMLET_GUARDED_LOCK" true 2>/dev/null; then
    pass "T3: the lock is free again after the holder exited"
  else
    fail "T3: the lock is still held after the holder was killed"
  fi
fi

# --- summary -----------------------------------------------------------------
if [ "$FAILURES" -eq 0 ]; then
  printf '\033[1;32me2e-07-ci-abort: PASS\033[0m\n'
else
  printf '\033[1;31me2e-07-ci-abort: %s FAILURE(S)\033[0m\n' "$FAILURES"
  exit 1
fi
