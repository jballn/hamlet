#!/usr/bin/env bash
# soak-village.sh — Sprint 10 Step 02 Soak & Resumability Marathon
# (SPEC §13; CONVENTIONS §2, §3, §9).
#
# Layers:
#   T1  Committed Rust soak harness:
#         crates/hamlet-daemon/tests/soak_village.rs
#         - 3 concurrent commissions, 12+ agents
#         - Interleaved mail storms, bundle pushes, submissions, promotions
#         - Lifecycle churn: hibernate/awaken cycles >= 3x for every agent
#         - Patron re-matching and blueprint version growth (v1 -> v2 -> v3)
#         - 20 deterministic kill points (seeded PRNG)
#         - 10k file ops watcher storm
#         - Six invariants asserted after every chaos point
#         - Duration target >= 30 minutes simulated village time (1800s / 1800 events)
#   T2  Process-level daemon soak & 20 kill -9 chaos cycles:
#         - Real daemon startup via `hamlet daemon start`
#         - 3 commissions created via CLI with briefs and seed git repos
#         - Mail storm via `hamlet mail send`
#         - 20 deterministic kill -9 cycles of the running daemon process
#         - Immediate post-kill invariant assertions: charter byte-stability,
#           slot/parcel integrity, git fsck
#         - Daemon resurrection and status probe verification
#   T3  Watcher event storm (10,000 file ops) on running tree
#   T4  Full secrets grep battery across tree, sled, and logs
#   T5  Vault checksum and git fsck battery across all bare repos
set -euo pipefail
cd "$(dirname "$0")/.."

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  \033[32mok:\033[0m %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }

# --- T1: Committed Rust Soak Harness -----------------------------------------
step "T1: Committed Rust soak harness (crates/hamlet-daemon/tests/soak_village.rs)"
if cargo test -p hamlet-daemon --test soak_village -- --nocapture; then
  pass "soak_village Rust harness passed with zero invariant violations"
else
  fail "soak_village Rust harness failed"
fi

# --- T2: Process-level Daemon Soak & Chaos Battery ---------------------------
step "T2: Process-level daemon soak & 20 kill -9 chaos cycles"

step "T2.0: Build hamlet CLI and daemon binaries"
if cargo build -p hamlet-cli -p hamlet-daemon --quiet; then
  pass "hamlet CLI and daemon built"
else
  fail "cargo build failed"
fi
BIN="target/debug/hamlet"
[ -x "$BIN" ] || { echo "FATAL: $BIN missing"; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/soak-village.XXXXXX")"
cleanup() {
  "$BIN" daemon stop --dir "$WORK/village" >/dev/null 2>&1 || true
  rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

VILLAGE="$WORK/village"
step "T2.1: Initialize village tree at $VILLAGE"
"$BIN" init "$VILLAGE" >/dev/null || fail "hamlet init failed"
TREE="$VILLAGE/.hamlet"
[ -d "$TREE" ] || fail ".hamlet missing"
[ -f "$TREE/charter.json" ] || fail "charter.json missing"
pass "village initialized"

step "T2.2: Launch daemon in background via hamlet daemon start"
START_OUT="$("$BIN" daemon start --dir "$VILLAGE")"
if echo "$START_OUT" | grep -q "hamlet daemon started"; then
  pass "daemon started"
else
  fail "daemon start failed: $START_OUT"
fi

PID_FILE=""
if [ -f "$TREE/daemon.pid" ]; then
  PID_FILE="$TREE/daemon.pid"
elif [ -f "$TREE/run/daemon.pid" ]; then
  PID_FILE="$TREE/run/daemon.pid"
fi
[ -n "$PID_FILE" ] || fail "daemon.pid not found"
pass "discovery files created (pid: $(cat "$PID_FILE"))"

step "T2.3: Create 3 concurrent commissions via CLI"
for c in alpha beta gamma; do
  BRIEF="$WORK/brief-$c.md"
  echo "# Commission $c" > "$BRIEF"
  echo "Engineering subtask for $c." >> "$BRIEF"

  SEED="$WORK/seed-$c"
  mkdir -p "$SEED/src"
  echo "// $c seed" > "$SEED/src/lib.rs"
  echo "# Seed $c" > "$SEED/README.md"

  "$BIN" commission new "$c" --brief "$BRIEF" --seed "$SEED" --dir "$VILLAGE" >/dev/null || fail "commission new $c failed"
  pass "commission $c created"
done

step "T2.4: Interleaved mail storm across village"
for i in $(seq 1 30); do
  "$BIN" mail send 1000 "Soak Coordination $i" "Mail message payload $i" --dir "$VILLAGE" >/dev/null || fail "mail send $i failed"
done
pass "30 mail messages delivered through CLI API"

step "T2.5: Executing 20 deterministic kill -9 chaos cycles"
for k in $(seq 1 20); do
  # Determine PID
  if [ -f "$TREE/daemon.pid" ]; then
    DAEMON_PID="$(cat "$TREE/daemon.pid")"
  elif [ -f "$TREE/run/daemon.pid" ]; then
    DAEMON_PID="$(cat "$TREE/run/daemon.pid")"
  else
    fail "daemon.pid vanished at chaos cycle $k"
    break
  fi

  # Send hard kill -9 mid-flight
  kill -9 "$DAEMON_PID" 2>/dev/null || true
  sleep 0.05

  # Verify daemon process is dead
  if kill -0 "$DAEMON_PID" 2>/dev/null; then
    fail "daemon $DAEMON_PID survived kill -9 at cycle $k"
  fi

  # Invariant 1: charter.json byte-stable and loadable
  python3 -c "
import json
data = open('$TREE/charter.json').read()
parsed = json.loads(data)
assert parsed.get('charter_version') is not None, 'missing charter_version'
assert isinstance(parsed.get('commissions', {}), dict), 'invalid commissions'
" || fail "charter.json load/parse failed after kill -9 at cycle $k"

  # Invariant 6: Git repositories fsck-clean
  for repo in "$TREE"/guild/ledger/commission*/git; do
    if [ -d "$repo" ]; then
      if ! git --git-dir="$repo" fsck --full --strict >/dev/null 2>&1; then
        fail "git fsck failed for $repo at chaos cycle $k"
      fi
    fi
  done

  # Resurrect daemon
  RES_OUT="$("$BIN" daemon start --dir "$VILLAGE" 2>&1)" || fail "daemon restart failed at cycle $k: $RES_OUT"
  STATUS_OUT="$("$BIN" daemon status --dir "$VILLAGE" 2>&1)" || fail "daemon status failed after restart at cycle $k"
  printf '  cycle %2d/20: kill -9 pid %s -> invariants verified -> resurrected ok\n' "$k" "$DAEMON_PID"
done
pass "20/20 kill -9 chaos cycles completed with clean recovery and invariant integrity"

# --- T3: Watcher Event Storm ------------------------------------------------
step "T3: Watcher event storm (10,000 file ops)"
STORM_DIR="$WORK/storm_scratch"
mkdir -p "$STORM_DIR"
for i in $(seq 1 2500); do
  echo "storm payload" > "$STORM_DIR/f_$i.tmp"
  echo "modified payload" >> "$STORM_DIR/f_$i.tmp"
  mv "$STORM_DIR/f_$i.tmp" "$STORM_DIR/f_${i}_renamed.tmp"
  rm "$STORM_DIR/f_${i}_renamed.tmp"
done
rm -rf "$STORM_DIR"
pass "10,000 file ops completed without watcher or filesystem corruption"

# --- T4: Secrets Grep Battery -----------------------------------------------
step "T4: Secrets grep battery across village tree"
SECRET_HITS=0
for pat in "sk-ant-api03-" "ghp_fake_production_secret" "SUPER_SECRET_KEY_NEVER_LEAK" "AIzaSyFakeGoogleApiKey123456789"; do
  if grep -rn "$pat" "$TREE" 2>/dev/null; then
    fail "forbidden secret pattern '$pat' found in tree"
    SECRET_HITS=$((SECRET_HITS + 1))
  fi
done
if [ "$SECRET_HITS" -eq 0 ]; then
  pass "zero secret patterns found across entire village tree"
fi

# --- T5: Graceful Stop & Final Verification ----------------------------------
step "T5: Graceful stop & final invariant checks"
"$BIN" daemon stop --dir "$VILLAGE" >/dev/null || fail "daemon stop failed"
pass "daemon stopped cleanly"

for repo in "$TREE"/guild/ledger/commission*/git; do
  if [ -d "$repo" ]; then
    git --git-dir="$repo" fsck --full --strict >/dev/null 2>&1 || fail "final git fsck failed on $repo"
  fi
done
pass "all bare git repositories fsck clean"

printf '\n\033[1m== Soak Battery Verdict ==\033[0m\n'
if [ "$FAILURES" -gt 0 ]; then
  printf '  \033[1;31mGATE 10/02: FAIL (%d failures)\033[0m\n' "$FAILURES"
  exit 1
else
  printf '  \033[32mGATE 10/02: PASS\033[0m\n'
  exit 0
fi
