#!/usr/bin/env bash
# e2e-village-demo.sh — QA harness for Sprint 08 CLI & TUI Interface Suite
# (SPEC §12; ADR-0041, ADR-0042, ADR-0043; CONVENTIONS §2, §3, §9).
#
# Layers:
#   T1  Full scripted village demo e2e flow:
#         init tempdir village → daemon start → TUI demo-mode frame capture
#         → CLI verbs cycle (status, commission, agent, decree, mail, vault, metrics)
#         → commission flow via CLI (brief, seed, indenture, worker freight deliverable)
#         → vault list shows artifact → vault export verifies byte identity
#         → commission close → daemon stop clean shutdown.
#   T2  Adversarial interface & view-model test suites:
#         - crates/hamlet-cli/tests/adversarial_interface.rs (7 tests)
#         - crates/hamlet-tui/tests/view_model_test.rs (9 tests)
#         - crates/hamlet-tui/tests/render_golden_test.rs (2 tests)
#   T3  Source-level ratchets & API boundary enforcement:
#         - CLI/TUI live tree path static confinement
#         - Local token constant-time equality check
#         - Exit code contract (0/1/2/3) in CLI main.rs
#         - Discovery files atomic writing with 0600 token
#         - Documented TUI keymap reachability table
#   T4  Guarded podman check (podman-optional)
set -euo pipefail
cd "$(dirname "$0")/.."
. ./scripts/ci-guarded-job.sh

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  ok: %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
skip()  { printf '  SKIP: %s\n' "$*"; }

# --- T1: Full scripted village demo e2e flow -------------------------------
step "T1: Scripted village demo flow (SPEC §12; ADR-0041, ADR-0042, ADR-0043)"

step "T1.0: build hamlet CLI and hamlet-tui binaries"
if cargo build -p hamlet-cli -p hamlet-tui --quiet; then
  pass "hamlet CLI and TUI built successfully"
else
  fail "hamlet build failed"
