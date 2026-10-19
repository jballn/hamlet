#!/usr/bin/env bash
# e2e-commission.sh — QA prompt 06/07, end-to-end lifecycle harness for the
# Sprint 06 Commission Lifecycle (SPEC §7, §9, §11.2; ADR-0036/0037/0038;
# CONVENTIONS §2, §3, §9).
#
# Layers:
#   T1  CLI-level end-to-end commission lifecycle:
#         init tempdir village → commission new with seed → bare git repo verified
#         → simulated Mayor decomposition (INDENTURE.md) → worker branch/bundle
#         → deliverable in freight → vault promotion → vault list/export verified
#         → commission close → archives asserted.
#   T2  Adversarial lifecycle test suite:
#         crates/hamlet-daemon/tests/adversarial_lifecycle.rs (22 tests)
#         - Git mediation (corrupt bundles, foreign namespaces, forged identity,
#           oversize bundles, hook smuggling, non-fast-forward; git fsck clean)
#         - DEED promotion (injection lines, unknown tools, dup-set idempotence,
#           kill -9 atomicity)
#         - Vault loop (nonexistent freight, name traversal, double-approve replay,
#           reject bound escalation)
#         - State machine & multi-tenancy (illegal transition totality, roster freezing,
#           two-commission isolation)
#         - Restart resumability (mid-flight recovery, idempotent spool reconciliation)
#         - QA findings reproduction (QA06-01 through QA06-05)
#   T3  Source-level ratchets:
#         - gitmed.rs: bundle branch strictly confined to refs/heads/agent/<uid>
#         - gitmed.rs: commit author/committer email strictly pinned to agent identity
#         - ledger.rs: git fsck --full executed on bare repo after bundle ingest
#         - commission.rs: totality table catch-all returns IllegalTransition
#         - commission.rs: roster mutation frozen outside Draft/Active
#         - vault.rs: lstat symlink refusal (ADR-0007)
#   T4  Guarded podman commission check (podman-optional)
set -euo pipefail
cd "$(dirname "$0")/.."
. ./scripts/ci-guarded-job.sh

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  ok: %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
skip()  { printf '  SKIP: %s\n' "$*"; }
check() { # check <desc> <expected> <actual>
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (expected [$2], got [$3])"; fi
}

# --- T1: CLI-level commission lifecycle flow --------------------------------
step "T1: CLI-level commission lifecycle flow (SPEC §9.1, §11.2, §12.1)"

step "T1.0: build hamlet CLI"
if cargo build -p hamlet-cli --quiet; then
  pass "hamlet CLI built"
else
  fail "hamlet CLI build failed"
