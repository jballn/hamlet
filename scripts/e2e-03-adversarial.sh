#!/usr/bin/env bash
# e2e-03-adversarial.sh — QA prompt 03/05, mock-agent adversarial harness for
# the Sprint 03 agent engine (SPEC §6; ADR-0015/0016/0017; CONVENTIONS §3 r6,
# §6, §9). No podman required — everything runs against the FakeAgent seam.
#
# Layers:
#   T1  Adversarial agent suite (always runs):
#         crates/hamlet-daemon/tests/adversarial_agent.rs
#       FIFO-under-load (100 messages), interrupt storms/races, crash paths
#       (mid-task crash + queue preservation, restart-limit terminality with
#       a zombie-respawn observation window), daemon-death mid-delivery
#       (no replay of an acked envelope), malformed/huge/smuggling frames,
#       secrets hygiene (file + sled byte scan), backpressure caps.
#   T2  QA05-02 regression, GATING (the finding is fixed): the tail-loss
#       repro (stop-mid-run used to drop buffered stdout frames) is now a
#       committed, un-ignored test. It is a ~40%-rate race, so T1's single
#       run is not enough — T2 samples the window 10x and fails the harness
#       on any loss.
#   T4  Source-level ratchet for the metrics liveness wiring (HANDOFF
#       finding QA05-04): the life task must arm a flush-retry tick guarded
#       by `has_pending`, or coalesced accounting stays invisible to
#       `metrics_for` until the life ends — the stale-tail flake that made
#       the flood scenarios intermittent.
#   T3  Source-level ratchets:
#         - `.expose()` (secret unmasking) appears ONLY in supervise.rs's
#           session_env — the single channel secrets take into a parcel
#           (SPEC §11.4, CONVENTIONS §6).
#         - FakeAgent never logs env VALUES (only key names).
set -euo pipefail
cd "$(dirname "$0")/.."

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  ok: %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }

# --- T1: adversarial agent suite ---------------------------------------------
step "T1: mock-agent adversarial suite (FakeAgent; SPEC §6)"
if T1_OUT="$(cargo test -p hamlet-daemon --test adversarial_agent --quiet 2>&1)"; then
  printf '%s\n' "$T1_OUT" | tail -3
  pass "adversarial_agent: all committed tests green"
else
  printf '%s\n' "$T1_OUT"
  fail "adversarial_agent suite"
fi

# --- T2: QA05-02 regression, sampled (gating; see HANDOFF QA05-02) ------------
step "T2: QA05-02 tail-loss regression, 10 samples of the race window"
# QA05-06: the filter must match EXACTLY ONE test, and each sample must
# prove it ran one — a filter matching zero tests exits 0 and reports a
# false pass (that is precisely what `--exact` + a name PREFIX did here).
REPRO_FAILS=0
REPRO_RAN=0
REPRO_NAME="repro_qa05_02_stop_midrun_loses_telemetry_tail"
for _ in 1 2 3 4 5 6 7 8 9 10; do
  REPRO_OUT="$(cargo test -q -p hamlet-daemon --test adversarial_agent \
    "$REPRO_NAME" -- --exact 2>&1)" || true
  if printf '%s\n' "$REPRO_OUT" | grep -q '1 passed'; then
    REPRO_RAN=$((REPRO_RAN + 1))
  elif printf '%s\n' "$REPRO_OUT" | grep -qE 'FAILED|panicked'; then
    REPRO_FAILS=$((REPRO_FAILS + 1))
  else
    fail "T2 harness broken: filter matched ZERO tests or output changed:"
    printf '%s\n' "$REPRO_OUT" | tail -2
    break
  fi
done
if [ "$REPRO_FAILS" -gt 0 ]; then
  fail "QA05-02 REGRESSED: repro lost the stdout tail in $REPRO_FAILS/10 runs"
elif [ "$REPRO_RAN" -eq 10 ]; then
  pass "QA05-02 fixed: repro ran and passed 10/10 (was 6/15 failing before drain_tail)"
fi

# --- T3: source-level secrets ratchets ----------------------------------------
step "T3: secrets ratchets (SPEC §11.4, CONVENTIONS §6)"

EXPOSE_HITS="$(grep -rn '\.expose()' crates/hamlet-daemon/src --include='*.rs' || true)"
OFFENDERS="$(printf '%s\n' "$EXPOSE_HITS" | grep -v 'supervise\.rs' | grep -v '^$' || true)"
if [ -z "$OFFENDERS" ]; then
  pass "daemon .expose() confined to supervise.rs (session_env channel)"
else
  fail "secret unmasking outside supervise.rs:"
  printf '%s\n' "$OFFENDERS"
fi

# FakeAgent logs env-check verdicts by KEY NAME only, never values.
if grep -q 'env-check {verdict}:{key}' crates/hamlet-daemon/src/bin/fakeagent.rs; then
  pass "fakeagent env-check logs key names only"
else
  fail "fakeagent env-check logging shape changed — re-audit for value leaks"
fi

# --- T4: metrics liveness wiring ratchet (see HANDOFF step-04 follow-up) -----
step "T4: metrics flush-retry wiring (live totals at a quiet boundary)"

if grep -q 'if metrics.has_pending() => Step::FlushTick' \
    crates/hamlet-daemon/src/supervise.rs; then
  pass "life task arms the flush-retry tick guarded by has_pending()"
else
  fail "flush-retry tick missing from the life-task select loop:"
  printf '  coalesced metrics would stay invisible to metrics_for until finish()\n'
fi

if grep -q 'Step::FlushTick => {' crates/hamlet-daemon/src/supervise.rs \
  && grep -q 'metrics.retry_flush()' crates/hamlet-daemon/src/supervise.rs; then
  pass "tick re-pins and retries the flush"
else
  fail "FlushTick arm does not retry_flush (a fired sleep must also be re-pinned)"
fi

if [ "$FAILURES" -eq 0 ]; then
  printf '\033[1;32me2e-03-adversarial: PASS\033[0m\n'
else
  printf '\033[1;31me2e-03-adversarial: %s FAILURE(S)\033[0m\n' "$FAILURES"
  exit 1
fi
