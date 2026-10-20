#!/usr/bin/env bash
# check-deps.sh — enforce the locked dependency allowlist (SPEC §3).
#
# Parses every crates/*/Cargo.toml and fails if any external dependency
# appears outside the allowlist. Internal `hamlet-*` path deps are exempt.
# Additions to the allowlist require an ADR (see roadmap/CONVENTIONS.md §2).
#
# Runtime allowlist (SPEC §3):
#   tokio, axum, tokio-tungstenite, serde, serde_json, sled, dotenvy,
#   regex, reqwest, notify
# ADR-approved additions (CONVENTIONS §2):
#   toml (ADR-0009 — blueprint schema)
#   tower-http (ADR-0022 — hyper-level webhook body cap, feature `limit`)
#   clap, ratatui, crossterm (ADR-0042 — CLI command tree and TUI architecture)
# Dev-dependency allowlist (Sprint 00 baseline, recorded in HANDOFF.md):
#   tempfile, anyhow, thiserror
set -euo pipefail
cd "$(dirname "$0")/.."

RUNTIME_ALLOW="tokio axum tokio-tungstenite serde serde_json sled dotenvy regex reqwest notify toml tower-http clap ratatui crossterm"
DEV_ALLOW="tempfile anyhow thiserror"

shopt -s nullglob
manifests=(crates/*/Cargo.toml)
if [ ${#manifests[@]} -eq 0 ]; then
  echo "check-deps: no crates/*/Cargo.toml found — nothing to check (empty tree OK)."
  exit 0
fi

violations=0
for m in "${manifests[@]}"; do
  while IFS='|' read -r kind name; do
    case "$name" in
      hamlet|hamlet-*) continue ;;  # internal workspace crates
    esac
    ok=0
    for a in $RUNTIME_ALLOW; do [ "$name" = "$a" ] && ok=1 && break; done
    if [ "$kind" = "dev" ] && [ $ok -eq 0 ]; then
      for a in $DEV_ALLOW; do [ "$name" = "$a" ] && ok=1 && break; done
    fi
    if [ $ok -eq 0 ]; then
      echo "check-deps: VIOLATION in $m: [$kind-dependencies] '$name' is not in the allowlist (needs an ADR)."
      violations=$((violations + 1))
    fi
  done < <(awk '
    function flush_sec() { sec = "" }
    /^[[:space:]]*\[/ {
      line = $0
      sub(/^[[:space:]]*\[/, "", line); sub(/\][[:space:]]*(#.*)?$/, "", line)
      sub(/[[:space:]]+$/, "", line)
      if (line == "dependencies")            { sec = "run" }
      else if (line == "dev-dependencies")   { sec = "dev" }
      else if (line == "build-dependencies") { sec = "run" }
      else if (line ~ /^dependencies\./)     { n = line; sub(/^dependencies\./, "", n); print "run|" n; sec = "" }
      else if (line ~ /^dev-dependencies\./) { n = line; sub(/^dev-dependencies\./, "", n); print "dev|" n; sec = "" }
      else if (line ~ /^build-dependencies\./) { n = line; sub(/^build-dependencies\./, "", n); print "run|" n; sec = "" }
      else                                   { sec = "" }
      next
    }
    sec != "" && /=/ {
      line = $0
      sub(/^[[:space:]]*/, "", line)
      if (line ~ /^#/) next
      n = line
      sub(/[[:space:]]*=.*$/, "", n)
      gsub(/"/, "", n)
      sub(/\..*$/, "", n)   # dotted keys like `tokio.workspace = true`
      if (n ~ /^[A-Za-z0-9_-]+$/) print sec "|" n
    }
  ' "$m")
done

if [ $violations -gt 0 ]; then
  echo "check-deps: FAIL — $violations dependency allowlist violation(s)."
  exit 1
fi
echo "check-deps: OK — all dependencies within the locked allowlist."