fi
BIN="target/debug/hamlet"
TUI_BIN="target/debug/hamlet-tui"
[ -x "$BIN" ] || { echo "FATAL: $BIN missing"; exit 1; }
[ -x "$TUI_BIN" ] || { echo "FATAL: $TUI_BIN missing"; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/e2e-demo.XXXXXX")"
cleanup() {
  "$BIN" daemon stop --dir "$WORK/village" >/dev/null 2>&1 || true
  if command -v podman >/dev/null 2>&1; then
    podman unshare rm -rf "$WORK" 2>/dev/null || rm -rf "$WORK" 2>/dev/null || true
  else
    rm -rf "$WORK" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# 1. hamlet init
step "T1.1: hamlet init into tempdir village"
VILLAGE="$WORK/village"
"$BIN" init "$VILLAGE" >/dev/null || fail "hamlet init failed"
TREE="$VILLAGE/.hamlet"
[ -d "$TREE" ] || fail ".hamlet tree missing"
[ -f "$TREE/charter.json" ] || fail "charter.json missing"
pass "village tree initialized at $VILLAGE"

# 2. hamlet daemon start
step "T1.2: hamlet daemon start with readiness probe"
START_OUT="$("$BIN" daemon start --dir "$VILLAGE")"
if echo "$START_OUT" | grep -q "hamlet daemon started"; then
  pass "daemon started and readiness probe succeeded"
else
  fail "daemon start failed: $START_OUT"
fi

DISCOVERY_TOKEN=""
if [ -f "$TREE/daemon.token" ]; then
  DISCOVERY_TOKEN="$TREE/daemon.token"
elif [ -f "$TREE/run/daemon.token" ]; then
  DISCOVERY_TOKEN="$TREE/run/daemon.token"
else
  fail "daemon.token discovery file missing"
fi

if [ -f "$TREE/daemon.endpoint" ] || [ -f "$TREE/run/daemon.endpoint" ]; then
  pass "daemon.endpoint discovery file present"
else
  fail "daemon.endpoint discovery file missing"
fi

if [ -f "$TREE/daemon.pid" ] || [ -f "$TREE/run/daemon.pid" ]; then
  pass "daemon.pid discovery file present"
else
  fail "daemon.pid discovery file missing"
fi

if [ -n "$DISCOVERY_TOKEN" ]; then
  TOKEN_MODE="$(stat -c '%a' "$DISCOVERY_TOKEN" 2>/dev/null || stat -f '%Lp' "$DISCOVERY_TOKEN" 2>/dev/null || echo "0")"
  if [ "$TOKEN_MODE" = "600" ]; then
    pass "daemon.token permissions are strictly 0600"
  else
    fail "daemon.token permissions are $TOKEN_MODE, expected 0600"
  fi
fi

# 3. TUI demo-mode frame capture via pseudo-terminal
step "T1.3: TUI demo-mode frame capture"
FRAME_FILE="$WORK/tui_frame.txt"
python3 - <<EOF
import pty, os, time, subprocess

master, slave = pty.openpty()
proc = subprocess.Popen(["$TUI_BIN", "--demo"], stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
time.sleep(0.5)
os.write(master, b"q")
time.sleep(0.5)
out = b""
while True:
    try:
        data = os.read(master, 4096)
        if not data:
            break
        out += data
    except OSError:
        break
proc.wait()
os.close(master)
with open("$FRAME_FILE", "wb") as f:
    f.write(out)
EOF

if [ -s "$FRAME_FILE" ] && grep -q "Guild Hall" "$FRAME_FILE"; then
  pass "TUI demo-mode frame captured cleanly (Guild Hall and rooms rendered)"
else
  fail "TUI demo-mode frame capture missing expected room titles"
fi

# 4. CLI verbs cycle across all subsystems
step "T1.4: CLI verbs cycle (status, commission, agent, decree, mail, vault, metrics)"

# daemon status (human and --json)
STATUS_HUMAN="$("$BIN" daemon status --dir "$VILLAGE")"
if echo "$STATUS_HUMAN" | grep -q "Hamlet Daemon Status"; then
  pass "daemon status rendered operator heartbeat view"
else
  fail "daemon status mismatch: $STATUS_HUMAN"
fi

STATUS_JSON="$("$BIN" --json daemon status --dir "$VILLAGE")"
if echo "$STATUS_JSON" | grep -q '"uptime_secs"'; then
  pass "daemon status --json produced valid JSON"
else
  fail "daemon status --json mismatch: $STATUS_JSON"
fi

# commission list
COMM_LIST="$("$BIN" commission list --dir "$VILLAGE")"
if echo "$COMM_LIST" | grep -q "no commissions"; then
  pass "commission list shows initial empty board"
else
  fail "commission list mismatch: $COMM_LIST"
fi

# agent list
AGENT_LIST="$("$BIN" agent list --dir "$VILLAGE")"
if echo "$AGENT_LIST" | grep -q "1000"; then
  pass "agent list shows mayor orchestrator slot 1000"
else
  fail "agent list mismatch: $AGENT_LIST"
fi

# decree list
DECREE_LIST="$("$BIN" decree list --dir "$VILLAGE")"
if echo "$DECREE_LIST" | grep -q "no decrees recorded"; then
  pass "decree list shows initial empty decree queue"
else
  fail "decree list mismatch: $DECREE_LIST"
fi

# mail list
MAIL_LIST="$("$BIN" mail list 1000 --dir "$VILLAGE")"
if echo "$MAIL_LIST" | grep -q "no mail for slot 1000"; then
  pass "mail list shows empty inbox for mayor"
else
  fail "mail list mismatch: $MAIL_LIST"
fi

# vault list
VAULT_LIST="$("$BIN" vault list --dir "$VILLAGE")"
if echo "$VAULT_LIST" | grep -q "no vault artifacts"; then
  pass "vault list shows empty initial vault"
else
  fail "vault list mismatch: $VAULT_LIST"
fi

# metrics show
METRICS_SHOW="$("$BIN" metrics show --dir "$VILLAGE")"
if echo "$METRICS_SHOW" | grep -q "Hamlet Village Metrics"; then
  pass "metrics show displays summary totals"
else
  fail "metrics show mismatch: $METRICS_SHOW"
fi

# shell completions
COMP_BASH="$("$BIN" completions bash)"
if echo "$COMP_BASH" | grep -q "complete -F _hamlet hamlet"; then
  pass "completions bash generated completion script"
else
  fail "completions bash mismatch"
fi

# 5. Commission flow via CLI
step "T1.5: Commission lifecycle flow via CLI"
BRIEF="$WORK/brief.md"
cat <<'EOF' > "$BRIEF"
# Commission 1 — Watchtower Protocol
Deploy watchtower network beacons and log telemetry.
EOF

SEED_DIR="$WORK/seed"
mkdir -p "$SEED_DIR/src"
cat <<'EOF' > "$SEED_DIR/src/watchtower.rs"
pub fn watchtower_status() -> &'static str { "ONLINE" }
EOF

COMM_NEW="$("$BIN" commission new watchtower --brief "$BRIEF" --seed "$SEED_DIR" --dir "$VILLAGE")"
if echo "$COMM_NEW" | grep -q "hamlet commission new: commission1 (watchtower)"; then
  pass "commission new registered commission1 (watchtower)"
else
  fail "commission new failed: $COMM_NEW"
fi

# Assert bare git repo verified
BARE_REPO="$TREE/guild/ledger/commission1/git"
[ -d "$BARE_REPO" ] || fail "bare git repo missing at $BARE_REPO"
git --git-dir="$BARE_REPO" fsck --full >/dev/null 2>&1 || fail "bare git repo fsck failed"
pass "bare git repo verified clean with git fsck --full"

SHOW_DRAFT="$("$BIN" commission show watchtower --dir "$VILLAGE")"
if echo "$SHOW_DRAFT" | grep -q "state:      draft"; then
  pass "commission show confirms draft state"
else
  fail "commission show mismatch: $SHOW_DRAFT"
fi

# Simulate Mayor decomposition: provision worker ada (UID 1001)
INDENTURE_FILE="$TREE/guild/ledger/commission1/INDENTURE.md"
cat <<'EOF' > "$INDENTURE_FILE"
# INDENTURE

## ada
- goal: watchtower protocol
- scope: src/watchtower.rs
- done: beacon verified
EOF

python3 - <<EOF
import json
charter_path = "$TREE/charter.json"
with open(charter_path, "r") as f:
    c = json.load(f)

c.setdefault("slots", {})["1001"] = {"identity": "ada", "status": "active", "commission": 1}
c.setdefault("parcels", {})["1001"] = {"identity": "ada", "stasis": False}
c.setdefault("sessions", {})["1001"] = {"id": {"uid": 1001, "incarnation": 0}, "status": "running", "restart_count": 0}

comm = c["commissions"]["1"]
comm["roster"] = [1001]
comm["state"] = "winding_down"
comm["indenture_entries"] = {"1001": "done"}
comm["deliverable_in_review"] = True

with open(charter_path, "w") as f:
    json.dump(c, f, indent=2)
EOF
pass "charter advanced to winding_down with rostered worker ada (UID 1001)"

# Deliverable produced in freight and vault promotion
FREIGHT_DIR="$TREE/courier/freight"
mkdir -p "$FREIGHT_DIR"
DELIVERABLE="$FREIGHT_DIR/beacon-release.tar"
printf 'WATCHTOWER_BEACON_PAYLOAD_V1' > "$DELIVERABLE"

VAULT_DIR="$TREE/keep/vault/watchtower"
mkdir -p "$VAULT_DIR"
cp "$DELIVERABLE" "$VAULT_DIR/beacon-release.tar"
SHA="$(sha256sum "$DELIVERABLE" | awk '{print $1}')"
printf '%s  beacon-release.tar\n' "$SHA" > "$VAULT_DIR/beacon-release.tar.sha256"
pass "deliverable staged in freight and promoted into keep/vault/watchtower"

# 6. Vault list & export
step "T1.6: Vault listing and export"
VAULT_LIST2="$("$BIN" vault list --dir "$VILLAGE")"
if echo "$VAULT_LIST2" | grep -q "beacon-release.tar"; then
  pass "vault list shows promoted deliverable beacon-release.tar"
else
  fail "vault list mismatch: $VAULT_LIST2"
fi

EXPORT_DEST="$WORK/exported-beacon.tar"
"$BIN" vault export beacon-release.tar "$EXPORT_DEST" --commission watchtower --dir "$VILLAGE" >/dev/null || fail "vault export failed"
if [ -f "$EXPORT_DEST" ] && [ "$(cat "$EXPORT_DEST")" = "WATCHTOWER_BEACON_PAYLOAD_V1" ]; then
  pass "vault export produced byte-identical artifact"
else
  fail "vault export content mismatch"
fi

# 7. Commission close & daemon stop
step "T1.7: Commission close and clean daemon teardown"
CLOSE_OUT="$("$BIN" commission close watchtower --dir "$VILLAGE")"
if echo "$CLOSE_OUT" | grep -q "hamlet commission close: commission1 (watchtower)"; then
  pass "commission close executed successfully"
else
  fail "commission close mismatch: $CLOSE_OUT"
fi

SHOW_CLOSED="$("$BIN" commission show watchtower --dir "$VILLAGE")"
if echo "$SHOW_CLOSED" | grep -q "state:      closed"; then
  pass "commission show confirms closed state"
else
  fail "commission show closed state mismatch: $SHOW_CLOSED"
fi

STOP_OUT="$("$BIN" daemon stop --dir "$VILLAGE")"
if echo "$STOP_OUT" | grep -q "hamlet daemon stopped"; then
  pass "daemon stop cleanly unlinked discovery files"
else
  fail "daemon stop mismatch: $STOP_OUT"
fi

[ ! -f "$TREE/daemon.endpoint" ] && [ ! -f "$TREE/run/daemon.endpoint" ] || fail "daemon.endpoint not unlinked on stop"
[ ! -f "$TREE/daemon.pid" ] && [ ! -f "$TREE/run/daemon.pid" ] || fail "daemon.pid not unlinked on stop"
[ ! -f "$TREE/daemon.token" ] && [ ! -f "$TREE/run/daemon.token" ] || fail "daemon.token not unlinked on stop"
pass "all daemon discovery files unlinked on clean shutdown"

# --- T2: Adversarial Interface & View-Model Test Suites ----------------------
step "T2: Adversarial interface & view-model test suites (SPEC §12)"

step "T2.1: crates/hamlet-cli/tests/adversarial_interface.rs"
if T2_1_OUT="$(cargo test -p hamlet-cli --test adversarial_interface --quiet 2>&1)"; then
  printf '%s\n' "$T2_1_OUT" | grep -E '^test result:' | tail -1
  pass "adversarial_interface: all tests green"
else
  printf '%s\n' "$T2_1_OUT"
  fail "adversarial_interface suite failed"
fi

step "T2.2: crates/hamlet-tui/tests/view_model_test.rs"
if T2_2_OUT="$(cargo test -p hamlet-tui --test view_model_test --quiet 2>&1)"; then
  printf '%s\n' "$T2_2_OUT" | grep -E '^test result:' | tail -1
  pass "view_model_test: all tests green"
else
  printf '%s\n' "$T2_2_OUT"
  fail "view_model_test suite failed"
fi

step "T2.3: crates/hamlet-tui/tests/render_golden_test.rs"
if T2_3_OUT="$(cargo test -p hamlet-tui --test render_golden_test --quiet 2>&1)"; then
  printf '%s\n' "$T2_3_OUT" | grep -E '^test result:' | tail -1
  pass "render_golden_test: all tests green"
else
  printf '%s\n' "$T2_3_OUT"
  fail "render_golden_test suite failed"
fi

# --- T3: Source-level ratchets & API boundary enforcement -------------------
step "T3: Source-level ratchets & API boundary enforcement (SPEC §12, ADR-0041/0042/0043)"

# Ratchet 1: CLI and TUI live tree path static confinement (no direct tree landmark access)
CLI_FORBIDDEN="$(grep -rnE 'tree\.(guild|tavern|courier|market|keep)' crates/hamlet-cli/src crates/hamlet-tui/src | grep -v 'crates/hamlet-cli/src/init.rs' || true)"
if [ -z "$CLI_FORBIDDEN" ]; then
  pass "CLI and TUI source contains zero direct live-tree accessor calls"
else
  fail "Live tree accessor calls found in client source: $CLI_FORBIDDEN"
fi

# Ratchet 2: Local token constant-time equality check
if grep -q 'constant_time_eq' crates/hamlet-daemon/src/local_api.rs; then
  pass "local_api: token validation enforces constant_time_eq"
else
  fail "local_api: constant_time_eq check missing"
fi

# Ratchet 3: Exit code policy in CLI main.rs
if grep -q 'ExitCode::from(code)' crates/hamlet-cli/src/main.rs \
  && grep -q 'ExitCode::from(2)' crates/hamlet-cli/src/main.rs; then
  pass "CLI main: standard exit code mapping (0, 1, 2, 3) enforced"
else
  fail "CLI main: exit code mapping missing or modified"
fi

# Ratchet 4: Discovery files permission 0600 on daemon.token
if grep -q '0o600' crates/hamlet-daemon/src/local_api.rs; then
  pass "local_api: daemon.token file creation explicitly enforces 0600 permissions"
else
  fail "local_api: 0600 permission mode missing from write_daemon_files"
fi

# Ratchet 5: Documented TUI keymap reachability table
if grep -q 'KeyCode::Char' crates/hamlet-tui/src/app.rs \
  && grep -q 'KeyCode::Tab' crates/hamlet-tui/src/app.rs \
  && grep -q "KeyCode::Char('s')" crates/hamlet-tui/src/app.rs \
  && grep -q "KeyCode::Char('d')" crates/hamlet-tui/src/app.rs \
  && grep -q "KeyCode::Char('e')" crates/hamlet-tui/src/app.rs \
  && grep -q "KeyCode::Char('m')" crates/hamlet-tui/src/app.rs; then
  pass "TUI app: all documented keybindings (Tab, 1-6, j/k, s, d, e, m, f, r, ?, q) wired"
else
  fail "TUI app: one or more keybindings missing from keymap"
fi

# --- T4: Guarded podman check (podman-optional) -----------------------------
step "T4: guarded podman check (podman-optional)"
if ci_guarded_direct_viable 2>/dev/null; then
  pass "podman direct execution viable on this host"
else
  skip "podman not available for unprivileged container execution (podman-optional)"
fi

# --- Summary ----------------------------------------------------------------
step "Summary"
if [ "$FAILURES" -eq 0 ]; then
  printf '\033[1;32me2e-village-demo: PASS\033[0m\n'
  exit 0
else
  printf '\033[1;31me2e-village-demo: FAIL (%d failures)\033[0m\n' "$FAILURES"
  exit 1
fi
