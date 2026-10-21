#!/usr/bin/env bash
# run-village-demo.sh — runs hamlet daemon with demo fixtures and launches 8-bit web village (SPEC §12.3; ADR-0044).
set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"

SMOKE_MODE=0
for arg in "$@"; do
  if [ "$arg" = "--smoke" ]; then
    SMOKE_MODE=1
  fi
done

echo "==> [run-village-demo] Building binaries (hamlet-cli, hamlet-daemon)..."
cargo build -p hamlet-cli -p hamlet-daemon --quiet

CLI_BIN="$REPO_ROOT/target/debug/hamlet"
DAEMON_BIN="$REPO_ROOT/target/debug/hamlet-daemon"

echo "==> [run-village-demo] Building hamlet-web WASM distribution bundle..."
"$REPO_ROOT/scripts/build-web.sh"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/hamlet-demo.XXXXXX")"
VILLAGE="$WORK/village"
TREE="$VILLAGE/.hamlet"

cleanup() {
  echo ""
  echo "==> [run-village-demo] Stopping daemon and cleaning up $WORK..."
  "$CLI_BIN" daemon stop --dir "$VILLAGE" >/dev/null 2>&1 || true
  rm -rf "$WORK" 2>/dev/null || true
  echo "==> [run-village-demo] Clean exit."
}
trap cleanup EXIT INT TERM

echo "==> [run-village-demo] Initializing demo village at $VILLAGE..."
"$CLI_BIN" init "$VILLAGE" >/dev/null

echo "==> [run-village-demo] Starting daemon..."
"$CLI_BIN" daemon start --dir "$VILLAGE" >/dev/null

echo "==> [run-village-demo] Seeding realistic village demo fixtures (SPEC §2)..."

# 1. Commission 1: watchtower
BRIEF="$WORK/brief.md"
cat <<'EOF' > "$BRIEF"
# Commission 1 — Watchtower Protocol
Deploy watchtower network beacons and log telemetry across parcels.
EOF

SEED_DIR="$WORK/seed"
mkdir -p "$SEED_DIR/src"
cat <<'EOF' > "$SEED_DIR/src/watchtower.rs"
pub fn watchtower_status() -> &'static str { "ONLINE" }
EOF

"$CLI_BIN" commission new watchtower --brief "$BRIEF" --seed "$SEED_DIR" --dir "$VILLAGE" >/dev/null

# 2. Indenture task breakdown
INDENTURE_FILE="$TREE/guild/ledger/commission1/INDENTURE.md"
cat <<'EOF' > "$INDENTURE_FILE"
# INDENTURE

## builder
- goal: watchtower beacon implementation
- scope: src/watchtower.rs
- done: beacon verified

## scout
- goal: fuzz and verify telemetry streams
- scope: tests/beacon.rs
- done: verified
EOF

# 3. Patron in stasis in Tavern
mkdir -p "$TREE/tavern/patrons/PROPRIETORveteran-miner"
cat <<'EOF' > "$TREE/tavern/patrons/PROPRIETORveteran-miner/PROPRIETOR.md"
# PROPRIETOR — veteran-miner
Hibernated parcel worker ready for re-assignment.
EOF

# 4. Census Working Parcels
mkdir -p "$TREE/keep/census/parcel1001"
cat <<'EOF' > "$TREE/keep/census/parcel1001/TITLE.md"
# Chain of Title — Parcel 1001
- Tenant: builder
- Commission: watchtower
- State: working
EOF

cat <<'EOF' > "$TREE/keep/census/parcel1001/DEED.md"
# DEED — Parcel 1001
Leased to builder (UID 1001).
EOF

cat <<'EOF' > "$TREE/keep/census/parcel1001/PROPRIETOR.md"
# PROPRIETOR — builder
Role: Systems Engineer
EOF

mkdir -p "$TREE/keep/census/parcel1002"
cat <<'EOF' > "$TREE/keep/census/parcel1002/TITLE.md"
# Chain of Title — Parcel 1002
- Tenant: scout
- Commission: watchtower
- State: idle
EOF

# 5. Vault promoted deliverables
VAULT_DIR="$TREE/keep/vault/watchtower"
mkdir -p "$VAULT_DIR"
printf 'WATCHTOWER_BEACON_PAYLOAD_V1' > "$VAULT_DIR/beacon-release.tar"
SHA="$(sha256sum "$VAULT_DIR/beacon-release.tar" | awk '{print $1}')"
printf '%s  beacon-release.tar\n' "$SHA" > "$VAULT_DIR/beacon-release.tar.sha256"

# 6. Courier mail spools via CLI
"$CLI_BIN" mail send 1000 "Watchtower Commissioned" "Commission 1 is underway." --dir "$VILLAGE" >/dev/null
"$CLI_BIN" mail send 1001 "Lease Activated" "Welcome to parcel 1001. Begin work on watchtower." --dir "$VILLAGE" >/dev/null

