#!/usr/bin/env bash
# e2e-04-comms.sh — QA prompt 05/05, adversarial harness for the Sprint 04
# communication planes (SPEC §8; ADR-0009/0010/0019/0020/0022/0023;
# CONVENTIONS §9). No podman required — every unit-level attack runs against
# the scripted seams (FakeRuntime / HttpClient); only T3 needs a real host.
#
# Layers:
#   T1  Adversarial comms suite (always runs):
#         crates/hamlet-daemon/tests/adversarial_comms.rs
#       Mail: header-injection matrix (CR/space/oversize), spoofed-From
#       delivery + bounce, bounce paths (unallocated/hibernating/crashed/
#       overflow, named senders never chained), partial-file recovery,
#       kill/restart zero-duplicates across two restarts, FIFO storm.
#       Watcher: churn bursts, event ordering determinism.
#       Freight: audit-rule matrix (owner/foreign-uid/named-group/world-
#       writable, combined), symlink escape flags without getfacl,
#       promotion-read under strict ACLs.
#       Dispatch: URL allowlist matrix (byte-prefix, port, http:// denied at
#       config time), secret redaction in outbox artifacts, retry schedule
#       gaps per ADR-0010, dead-letter after exhaustion, malformed JSON ->
#       precise error file without panic.
#       Inbound webhooks: bad auth (constant-time), oversize body, path-
#       traversal topics 404 + sanitizer-clean artifacts, content-type
#       smuggling (body is truth), fixed rejection tokens (no echo).
#   T2  Source-level ratchets:
#         - mail.rs: CR anywhere in a message is a typed rejection (LF-only,
#           ADR-0019 cl. 5) — the header-injection backstop.
#         - web.rs: constant-time secret_eq + fixed rejection tokens
#           (ADR-0022: no input echo in error artifacts).
#         - dispatch.rs: REDACTED_HEADERS covers the full auth/cookie family
#           (SPEC §8.4; ADR-0020 cl. 6).
#         - daemon freight.rs: quarantine constants pinned (flag 0444 / dir
#           0755 — agents can read, never rewrite, the flag).
#         - dispatch.rs: the allowlist ENFORCEMENT call survives (QA04-10 —
#           deleting it once left the whole adversarial suite green).
#         - dispatch.rs: the backoff sleep is the bare deterministic schedule
#           value — no jitter arithmetic (QA04-11 — a jittered sleep kept all
#           36 workspace test binaries green).
#   T3  Guarded real-podman freight suite. Runs the suite directly when this
#       host/user can (ci_guarded_direct_viable); otherwise the battery is
#       handed to scripts/guarded-entry.sh, which measures with
#       guarded-service-user.sh --doctor and runs it as the dedicated service
#       user. Only when NEITHER route can run is a skip recorded — with the
#       remedy printed (ADR-0032 cl. 5), never as a suite pass.
#       A host precondition the suite itself cannot satisfy (QA04-07/08) is
#       reported as a SKIP with its remedy, never as a suite pass.
set -euo pipefail
cd "$(dirname "$0")/.."
. ./scripts/ci-guarded-job.sh

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  ok: %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
skip()  { printf '  SKIP: %s\n' "$*"; }

# --- T1: adversarial comms suite --------------------------------------------
step "T1: adversarial comms suite (scripted seams; SPEC §8)"
if T1_OUT="$(cargo test -p hamlet-daemon --test adversarial_comms --quiet 2>&1)"; then
  printf '%s\n' "$T1_OUT" | tail -3
  pass "adversarial_comms: all committed tests green"
else
  printf '%s\n' "$T1_OUT"
  fail "adversarial_comms suite"
fi

# --- T2: source-level ratchets ----------------------------------------------
step "T2: source-level ratchets (SPEC §8, ADR-0019/0020/0022)"

# LF-only discipline: a CR anywhere in the message is a typed rejection, so
# CRLF/CR-LF header splitting cannot smuggle extra headers into mail files.
if grep -qF "contains('\\r')" crates/hamlet-core/src/mail.rs; then
  pass "mail parse rejects CR (LF-only, ADR-0019 cl. 5)"
else
  fail "CR check missing from mail.rs — header-injection surface opened"
fi

# Webhook auth: constant-time compare (no timing side-channel) and fixed
# rejection tokens (input is never echoed into error artifacts).
if grep -q 'pub fn secret_eq' crates/hamlet-daemon/src/web.rs; then
  pass "webhook auth uses constant-time secret_eq"
else
  fail "secret_eq missing from web.rs — timing side-channel surface"
fi

TOKENS_OK=1
for token in unknown-topic bad-auth auth-unavailable payload-too-large malformed-json internal-error; do
  grep -q "\"$token\"" crates/hamlet-daemon/src/web.rs || TOKENS_OK=0
done
if [ "$TOKENS_OK" -eq 1 ]; then
  pass "all six rejection tokens fixed (sanitizer-clean, no echo)"
else
  fail "a rejection reason token changed — re-audit error artifacts for input echo"
fi

# Outbound redaction: the full authorization/cookie family is stripped from
# outbox/inbox artifacts before anything touches disk.
REDACT_OK=1
for header in authorization proxy-authorization cookie set-cookie x-api-key; do
  grep -q "\"$header\"" crates/hamlet-core/src/dispatch.rs || REDACT_OK=0
