#!/usr/bin/env bash
# e2e-01-topology-state.sh — QA prompt 01/05, CLI-level adversarial e2e for
# `hamlet init` + tree/state invariants observable from the real binary
# (SPEC §2 topology, §11.4 .env 0600, CONVENTIONS §3 state contract).
#
# Rust-level adversarial coverage (sanitizer sweeps, charter atomicity,
# SIGKILL races, UID slots, resumability) lives in:
#   crates/hamlet-core/tests/adversarial_sanitize.rs
#   crates/hamlet-core/tests/adversarial_state.rs
#   crates/hamlet-cli/tests/adversarial_init.rs
# Container coverage lives in the guarded battery (e2e-02 T4 / e2e-04 T3 /
# e2e-05 T3, all through scripts/guarded-entry.sh); T8 below only ACCOUNTS for
# it, so a run that silently asserted nothing cannot hide in this log.
set -euo pipefail
cd "$(dirname "$0")/.."
. ./scripts/ci-guarded-job.sh

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  ok: %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
check() { # check <desc> <expected> <actual>
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (expected [$2], got [$3])"; fi
}
# Accounting-only line: T8 asserts nothing, so it must never touch FAILURES —
# an unreachable container job is a coverage note, not a red in this harness.
skip()  { printf '  \033[1;33mSKIP: %s\033[0m\n' "$*"; }

step "building hamlet CLI"
cargo build -p hamlet-cli --quiet
BIN="target/debug/hamlet"
[ -x "$BIN" ] || { echo "FATAL: $BIN missing"; exit 1; }

WORK="$(mktemp -d)"
trap 'chmod -R u+rwX "$WORK" 2>/dev/null || true; rm -rf "$WORK"' EXIT

mode() { stat -c '%a' "$1"; }

# --- T1: fresh init creates the full normative tree ------------------------
step "T1: fresh init into a missing nested dir"
V1="$WORK/v1"
"$BIN" init "$V1/deep/target" >/dev/null || fail "T1 init exit"
R="$V1/deep/target/.hamlet"
DIRS="guild guild/register guild/blueprints guild/ledger tavern tavern/gossip
tavern/patrons courier courier/mail courier/freight courier/dispatch
courier/dispatch/outbox courier/dispatch/inbox market market/skills
market/pkgcache keep keep/vault keep/archives keep/census keep/census/manor
keep/census/manor/home"
MISSING=""
for d in $DIRS; do [ -d "$R/$d" ] || MISSING="$MISSING $d"; done
check "T1 all normative dirs exist (SPEC §2)" "" "$MISSING"
for f in .env charter.json config.json; do
  [ -f "$R/$f" ] || fail "T1 missing top-level file $f"
done
check "T1 dir mode 0755 (guild)"        "755" "$(mode "$R/guild")"
check "T1 dir mode 0755 (manor/home)"  "755" "$(mode "$R/keep/census/manor/home")"
check "T1 charter.json mode 0644"      "644" "$(mode "$R/charter.json")"
check "T1 config.json mode 0644"       "644" "$(mode "$R/config.json")"
check "T1 .env mode EXACTLY 0600"      "600" "$(mode "$R/.env")"
check "T1 empty charter golden" '{
  "charter_version": 3
}' "$(cat "$R/charter.json")"

# --- T2: re-init idempotency (byte + mode stability) ------------------------
step "T2: re-init is a complete no-op"
BEFORE="$(cd "$R" && find . -type f -exec sha256sum {} + | sort)"
MODES_BEFORE="$(cd "$R" && find . -exec stat -c '%a %n' {} + | sort)"
"$BIN" init "$V1/deep/target" | grep -q "exists — untouched" || fail "T2 report should say exists"
AFTER="$(cd "$R" && find . -type f -exec sha256sum {} + | sort)"
MODES_AFTER="$(cd "$R" && find . -exec stat -c '%a %n' {} + | sort)"
check "T2 file bytes unchanged"  "$BEFORE" "$AFTER"
check "T2 modes unchanged"       "$MODES_BEFORE" "$MODES_AFTER"
[ -z "$(find "$R" -name '*.tmp')" ] && pass "T2 no temp litter" || fail "T2 temp litter found"

