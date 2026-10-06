#!/usr/bin/env bash
# e2e-06-guard-mutations.sh — mutation grading for the guarded battery's HOST
# PRECONDITIONS (QA04-24; SPEC §4, ADR-0031/0032/0033/0034, CONVENTIONS §5).
#
# Why this file exists. The guarded suites are gated by three guards written
# during the Sprint 04 blocker pass:
#
#   support::userns_precondition      (QA04-07, restated twice)
#   support::work_area_precondition    (QA04-08)
#   the `mirror_warms` branch         (QA04-09, re-cut in QA04-22)
#
# All three were wrong in the SAME direction — they reported "unmet" on hosts
# that were fine — and QA04-10/QA04-11 proved the standing rule this file
# enforces: a claim nobody has watched fail is not a claim. A guard that skips
# a compliant host does not read as a skip; it reads as a green battery that
# asserted nothing, which is the CI04-01 class of lie, one level above the
# product.
#
# Layers:
#   T1  Guard acceptance, forward: tests/guard_acceptance.rs pins every guard
#       in BOTH directions using `podman` shims (no container, no root, <1s).
#   T2  Mutation grading: each historical defect of each guard is re-introduced
#       into crates/hamlet-daemon/tests/support/mod.rs, T1 is re-run, and the
#       mutation MUST be killed (red). A mutation that survives is a hole and
#       is reported by this script as a FAILURE, not as a note.
#   T3  Live host shape: the one thing shims cannot simulate — which arm of the
#       QA04-09 `mirror_warms` branch this host actually takes — recorded so the
#       gate log says which claim was exercised. See HANDOFF.md QA04-25.
#
# Usage:  ./scripts/e2e-06-guard-mutations.sh
# Needs:  cargo, a disk-backed path for QA_GUARD_DISK_AREA (default
#         $PWD/target/qa-guard-area), podman on PATH (only for the T3 note and
#         the T1 positive case; both record a SKIP with a remedy without it).
set -euo pipefail
cd "$(dirname "$0")/.."

# The one-guarded-job-per-host lock (QA04-26 / QA05-09): T3 runs a real
# guarded suite, so it must take the same lock every other CI-driven guarded
# invocation takes — or a concurrent battery deletes its containers
# mid-assertion and the failure reads like a product defect.
. ./scripts/ci-guarded-job.sh

