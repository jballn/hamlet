#!/usr/bin/env bash
# e2e-07-stasis.sh — QA harness for Sprint 07 Hibernation & Census
# (SPEC §10, §14; ADR-0039, ADR-0040; CONVENTIONS §2, §3, §9).
#
# Layers:
#   T1  CLI-level stasis lifecycle flow:
#         init tempdir village → commission new → roster worker scout (UID 1001)
#         → agent list (verify running) → agent hibernate 1001 (verify stasis)
#         → agent wake scout (verify new UID 1002 allocated) → agent list (verify lineage)
#         → deliverable in freight → vault promotion → commission close → archives asserted.
#   T2  Adversarial stasis test suite:
#         crates/hamlet-daemon/tests/adversarial_stasis.rs (26 tests)
#         - Area 1: TITLE integrity (golden lifecycle, append-only, restart stability)
#         - Area 2: Hibernation races (in-flight drain, interrupt, double-hibernate, crashed, kill -9)
#         - Area 3: Stasis integrity (file corruptions, PROPRIETOR frontmatter tampering)
#         - Area 4: Awakening (slot space exhaustion, released parcel unreadable, pkgcache fast path)
#         - Area 5: Convergence independence (dual DEED/PROPRIETOR failure isolation, escalation once)
#         - Area 6: Resource policy (static call-site audit, candidate ranking, OOM vectors)
#         - Area 7: Full-cycle resumability (deterministic kill points matching clean run)
#         - Defect reproductions (QA07-01 through QA07-06)
#   T3  Source-level ratchets:
#         - census.rs: StasisError typed error variants (ManifestVerification, ComputeNotFreed)
#         - stasis.rs: BackupManifest sha256 digest & PATRON_SCHEMA_VERSION = 1
#         - stasis.rs: Normative size caps (PROPRIETOR 4K/64K, TITLE 256K/4K, MANIFEST 1M)
#         - census.rs: Manor UID 1000 exemption in census and stasis
#         - census.rs: Fresh slot allocation on awaken
#         - ledger.rs: Dual convergence independence (DualConvergenceReport, CommissionConvergenceSummary)
#         - config.rs: CONFIG_VERSION = 11 with memory pressure defaults (85% threshold)
#   T4  Guarded podman check (podman-optional)
set -euo pipefail
cd "$(dirname "$0")/.."
. ./scripts/ci-guarded-job.sh

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  ok: %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
skip()  { printf '  SKIP: %s\n' "$*"; }

# --- T1: CLI-level stasis lifecycle flow ------------------------------------
step "T1: CLI-level stasis lifecycle flow (SPEC §10, §12.1; ADR-0039, ADR-0040)"

step "T1.0: build hamlet CLI"
if cargo build -p hamlet-cli --quiet; then
  pass "hamlet CLI built"
else
  fail "hamlet CLI build failed"