done
if [ "$REDACT_OK" -eq 1 ]; then
  pass "REDACTED_HEADERS covers the auth/cookie family (SPEC §8.4)"
else
  fail "a secret-bearing header left the redaction list"
fi

# Allowlist ENFORCEMENT (not just the matcher): the plane must actually call
# `allows()` before touching the network. QA04-10: mutation testing showed
# that deleting this call left adversarial_comms 27/27 GREEN, because the
# suite's allowlist test exercised only the pure `UrlAllowlist` matcher.
if grep -qF 'if !allowlist.allows(&request.url) {' crates/hamlet-daemon/src/dispatch.rs; then
  pass "dispatch plane enforces the URL allowlist (QA04-10)"
else
  fail "allowlist.allows() call missing from dispatch.rs — outbox files would POST anywhere"
fi

# Deterministic, jitter-free backoff (ADR-0020 cl. 4). QA04-11: adding jitter
# to the sleep (`ms + (attempt % 7) * 3`) kept the ENTIRE workspace green,
# because the only schedule assertions were on the pure `RetryPolicy::schedule()`
# plus a lower-bound wall-clock check. Wall-clock tolerances cannot pin
# ms-scale determinism, so the sleep expression itself is ratcheted.
if grep -qF 'tokio::time::sleep(Duration::from_millis(ms)).await;' crates/hamlet-daemon/src/dispatch.rs; then
  # `ms` must be the schedule value itself — never rebound or arithmetically
  # perturbed before the sleep.
  if grep -qE 'let (mut )?ms[ :]|ms *[-+*/]' crates/hamlet-daemon/src/dispatch.rs; then
    fail "backoff sleep perturbed — ADR-0020 cl. 4 requires the jitter-free schedule"
  else
    pass "backoff sleep is the bare deterministic schedule (QA04-11)"
  fi
else
  fail "backoff sleep expression changed — re-audit the deterministic schedule (ADR-0020 cl. 4)"
fi

# Quarantine is agent-readable, never agent-writable: the flag file is 0444
# and its directory 0755 — pinned constants, not scattered literals.
if grep -q 'MODE_QUARANTINE_FLAG: u32 = 0o444' crates/hamlet-daemon/src/freight.rs \
   && grep -q 'MODE_QUARANTINE_DIR: u32 = 0o755' crates/hamlet-daemon/src/freight.rs; then
  pass "quarantine constants pinned (flag 0444 / dir 0755)"
else
  fail "quarantine mode constants drifted — re-audit agent writability"
fi

# --- T3: guarded real-podman freight suite -----------------------------------
step "T3: guarded podman freight suite (SPEC §8.6; ADR-0011)"
if ! why="$(ci_guarded_direct_viable)"; then
  skip "direct route closed: ${why}"
  drc=0; ci_guarded_delegate "freight_guarded (via the service-user entry point)" || drc=$?
  case "$drc" in
    0) pass "freight_guarded: guarded battery green via the service-user entry point" ;;
    3) skip "freight_guarded: no route on this host (remedy printed above — nothing asserted)" ;;
    *) fail "guarded battery: a suite genuinely failed (log above)" ;;
  esac
else
  # Preflight: ADR-0011 needs a subuid range for the slot UIDs, and podman
  # must be rootless. Fail loudly before a cold (~200 MB) bootstrap.
  USER_NAME="$(id -un)"
  SUBUID_LINE="$(grep "^${USER_NAME}:" /etc/subuid 2>/dev/null || true)"
  if [ -z "$SUBUID_LINE" ]; then
    skip "no /etc/subuid entry for ${USER_NAME} — ADR-0011 subuid chown cannot work (QA04-07 remedy: a dedicated hamlet service user)"
  else
    # --ignored is REQUIRED: the suite is #[ignore]d (CONVENTIONS §5), so a
    # run without it executes ZERO tests and reports a vacuous green — the
    # CI04-01 class of lie, one step deeper. --nocapture so the suites'
    # recorded skip reasons reach the log instead of being swallowed.
    # The host lock (QA05-09) keeps a concurrent guarded job from deleting
    # this suite's containers mid-assertion.
    if ! ci_guarded_lock_or_report; then
      skip "another guarded job holds the host lock (see above) — not run concurrently"
    elif T3_OUT="$(cargo test -p hamlet-daemon --test freight_guarded -- --ignored podman --nocapture 2>&1)"; then
      printf '%s\n' "$T3_OUT" | tail -3
      if printf '%s\n' "$T3_OUT" | grep -q '^SKIP '; then
        printf '%s\n' "$T3_OUT" | grep '^SKIP ' | sed 's/^/  SKIP: /'
        skip "freight_guarded: host precondition unmet (reason above; see tests/support/mod.rs)"
      else
        pass "freight_guarded: real-podman quarantine/promotion suite green"
      fi
    else
      printf '%s\n' "$T3_OUT"
      fail "freight_guarded suite"
    fi
    ci_guarded_lock_release
  fi
fi

# --- summary -----------------------------------------------------------------
if [ "$FAILURES" -eq 0 ]; then
  printf '\033[1;32me2e-04-comms: PASS\033[0m\n'
else
  printf '\033[1;31me2e-04-comms: %s FAILURE(S)\033[0m\n' "$FAILURES"
  exit 1
fi
