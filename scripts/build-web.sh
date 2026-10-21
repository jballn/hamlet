#!/usr/bin/env bash
# build-web.sh — compiles hamlet-web WASM artifact and packages distribution bundle (SPEC §12.3; ADR-0044).
set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"
DIST_DIR="$REPO_ROOT/crates/hamlet-web/dist"
WEB_SRC="$REPO_ROOT/crates/hamlet-web/web"
ASSETS_SRC="$REPO_ROOT/crates/hamlet-web/assets"
SHARED_ASSETS="$REPO_ROOT/assets"

echo "==> [build-web] Checking wasm32-unknown-unknown target..."
if ! rustup target list | grep -q "wasm32-unknown-unknown (installed)"; then
  echo "build-web: wasm32-unknown-unknown target not found. Installing via rustup..."
  rustup target add wasm32-unknown-unknown
fi

echo "==> [build-web] Building hamlet-web in release mode..."
cargo build --target wasm32-unknown-unknown --release -p hamlet-web

WASM_OUT="$REPO_ROOT/target/wasm32-unknown-unknown/release/hamlet_web.wasm"
if [ ! -f "$WASM_OUT" ]; then
  echo "build-web: ERROR — expected wasm artifact not found at $WASM_OUT"
  exit 1
fi

echo "==> [build-web] Preparing distribution directory: $DIST_DIR..."
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR/assets/sprites"
mkdir -p "$DIST_DIR/assets/audio"

echo "==> [build-web] Copying static web assets and WASM binary..."
cp "$WASM_OUT" "$DIST_DIR/hamlet_web.wasm"
cp "$WEB_SRC/index.html" "$DIST_DIR/index.html"
cp "$WEB_SRC/hamlet.js" "$DIST_DIR/hamlet.js"
cp "$WEB_SRC/style.css" "$DIST_DIR/style.css"

if [ -f "$ASSETS_SRC/village_map.json" ]; then
  cp "$ASSETS_SRC/village_map.json" "$DIST_DIR/assets/village_map.json"
fi

# Copy sprite sheets from shared assets
if [ -f "$SHARED_ASSETS/characters/Spritesheet/roguelikeChar_transparent.png" ]; then
  cp "$SHARED_ASSETS/characters/Spritesheet/roguelikeChar_transparent.png" "$DIST_DIR/assets/sprites/characters.png"
fi
if [ -f "$SHARED_ASSETS/village/Spritesheet/roguelikeSheet_transparent.png" ]; then
  cp "$SHARED_ASSETS/village/Spritesheet/roguelikeSheet_transparent.png" "$DIST_DIR/assets/sprites/village.png"
fi
if [ -f "$SHARED_ASSETS/ui/Tilesheets/Large tiles/Thick outline/tilemap_packed.png" ]; then
  cp "$SHARED_ASSETS/ui/Tilesheets/Large tiles/Thick outline/tilemap_packed.png" "$DIST_DIR/assets/sprites/ui.png"
fi

# Copy audio assets
if [ -f "$SHARED_ASSETS/audio/toBe.wav" ]; then
  cp "$SHARED_ASSETS/audio/toBe.wav" "$DIST_DIR/assets/audio/toBe.wav"
fi

echo "==> [build-web] Computing asset version hash and manifest..."
# Calculate composite sha256 over all dist files
manifest_tmp=$(mktemp)
(
  cd "$DIST_DIR"
  find . -type f ! -name "manifest.json" | sort | while read -r f; do
    sha256sum "$f"
  done
) > "$manifest_tmp"

VERSION_HASH=$(sha256sum "$manifest_tmp" | awk '{print $1}')
rm -f "$manifest_tmp"

# Write manifest.json
cat > "$DIST_DIR/manifest.json" <<EOF
{
  "version_hash": "$VERSION_HASH",
  "wasm_abi_version": 1,
  "artifacts": {
    "wasm": "hamlet_web.wasm",
    "html": "index.html",
    "js": "hamlet.js",
    "css": "style.css",
    "map": "assets/village_map.json",
    "sprites": {
      "characters": "assets/sprites/characters.png",
      "village": "assets/sprites/village.png",
      "ui": "assets/sprites/ui.png"
    },
    "audio": {
      "sting": "assets/audio/toBe.wav"
    }
  }
}
EOF

# Verify WebAssembly magic header
WASM_MAGIC=$(head -c 4 "$DIST_DIR/hamlet_web.wasm" | xxd -p)
if [ "$WASM_MAGIC" != "0061736d" ]; then
  echo "build-web: ERROR — $DIST_DIR/hamlet_web.wasm has invalid magic bytes ($WASM_MAGIC != 0061736d)"
  exit 1
fi

WASM_SIZE=$(stat -c%s "$DIST_DIR/hamlet_web.wasm")
echo "==> [build-web] SUCCESS: Distribution bundle assembled at $DIST_DIR"
echo "    Version Hash: $VERSION_HASH"
echo "    WASM Size:    $WASM_SIZE bytes"
echo "    Magic Header: $WASM_MAGIC (valid wasm)"