# 7. Update charter with active roster
python3 - <<EOF
import json
charter_path = "$TREE/charter.json"
with open(charter_path, "r") as f:
    c = json.load(f)

c.setdefault("slots", {})["1001"] = {"identity": "builder", "status": "active", "commission": 1}
c.setdefault("parcels", {})["1001"] = {"identity": "builder", "stasis": False}
c.setdefault("sessions", {})["1001"] = {"id": {"uid": 1001, "incarnation": 0}, "status": "running", "restart_count": 0}

c.setdefault("slots", {})["1002"] = {"identity": "scout", "status": "active", "commission": 1}
c.setdefault("parcels", {})["1002"] = {"identity": "scout", "stasis": False}
c.setdefault("sessions", {})["1002"] = {"id": {"uid": 1002, "incarnation": 0}, "status": "idle", "restart_count": 0}

c.setdefault("slots", {})["1003"] = {"identity": "veteran-miner", "status": "hibernated"}
c.setdefault("parcels", {})["1003"] = {"identity": "veteran-miner", "stasis": True}

comm = c["commissions"]["1"]
comm["roster"] = [1001, 1002]
comm["state"] = "active"

with open(charter_path, "w") as f:
    json.dump(c, f, indent=2)
EOF

ENDPOINT_FILE=""
if [ -f "$TREE/run/daemon.endpoint" ]; then
  ENDPOINT_FILE="$TREE/run/daemon.endpoint"
elif [ -f "$TREE/daemon.endpoint" ]; then
  ENDPOINT_FILE="$TREE/daemon.endpoint"
fi

DISCOVERY_TOKEN=""
if [ -f "$TREE/run/daemon.token" ]; then
  DISCOVERY_TOKEN="$(cat "$TREE/run/daemon.token")"
elif [ -f "$TREE/daemon.token" ]; then
  DISCOVERY_TOKEN="$(cat "$TREE/daemon.token")"
fi

if [ -z "$ENDPOINT_FILE" ] || [ -z "$DISCOVERY_TOKEN" ]; then
  echo "run-village-demo: ERROR — could not discover daemon endpoint or token"
  exit 1
fi

DISCOVERY_HOST="$(python3 -c "import json; d = json.load(open('$ENDPOINT_FILE')); print(f\"{d['addr']}:{d['port']}\")")"
DEMO_URL="http://$DISCOVERY_HOST/?token=$DISCOVERY_TOKEN"

echo ""
echo "=========================================================================="
echo "  HAMLET 8-BIT VILLAGE RPG — DEMO READY"
echo "  SPEC §12.3 WASM Presentation Layer"
echo "=========================================================================="
echo "  Village URL:  $DEMO_URL"
echo "  Local Token:  $DISCOVERY_TOKEN"
echo "  Endpoint:     $DISCOVERY_HOST"
echo "  Village Tree: $TREE"
echo "=========================================================================="
echo ""

if [ "$SMOKE_MODE" -eq 1 ]; then
  echo "==> [run-village-demo] Performing automated smoke verification (HTTP & Assets)..."
  curl -sSf "http://$DISCOVERY_HOST/?token=$DISCOVERY_TOKEN" | grep -q "<title>hamlet — 8-bit Village RPG</title>"
  echo "    [ok] index.html served cleanly"

  curl -sSf "http://$DISCOVERY_HOST/hamlet.js?token=$DISCOVERY_TOKEN" | grep -q "SPEC §12.3"
  echo "    [ok] hamlet.js thin glue served cleanly"

  curl -sSf "http://$DISCOVERY_HOST/style.css?token=$DISCOVERY_TOKEN" | grep -q "village-canvas"
  echo "    [ok] style.css served cleanly"

  curl -sSf "http://$DISCOVERY_HOST/hamlet_web.wasm?token=$DISCOVERY_TOKEN" -o "$WORK/smoke_wasm.bin"
  head -c 4 "$WORK/smoke_wasm.bin" | xxd -p | grep -q "0061736d"
  echo "    [ok] hamlet_web.wasm served with valid magic header"

  curl -sSf "http://$DISCOVERY_HOST/manifest.json?token=$DISCOVERY_TOKEN" | grep -q "version_hash"
  echo "    [ok] manifest.json served cleanly"

  curl -sSfI "http://$DISCOVERY_HOST/assets/audio/toBe.wav?token=$DISCOVERY_TOKEN" | grep -iq "content-type: audio/wav"
  echo "    [ok] assets/audio/toBe.wav served cleanly with audio/wav mime"

  echo "==> [run-village-demo] Smoke test SUCCESS."
  exit 0
fi

# Attempt to launch system browser if interactive and display available
if [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; then
  if command -v xdg-open >/dev/null 2>&1; then
    echo "==> [run-village-demo] Opening default browser at $DEMO_URL..."
    xdg-open "$DEMO_URL" >/dev/null 2>&1 &
  fi
fi

echo "Press Ctrl+C to terminate demo village daemon."
while true; do
  sleep 1
done