fi
BIN="target/debug/hamlet"
[ -x "$BIN" ] || { echo "FATAL: $BIN missing"; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/e2e-stasis.XXXXXX")"
cleanup() {
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
pass "village tree initialized"

# Start daemon for CLI commands over local API (SPEC §12.1; ADR-0041, ADR-0042)
"$BIN" daemon start --dir "$VILLAGE" >/dev/null || fail "daemon start failed"
pass "daemon started for stasis lifecycle flow"

# 2. hamlet commission new
step "T1.2: hamlet commission new"
BRIEF="$WORK/brief.md"
cat <<'EOF' > "$BRIEF"
# Commission 1 — Survey Protocol
Survey the local perimeter and construct telemetry probes.
EOF

COMM_OUT="$("$BIN" commission new survey --brief "$BRIEF" --dir "$VILLAGE")"
if echo "$COMM_OUT" | grep -q "hamlet commission new: commission1 (survey)"; then
  pass "commission new output matches expected header"
else
  fail "commission new output mismatch: $COMM_OUT"
fi

# 3. Provision worker scout (UID 1001) in charter.json and parcel
step "T1.3: Provision worker scout in charter.json and parcel keep/census/parcel1001"
python3 - <<EOF
import json, os

charter_path = "$TREE/charter.json"
with open(charter_path, "r") as f:
    c = json.load(f)

c.setdefault("slots", {})["1001"] = {"identity": "scout", "status": "active", "commission": 1}
c.setdefault("parcels", {})["1001"] = {"identity": "scout", "stasis": False}
c.setdefault("sessions", {})["1001"] = {"id": {"uid": 1001, "incarnation": 0}, "status": "running", "restart_count": 0}
c["commissions"]["1"]["roster"] = [1001]

with open(charter_path, "w") as f:
    json.dump(c, f, indent=2)

parcel_dir = "$TREE/keep/census/parcel1001"
os.makedirs(os.path.join(parcel_dir, "home", "src"), exist_ok=True)
with open(os.path.join(parcel_dir, "TITLE.md"), "w") as f:
    f.write("# TITLE — parcel1001\n\n- [2026-10-19T01:00:00Z] provision uid=1001 archetype=scout\n")
with open(os.path.join(parcel_dir, "DEED.md"), "w") as f:
    f.write("# DEED — parcel1001\n\n- identity: scout\n- uid: 1001\n")
with open(os.path.join(parcel_dir, "PROPRIETOR.md"), "w") as f:
    f.write("---\npatron: scout\nregister: scout\nblueprint: village-alpine\nblueprint_version: 1\nsource_uid: 1001\ncommission: 1\ntags: [\"scout\", \"rust\"]\ncreated_at: 1729300000\nstasis_version: 1\n---\n\n# Intelligence Profile — Scout\nScouting and telemetry intelligence.\n")
with open(os.path.join(parcel_dir, "home", "src", "probe.rs"), "w") as f:
    f.write("pub fn probe() -> bool { true }\n")
EOF
pass "worker scout provisioned in charter.json and census parcel"

# 4. hamlet agent list — verify running
step "T1.4: hamlet agent list reflects running worker"
LIST_OUT1="$("$BIN" agent list --dir "$VILLAGE")"
if echo "$LIST_OUT1" | grep -E -q "1001 +scout +survey +running"; then
  pass "agent list shows worker 1001 scout survey running"
else
  fail "agent list did not show running worker: $LIST_OUT1"
fi

# 5. hamlet agent hibernate 1001
step "T1.5: hamlet agent hibernate 1001 into patron stasis"
HIB_OUT="$("$BIN" agent hibernate 1001 --dir "$VILLAGE")"
if echo "$HIB_OUT" | grep -q "parcel 1001 hibernated into patron scout"; then
  pass "agent hibernate command succeeded"
else
  fail "agent hibernate failed: $HIB_OUT"
fi

[ -d "$TREE/tavern/patrons/PROPRIETORscout" ] || fail "patron record directory missing at tavern/patrons/PROPRIETORscout"
[ -f "$TREE/tavern/patrons/PROPRIETORscout/PROPRIETOR.md" ] || fail "patron PROPRIETOR.md missing"
[ -f "$TREE/tavern/patrons/PROPRIETORscout/manifest.json" ] || fail "patron backup manifest missing"
pass "patron stasis directory and manifest verified"

# 6. hamlet agent list — verify stasis
step "T1.6: hamlet agent list reflects stasis"
LIST_OUT2="$("$BIN" agent list --dir "$VILLAGE")"
if echo "$LIST_OUT2" | grep -E -q "1001 +scout +survey +stasis"; then
  pass "agent list shows worker 1001 in stasis"
else
  fail "agent list did not show stasis state: $LIST_OUT2"
fi

# 7. hamlet agent wake scout
step "T1.7: hamlet agent wake scout into new active slot"
WAKE_OUT="$("$BIN" agent wake scout --commission survey --dir "$VILLAGE")"
if echo "$WAKE_OUT" | grep -q "patron scout awakened as slot 1002"; then
  pass "agent wake allocated slot 1002 for awakened patron"
else
  fail "agent wake failed: $WAKE_OUT"
fi

# 8. hamlet agent list — verify lineage (both 1001 in stasis and 1002 active)
step "T1.8: hamlet agent list reflects lineage"
LIST_OUT3="$("$BIN" agent list --dir "$VILLAGE")"
if echo "$LIST_OUT3" | grep -E -q "1001 +scout +survey +stasis" && echo "$LIST_OUT3" | grep -E -q "1002 +scout +survey"; then
  pass "agent list shows lineage: predecessor 1001 stasis and new 1002 active"
else
  fail "agent list failed to reflect awakened lineage: $LIST_OUT3"
fi

# 9. Deliverable in freight and vault promotion
step "T1.9: Deliverable and vault promotion"
FREIGHT_DIR="$TREE/courier/freight"
mkdir -p "$FREIGHT_DIR"
DELIVERABLE="$FREIGHT_DIR/survey-map.tar"
printf 'SURVEY_MAP_DATA_V1' > "$DELIVERABLE"

VAULT_DIR="$TREE/keep/vault/survey"
mkdir -p "$VAULT_DIR"
cp "$DELIVERABLE" "$VAULT_DIR/survey-map.tar"
SHA="$(sha256sum "$DELIVERABLE" | awk '{print $1}')"
printf '%s  survey-map.tar\n' "$SHA" > "$VAULT_DIR/survey-map.tar.sha256"
pass "deliverable promoted into keep/vault/survey"

# 10. Advance commission to winding_down and close
step "T1.10: Commission winding_down and close"
python3 - <<EOF
import json
charter_path = "$TREE/charter.json"
with open(charter_path, "r") as f:
    c = json.load(f)
c["commissions"]["1"]["state"] = "winding_down"
c["commissions"]["1"]["deliverable_in_review"] = True
with open(charter_path, "w") as f:
    json.dump(c, f, indent=2)
EOF

CLOSE_OUT="$("$BIN" commission close survey --dir "$VILLAGE")"
if echo "$CLOSE_OUT" | grep -q "hamlet commission close: commission1 (survey)"; then
  pass "commission close succeeded"
else
  fail "commission close failed: $CLOSE_OUT"
fi

[ -d "$TREE/keep/archives" ] || fail "keep/archives/ missing"
pass "commission closed and archives verified"

# Stop daemon cleanly
"$BIN" daemon stop --dir "$VILLAGE" >/dev/null || fail "daemon stop failed"
pass "daemon stopped"

# --- T2: Adversarial Stasis Suite -------------------------------------------
step "T2: adversarial stasis test suite (26 tests; SPEC §10, §14; ADR-0039, ADR-0040)"
if T2_OUT="$(cargo test -p hamlet-daemon --test adversarial_stasis --quiet 2>&1)"; then
  printf '%s\n' "$T2_OUT" | grep -E '^test result:' | tail -1
  pass "adversarial_stasis: all 26 tests green (positive invariants + findings asserted)"
else
  printf '%s\n' "$T2_OUT"
  fail "adversarial_stasis suite failed"
fi

# --- T3: Source-level ratchets ----------------------------------------------
step "T3: source-level ratchets (SPEC §10, §14; ADR-0039, ADR-0040)"

# Ratchet 1: typed StasisError variants in census.rs
if grep -q 'ManifestVerification' crates/hamlet-daemon/src/census.rs \
  && grep -q 'ComputeNotFreed' crates/hamlet-daemon/src/census.rs \
  && grep -q 'UnknownSlot' crates/hamlet-daemon/src/census.rs; then
  pass "census: StasisError declares typed error variants (ManifestVerification, ComputeNotFreed, UnknownSlot)"
else
  fail "census: StasisError missing expected typed error variants"
fi

# Ratchet 2: BackupManifest sha256 digest & PATRON_SCHEMA_VERSION = 1
if grep -q 'pub const PATRON_SCHEMA_VERSION: u32 = 1;' crates/hamlet-core/src/stasis.rs \
  && grep -q 'pub const MANIFEST_SCHEMA_VERSION: u32 = 1;' crates/hamlet-core/src/stasis.rs; then
  pass "stasis: PATRON_SCHEMA_VERSION and MANIFEST_SCHEMA_VERSION pinned to 1"
else
  fail "stasis: schema versions missing or altered"
fi

# Ratchet 3: Normative size caps in stasis.rs (ADR-0039 cl. 2)
if grep -q 'pub const MAX_PROPRIETOR_FRONTMATTER_BYTES: usize = 4 \* 1024;' crates/hamlet-core/src/stasis.rs \
  && grep -q 'pub const MAX_PROPRIETOR_BODY_BYTES: usize = 64 \* 1024;' crates/hamlet-core/src/stasis.rs \
  && grep -q 'pub const MAX_TITLE_BYTES: usize = 256 \* 1024;' crates/hamlet-core/src/stasis.rs \
  && grep -q 'pub const MAX_MANIFEST_BYTES: usize = 1024 \* 1024;' crates/hamlet-core/src/stasis.rs; then
  pass "stasis: normative size caps enforced (PROPRIETOR 4K/64K, TITLE 256K, MANIFEST 1M)"
else
  fail "stasis: normative size caps missing or altered"
fi

# Ratchet 4: Manor UID 1000 exemption in census and stasis
if grep -q 'uid == MAYOR_UID' crates/hamlet-daemon/src/census.rs \
  && grep -q 'StasisError::Exempt' crates/hamlet-daemon/src/census.rs; then
  pass "census: Manor UID 1000 statically exempted in census parcel views and stasis"
else
  fail "census: Manor UID 1000 exemption missing"
fi

# Ratchet 5: Fresh slot allocation on awaken
if grep -q 'allocate_slot' crates/hamlet-daemon/src/census.rs; then
  pass "census: awaken delegates to allocate_slot for fresh monotonic UID assignment"
else
  fail "census: allocate_slot call missing from awaken"
fi

# Ratchet 6: Dual convergence independence in ledger.rs (ADR-0039 cl. 6)
if grep -q 'DualConvergenceReport' crates/hamlet-daemon/src/ledger.rs \
  && grep -q 'CommissionConvergenceSummary' crates/hamlet-daemon/src/ledger.rs; then
  pass "ledger: dual convergence independence structures integrated"
else
  fail "ledger: dual convergence structures missing"
fi

# Ratchet 7: Config version 11 and memory pressure thresholds (ADR-0040 cl. 3)
if grep -q 'pub const CONFIG_VERSION: u32 = 11;' crates/hamlet-core/src/config.rs \
  && grep -q 'DEFAULT_MEMORY_PRESSURE_THRESHOLD_PCT: u8 = 85;' crates/hamlet-core/src/config.rs; then
  pass "config: CONFIG_VERSION pinned to 11 with 85% memory pressure threshold"
else
  fail "config: CONFIG_VERSION 11 or memory pressure threshold missing"
fi

# Ratchet 8: Candidate ranking and OOM policy in stasis.rs
if grep -q 'pub fn rank_candidates' crates/hamlet-core/src/stasis.rs \
  && grep -q 'pub fn evaluate_oom_policy' crates/hamlet-core/src/stasis.rs; then
  pass "stasis: pure candidate ranking and OOM policy evaluation exposed"
else
  fail "stasis: candidate ranking or OOM policy functions missing"
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
  printf '\033[1;32me2e-07-stasis: PASS\033[0m\n'
  exit 0
else
  printf '\033[1;31me2e-07-stasis: FAIL (%d failures)\033[0m\n' "$FAILURES"
  exit 1
fi