FAILURES=0
step() { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass() { printf '  ok: %s\n' "$*"; }
fail() { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
note() { printf '  note: %s\n' "$*"; }

SUPPORT="crates/hamlet-daemon/tests/support/mod.rs"
SHIM_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/qa-guard-shims.XXXXXX")"
export QA_GUARD_SHIM_DIR="$SHIM_ROOT"
export QA_GUARD_DISK_AREA="${QA_GUARD_DISK_AREA:-$PWD/target/qa-guard-area}"
mkdir -p "$QA_GUARD_DISK_AREA"
cleanup() { rm -rf "$SHIM_ROOT"; }
trap cleanup EXIT

# The support module is mutated in place and reverted from git afterwards;
# refuse to start on a dirty tree so a run can never eat someone's work.
if ! git diff --quiet -- "$SUPPORT"; then
  printf '\033[1;31mrefusing to mutate: %s is modified in the working tree\033[0m\n' "$SUPPORT"
  exit 1
fi
restore() { git checkout -- "$SUPPORT" 2>/dev/null || true; }
trap 'cleanup; restore' EXIT

run_acceptance() {
  # NOT --quiet: the per-test lines are how the harness proves the positive
  # case actually executed rather than recorded a skip.
  cargo test -p hamlet-daemon --test guard_acceptance -- --test-threads=1 --nocapture 2>&1
}

# The userns POSITIVE case (`a_compliant_host_is_not_skipped`) is the only
# assertion that can kill the QA04-17 probe mutation, and it needs a real
# subuid-capable podman. If it SKIPS, the mutation below is graded against a
# test that asserted nothing — the CI04-01 shape. So the harness runs with
# HAMLET_PODMAN_TESTS=1 whenever podman exists, and treats a skipped positive
# as a failure of the harness, not as a pass.
if command -v podman >/dev/null 2>&1; then
  export HAMLET_PODMAN_TESTS=1
else
  unset HAMLET_PODMAN_TESTS || true
  printf '\033[1;33mNOTE: no podman on PATH — the userns positive case will record a SKIP,\n'
  printf 'so mutations M1/M2 cannot be graded here. That is a coverage hole, not a pass.\033[0m\n'
fi

# did_positive_run <output> — true when the positive case executed (not skipped).
# `--quiet` may or may not print the per-test line depending on the runner
# version, so accept either form and treat only an explicit `SKIP`/absence of
# the test name as "not executed".
did_positive_run() {
  printf '%s' "$1" | grep -q 'a_compliant_host_is_not_skipped' \
    && ! printf '%s' "$1" | grep -q 'SKIP a_compliant_host_is_not_skipped'
}

# --- T1: acceptance, forward -----------------------------------------------
step "T1: guard acceptance — both directions, driven by podman shims"
if T1_OUT="$(run_acceptance)"; then
  printf '%s\n' "$T1_OUT" | grep -E '^(test |SKIP )' | sed 's/^/  /' || true
  pass "all guards accept compliant hosts and reject non-compliant ones"
  if ! did_positive_run "$T1_OUT"; then
    fail "the userns POSITIVE case did not execute (recorded skip?) — M1/M2 cannot be \
graded, and a mutation graded against a skipped test is a vacuous green"
  fi
else
  printf '%s\n' "$T1_OUT"
  fail "guard acceptance suite"
fi

# --- T2: mutation grading ---------------------------------------------------
# mutate <name> <python-ish sed script> — applies an edit, expects the
# acceptance suite to go RED, then reverts.
mutate() {
  local name="$1" from="$2" to="$3"
  step "T2 mutation: $name"
  if ! grep -qF -- "$from" "$SUPPORT"; then
    fail "$name — the historical defect is already absent from $SUPPORT (nothing to grade)"
    return
  fi
  python3 - "$SUPPORT" "$from" "$to" <<'PY'
import sys
path, frm, to = sys.argv[1], sys.argv[2], sys.argv[3]
src = open(path).read()
assert src.count(frm) == 1, f"anchor is not unique ({src.count(frm)}x): {frm!r}"
open(path, "w").write(src.replace(frm, to))
PY
  if MUT_OUT="$(run_acceptance)"; then
    printf '%s\n' "$MUT_OUT" | grep -E '^test ' | sed 's/^/    /' || true
    fail "$name SURVIVED — the guard still accepts a host it must reject; nothing pins it"
  else
    printf '%s\n' "$MUT_OUT" | grep -E '^test .*(FAILED|ok)' | sed 's/^/    /' || true
    pass "$name killed (acceptance suite went red)"
  fi
  restore
}

# expect_survive <name> <from> <to> — the inverse grading: the mutation MUST
# stay green, because a second, independent check is supposed to absorb it.
# Used where "no test catches this" is the *correct* outcome (defence in
# depth), so that a future refactor collapsing the two checks is caught.
expect_survive() {
  local name="$1" from="$2" to="$3"
  step "T2 mutation (survival EXPECTED): $name"
  if ! grep -qF -- "$from" "$SUPPORT"; then
    fail "$name — anchor absent from $SUPPORT (nothing to grade)"
    return
  fi
  python3 - "$SUPPORT" "$from" "$to" <<'PY'
import sys
path, frm, to = sys.argv[1], sys.argv[2], sys.argv[3]
src = open(path).read()
assert src.count(frm) == 1, f"anchor is not unique ({src.count(frm)}x): {frm!r}"
open(path, "w").write(src.replace(frm, to))
PY
  if MUT_OUT="$(run_acceptance)"; then
    pass "$name — still rejected by the second check (depth intact)"
  else
    printf '%s\n' "$MUT_OUT" | grep -E '^test .*FAILED' | sed 's/^/    /' || true
    fail "$name — the mutation now escapes: the two checks had collapsed into one"
  fi
  restore
}

# M1 — QA04-17, first cut: the probe used an inside uid outside the mapped
# window (`1 → <base> 65536`), so `chown` returned EINVAL on EVERY host and the
# whole battery skipped unconditionally. The current probe uid is 1000; putting
# 100001 back must make the guard reject a host that is in fact compliant —
# which is observable ONLY on a real podman host (a shim cannot make an
# out-of-window chown fail while an in-window one succeeds). Hence the
# positive-case requirement above.
mutate "QA04-17 probe uid outside the subuid window" \
  'const PROBE_INSIDE_UID: u32 = 1000;' \
  'const PROBE_INSIDE_UID: u32 = 100001;'

# M2 — the self-validation that QA04-17's fix introduced. Without it, a userns
# stack that accepts the chown but cannot map it reads as full support, and the
# SPEC §4 ownership assertions run vacuously.
mutate "QA04-07 self-validation dropped (m.uid() != own)" \
  '.map(|m| ok && m.uid() != own && m.uid() != u32::MAX)' \
  '.map(|m| ok && m.uid() != u32::MAX)'

# M2b — M2 with a chown that also *succeeds* at the syscall level. A shim
# cannot map a uid, so this is the same hole seen from the other side; the
# pair must be killed together (see the shim-based negative test).
mutate "QA04-07 owner check reduced to a MAX sentinel" \
  '&& m.uid() != own' \
  ''

# M3 — the chown exit status ignored (`unwrap_or((true, …))`) is reached only
# when `podman` cannot be spawned at all. This mutation is EXPECTED TO
# SURVIVE, and that is the point: the ownership self-validation is a second,
# independent gate, so a dead `podman` on PATH still yields a SKIP rather
# than support. Asserted rather than merely tolerated — if it ever stops
# surviving, the two checks have become one and the depth is gone.
expect_survive "QA04-07 dead podman is still caught by the ownership check" \
  '.unwrap_or((false, String::new()));' \
  '.unwrap_or((true, String::new()));'

# M4 — QA04-08: the RAM-backed rejection is the whole guard. With `is_ram_backed`
# neutered, a tmpfs TMPDIR (the default on this class of host) stops being a
# recorded SKIP and becomes an OOM/truncated-archive failure that reads like a
# product bug — the exact shape QA04-08 filed.
mutate "QA04-08 tmpfs no longer rejected" \
  'matches!(fstype.trim(), "tmpfs" | "ramfs")' \
  'matches!(fstype.trim(), "__none__")'

# M5 — QA04-08: the free-space floor replaced by "any answer is fine".
mutate "QA04-08 free-space floor removed" \
  'Some(avail) if avail >= QA04_08_MIN_BYTES => None,' \
  'Some(_avail) => None,'

# M6 — QA04-09: the classifier is the only thing stopping a digest or
# permission failure from being excused as EXDEV. With `contains` gone the
# QA04-09 split arm would excuse any failure at all.
mutate "QA04-09 is_exdev excuses everything" \
  'failure.to_ascii_lowercase().contains("cross-device")' \
  'true'

# M7 — QA04-09: the mirror-shape helper must report the MEASURED device pair.
# Inverting it is the honest mutation (a helper that answers from anything but
# the measurement is a guess about the host nobody made).
mutate "QA04-09 device comparison inverted (reports a guess)" \
  'Some(device_of(a)? == device_of(b)?)' \
  'Some(device_of(a)? != device_of(b)?)'

# --- T3: live host shape ----------------------------------------------------
step "T3: live host shape for the QA04-09 mirror_warms branch"
if [ "${HAMLET_PODMAN_TESTS:-0}" = "1" ] && command -v podman >/dev/null 2>&1; then
  note "TMPDIR=${TMPDIR:-<unset>}  fstype=$(df --output=fstype -- "${TMPDIR:-/tmp}" | sed -n 2p)"
  if ! ci_guarded_lock_or_report; then
    # Refused with a recorded reason (QA04-26): never a FAIL, never a silent
    # collision. The holder's pid is printed above by the lock helper.
    note "another guarded job holds the host lock — T3 records a SKIP with the holder named (QA04-26)"
  elif BOOT_OUT="$(cargo test -p hamlet-daemon --test bootstrap_guarded --quiet -- --ignored \
        podman_bootstrap_installs --nocapture 2>&1)"; then
    note "observed: $(printf '%s\n' "$BOOT_OUT" | grep -o 'host shape:.*' | head -1)"
    pass "the QA04-09 branch's exercised arm is recorded for this host"
    ci_guarded_lock_release
  else
    printf '%s\n' "$BOOT_OUT" | grep -E 'SKIP |panicked|test result' | sed 's/^/    /' || true
    note "the guarded bootstrap suite did not report its shape on this host (SKIP or failure) — \
see HANDOFF.md QA04-25"
    ci_guarded_lock_release
  fi
else
  note "no real podman run: the mirror_warms branch's exercised arm is UNSTATED for this host \
(HANDOFF.md QA04-25). Run with HAMLET_PODMAN_TESTS=1 and a disk-backed TMPDIR."
fi

# --- summary ----------------------------------------------------------------
if [ "$FAILURES" -eq 0 ]; then
  printf '\033[1;32me2e-06-guard-mutations: PASS\033[0m\n'
else
  printf '\033[1;31me2e-06-guard-mutations: %s FAILURE(S)\033[0m\n' "$FAILURES"
  exit 1
fi