# --- T3: tampered .env mode is re-tightened, content preserved --------------
step "T3: .env tamper → re-init tightens to 0600"
echo "HAMLET_MODEL_API_KEY=test-key-do-not-use" > "$R/.env"
chmod 0644 "$R/.env"
"$BIN" init "$V1/deep/target" >/dev/null
check "T3 .env re-tightened" "600" "$(mode "$R/.env")"
check "T3 .env content preserved" "HAMLET_MODEL_API_KEY=test-key-do-not-use" "$(cat "$R/.env")"

# --- T4: .env exactly 0600 under umask 000 ----------------------------------
step "T4: fresh init under umask 000"
( umask 000; "$BIN" init "$WORK/v4" >/dev/null )
check "T4 .env 0600 under umask 000"    "600" "$(mode "$WORK/v4/.hamlet/.env")"
check "T4 charter 0644 under umask 000" "644" "$(mode "$WORK/v4/.hamlet/charter.json")"
check "T4 dir 0755 under umask 000"     "755" "$(mode "$WORK/v4/.hamlet/keep/vault")"

# --- T5: hostile targets ----------------------------------------------------
step "T5: .hamlet occupied by a regular file is refused"
mkdir -p "$WORK/v5" && echo "not a dir" > "$WORK/v5/.hamlet"
rc=0; "$BIN" init "$WORK/v5" >/dev/null 2>&1 || rc=$?
check "T5 init exits non-zero" "1" "$rc"
check "T5 squatter untouched"  "not a dir" "$(cat "$WORK/v5/.hamlet")"

step "T5b: read-only working dir is refused with typed error"
if [ "$(id -u)" = "0" ]; then
  echo "  SKIP: running as root (permission bits not enforceable)"
else
  mkdir -p "$WORK/v6" && chmod 0555 "$WORK/v6"
  rc=0; ERR="$( "$BIN" init "$WORK/v6" 2>&1 )" || rc=$?
  chmod 0755 "$WORK/v6"
  check "T5b init exits non-zero" "1" "$rc"
  case "$ERR" in *"error:"*) pass "T5b stderr names the failure: $ERR";; *) fail "T5b no typed error message: $ERR";; esac
fi

# --- T6: usage & exit codes --------------------------------------------------
step "T6: usage errors exit 2"
rc=0; "$BIN" init >/dev/null 2>&1 || rc=$?
check "T6 'init' without dir exits 2" "2" "$rc"
rc=0; "$BIN" bogus >/dev/null 2>&1 || rc=$?
check "T6 unknown command exits 2"    "2" "$rc"
"$BIN" --version | grep -q "hamlet" && pass "T6 --version works" || fail "T6 --version"

# --- T7: secrets hygiene — template holds no real secrets --------------------
step "T7: .env template hygiene (CONVENTIONS §6)"
grep -Eq 'test-key-do-not-use|example\.invalid|^#|^$' "$WORK/v4/.hamlet/.env" \
  && pass "T7 template uses obvious fakes only" \
  || fail "T7 unexpected content in .env template"
if grep -RIn --exclude=.env -E 'test-key-do-not-use' "$WORK/v4/.hamlet" | grep -q .; then
  fail "T7 secret-looking value leaked outside .env"
else
  pass "T7 no secret material outside .env"
fi

# --- T8: guarded-battery accounting -----------------------------------------
# No assertion here; this is the coverage ledger. The podman-capable job runs
# as the service user via scripts/guarded-entry.sh; if the direct route is
# closed, the exact command that would run it is named rather than skipped
# silently (ADR-0032 cl. 5 — a skip is a coverage hole, not a pass).
step "T8: guarded-battery accounting (SPEC §4 uid policy lives there)"
if why="$(ci_guarded_direct_viable 2>&1)"; then
  pass "podman-capable job runnable here as $(id -un): ${why:-all preconditions met}"
else
  skip "guarded battery not runnable as $(id -un): ${why}"
  skip "run it with: sudo -H -u ${HAMLET_SERVICE_USER:-hamlet} env TMPDIR=${HAMLET_WORK_AREA:-/var/hamlet/tmp} HAMLET_PODMAN_TESTS=1 ./scripts/guarded-service-user.sh"
fi

echo
if [ "$FAILURES" -ne 0 ]; then
  printf '\033[1;31me2e-01-topology-state: FAIL (%s assertion(s))\033[0m\n' "$FAILURES"
  exit 1
fi
printf '\033[1;32me2e-01-topology-state: PASS\033[0m\n'