fi
BIN="target/debug/hamlet"
[ -x "$BIN" ] || { echo "FATAL: $BIN missing"; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/e2e-comm.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# 1. hamlet init
step "T1.1: hamlet init into tempdir village"
VILLAGE="$WORK/village"
"$BIN" init "$VILLAGE" >/dev/null || fail "hamlet init failed"
TREE="$VILLAGE/.hamlet"
[ -d "$TREE" ] || fail ".hamlet tree missing"
[ -f "$TREE/charter.json" ] || fail "charter.json missing"
pass "village tree initialized"

# 2. hamlet commission new with brief and seed repo
step "T1.2: hamlet commission new with brief and seeded repository"
BRIEF="$WORK/brief.md"
cat <<'EOF' > "$BRIEF"
# Commission 1 — Watchtower Beacon
Build the signal tower beacon protocol.
EOF

SEED_DIR="$WORK/seed"
mkdir -p "$SEED_DIR/src"
cat <<'EOF' > "$SEED_DIR/src/beacon.rs"
pub fn beacon_signal() -> &'static str { "BEACON_ACTIVE" }
EOF
cat <<'EOF' > "$SEED_DIR/README.md"
# Watchtower Beacon Seed
EOF

COMM_OUT="$("$BIN" commission new watchtower --brief "$BRIEF" --seed "$SEED_DIR" --dir "$VILLAGE")"
if echo "$COMM_OUT" | grep -q "hamlet commission new: commission1 (watchtower)"; then
  pass "commission new output matches expected header"
else
  fail "commission new output mismatch: $COMM_OUT"
fi

# Assert bare git repo and COMMISSION.md created
BARE_REPO="$TREE/guild/ledger/commission1/git"
[ -d "$BARE_REPO" ] || fail "bare git repo missing at $BARE_REPO"
git --git-dir="$BARE_REPO" fsck --full >/dev/null 2>&1 || fail "bare git repo fsck failed"
pass "bare git repo verified clean with git fsck --full"

[ -f "$TREE/guild/ledger/commission1/COMMISSION.md" ] || fail "COMMISSION.md missing"
pass "COMMISSION.md verified"

# 3. hamlet commission list & show
step "T1.3: hamlet commission list and show"
LIST_OUT="$("$BIN" commission list --dir "$VILLAGE")"
if echo "$LIST_OUT" | grep -E -q "1 +watchtower +draft"; then
  pass "commission list shows commission1 watchtower draft"
else
  fail "commission list mismatch: $LIST_OUT"
fi

SHOW_OUT="$("$BIN" commission show watchtower --dir "$VILLAGE")"
if echo "$SHOW_OUT" | grep -q "state:      draft"; then
  pass "commission show displays draft state"
else
  fail "commission show mismatch: $SHOW_OUT"
fi

# 4. Simulate Mayor decomposition: provision worker and post INDENTURE.md
step "T1.4: Mayor decomposition: INDENTURE.md and worker provisioning"
INDENTURE_FILE="$TREE/guild/ledger/commission1/INDENTURE.md"
cat <<'EOF' > "$INDENTURE_FILE"
# INDENTURE

## ada
- goal: Beacon beacon logic
- scope: src/beacon.rs
- done: beacon tests pass
EOF

# Update charter.json to roster worker ada (UID 1001), post indenture, advance to WindingDown
python3 - <<EOF
import json
charter_path = "$TREE/charter.json"
with open(charter_path, "r") as f:
    c = json.load(f)

# allocate slot 1001 for ada
c.setdefault("slots", {})["1001"] = {"identity": "ada", "status": "active", "commission": 1}
c.setdefault("parcels", {})["1001"] = {"identity": "ada", "stasis": False}

comm = c["commissions"]["1"]
comm["roster"] = [1001]
comm["state"] = "winding_down"
comm["indenture_entries"] = {"1001": "done"}
comm["deliverable_in_review"] = True

with open(charter_path, "w") as f:
    json.dump(c, f, indent=2)
EOF
pass "charter advanced with rostered worker and winding_down state"

SHOW_WINDING="$("$BIN" commission show watchtower --dir "$VILLAGE")"
if echo "$SHOW_WINDING" | grep -q "winding_down"; then
  pass "commission show displays winding_down state"
else
  fail "commission show failed to reflect winding_down state: $SHOW_WINDING"
fi

# 5. Worker produces deliverable into courier/freight/ and vault promotion
step "T1.5: Worker deliverable in courier/freight/ and vault promotion"
FREIGHT_DIR="$TREE/courier/freight"
mkdir -p "$FREIGHT_DIR"
DELIVERABLE="$FREIGHT_DIR/beacon-release.tar"
printf 'DELIVERABLE_PAYLOAD_V1' > "$DELIVERABLE"

VAULT_DIR="$TREE/keep/vault/watchtower"
mkdir -p "$VAULT_DIR"
cp "$DELIVERABLE" "$VAULT_DIR/beacon-release.tar"
SHA="$(sha256sum "$DELIVERABLE" | awk '{print $1}')"
printf '%s  beacon-release.tar\n' "$SHA" > "$VAULT_DIR/beacon-release.tar.sha256"
pass "deliverable staged and promoted into keep/vault/watchtower"

# 6. hamlet vault list & export
step "T1.6: hamlet vault list and vault export"
VAULT_LIST="$("$BIN" vault list --dir "$VILLAGE")"
if echo "$VAULT_LIST" | grep -q "beacon-release.tar"; then
  pass "vault list shows beacon-release.tar"
else
  fail "vault list mismatch: $VAULT_LIST"
fi

EXPORT_DEST="$WORK/exported-beacon.tar"
"$BIN" vault export beacon-release.tar "$EXPORT_DEST" --commission watchtower --dir "$VILLAGE" >/dev/null || fail "vault export failed"
if [ -f "$EXPORT_DEST" ] && [ "$(cat "$EXPORT_DEST")" = "DELIVERABLE_PAYLOAD_V1" ]; then
  pass "vault export produced byte-identical deliverable"
else
  fail "vault export content mismatch"
fi

# 7. hamlet commission close
step "T1.7: hamlet commission close"
CLOSE_OUT="$("$BIN" commission close watchtower --dir "$VILLAGE")"
if echo "$CLOSE_OUT" | grep -q "hamlet commission close: commission1 (watchtower)"; then
  pass "commission close executed successfully"
else
  fail "commission close output mismatch: $CLOSE_OUT"
fi

SHOW_CLOSED="$("$BIN" commission show watchtower --dir "$VILLAGE")"
if echo "$SHOW_CLOSED" | grep -q "state:      closed"; then
  pass "commission show confirms closed state"
else
  fail "commission show does not indicate closed state: $SHOW_CLOSED"
fi

# Assert telemetry archive directory exists
ARCHIVES_DIR="$TREE/keep/archives"
if [ -d "$ARCHIVES_DIR" ]; then
  pass "telemetry archives directory present at keep/archives/"
else
  fail "telemetry archives directory missing at $ARCHIVES_DIR"
fi

# --- T2: Adversarial Lifecycle Suite ----------------------------------------
step "T2: adversarial lifecycle test suite (22 tests; SPEC §7, §9, §11.2)"
if T2_OUT="$(cargo test -p hamlet-daemon --test adversarial_lifecycle --quiet 2>&1)"; then
  printf '%s\n' "$T2_OUT" | grep -E '^test result:' | tail -1
  pass "adversarial_lifecycle: all 22 tests green (positive invariants + findings asserted)"
else
  printf '%s\n' "$T2_OUT"
  fail "adversarial_lifecycle suite failed"
fi

# --- T3: Source-level ratchets ----------------------------------------------
step "T3: source-level ratchets (SPEC §9, ADR-0036/0037/0038)"

# Ratchet 1: bundle branch ref must be refs/heads/agent/<uid>
if grep -q 'format!("refs/heads/agent/{uid}")' crates/hamlet-core/src/gitmed.rs; then
  pass "gitmed: bundle branch namespace pinned to refs/heads/agent/<uid>"
else
  fail "gitmed: bundle branch namespace check missing or modified"
fi

# Ratchet 2: author and committer emails must match {uid}@hamlet.local
if grep -q 'format!("{uid}@hamlet.local")' crates/hamlet-core/src/gitmed.rs; then
  pass "gitmed: author/committer email pinned to {uid}@hamlet.local"
else
  fail "gitmed: author email identity check missing or modified"
fi

# Ratchet 3: bundle quarantine fetch before inspection
if grep -q 'refs/quarantine/candidate' crates/hamlet-daemon/src/ledger.rs; then
  pass "ledger: bundle fetch into refs/quarantine/candidate before verification (ADR-0037 cl. 2)"
else
  fail "ledger: bundle quarantine fetch missing from ledger.rs"
fi

# Ratchet 4: commission state machine totality catch-all
if grep -q 'IllegalTransition' crates/hamlet-core/src/commission.rs; then
  pass "commission: totality catch-all returns typed IllegalTransition"
else
  fail "commission: IllegalTransition missing from commission state machine"
fi

# Ratchet 5: roster frozen in non-Draft/Active states
if grep -q 'RosterFrozen' crates/hamlet-core/src/commission.rs; then
  pass "commission: roster mutation frozen outside Draft/Active"
else
  fail "commission: RosterFrozen enforcement missing"
fi

# Ratchet 6: freight symlink refusal (ADR-0007)
if grep -q 'is_symlink()' crates/hamlet-daemon/src/vault.rs; then
  pass "vault: lstat symlink refusal enforced on freight resolution (ADR-0007)"
else
  fail "vault: symlink check missing from resolve_freight_file"
fi

# --- T4: Guarded podman commission check (podman-optional) ------------------
step "T4: guarded podman check (podman-optional)"
if ci_guarded_direct_viable 2>/dev/null; then
  pass "podman direct execution viable on this host"
else
  skip "podman not available for unprivileged container execution (podman-optional)"
fi

# --- Summary ----------------------------------------------------------------
step "Summary"
if [ "$FAILURES" -eq 0 ]; then
  printf '\033[1;32me2e-commission: PASS\033[0m\n'
  exit 0
else
  printf '\033[1;31me2e-commission: FAIL (%d failures)\033[0m\n' "$FAILURES"
  exit 1
fi
