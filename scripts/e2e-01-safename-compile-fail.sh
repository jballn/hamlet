#!/usr/bin/env bash
# e2e-01-safename-compile-fail.sh — QA prompt 01/05 item 2.
#
# Compile-FAIL proof that the SafeName boundary is enforced by the TYPE
# SYSTEM, not convention (SPEC §2 name-safety, ADR-0002/0003).
#
# Recorded choice: `trybuild` was rejected — it is not on the locked
# dev-dependency allowlist (scripts/check-deps.sh; adding it needs an ADR).
# This script is the committed equivalent: raw `rustc` probes compiled
# against the built `hamlet_core` rlib. Every FAIL probe MUST fail to
# compile; the CONTROL probe must compile cleanly. Any deviation fails CI.
#
# Privilege-free by construction (rustc + an rlib, no host, no podman), so
# unlike the guarded container sections of e2e-02/04/05 it never needs the
# service-user entry point — stated here rather than left as an implicit gap.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\033[1m==> %s\033[0m\n' "$*"; }

step "building hamlet-core rlib for compile-fail probes"
cargo build -p hamlet-core --quiet
RLIB="target/debug/libhamlet_core.rlib"
[ -f "$RLIB" ] || { echo "FATAL: $RLIB not found after build"; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

probe() { # probe <name> <expect: ok|fail> <<'RS'
  local name="$1" expect="$2" src="$WORK/$1.rs"
  cat > "$src"
  local log="$WORK/$1.log" rc=0
  rustc --edition 2021 --crate-type lib \
    -L dependency=target/debug/deps \
    --extern hamlet_core="$RLIB" \
    --emit=metadata -o "$WORK/$1.rmeta" \
    "$src" > "$log" 2>&1 || rc=$?
  if [ "$expect" = ok ] && [ $rc -ne 0 ]; then
    echo "PROBE $name: expected COMPILE OK, got failure:"; sed 's/^/    /' "$log"; FAILURES=$((FAILURES+1))
  elif [ "$expect" = fail ] && [ $rc -eq 0 ]; then
    echo "PROBE $name: expected COMPILE FAIL, but it COMPILED — SafeName boundary breached:"
    sed 's/^/    /' "$src"; FAILURES=$((FAILURES+1))
  else
    echo "PROBE $name: ok (expected $expect)"
  fi
}

FAILURES=0

# --- control: correct usage MUST compile ------------------------------------
probe control_safename_usage ok <<'RS'
use hamlet_core::sanitize::sanitize_name;
use hamlet_core::tree::HamletTree;
pub fn paths() {
    let tree = HamletTree::new(std::path::PathBuf::from("/v/.hamlet"));
    let name = sanitize_name("agent-01").expect("valid");
    let _ = tree.vault_artifact(&name);
    let _ = tree.courier_mail_recipient(&name);
    let _ = tree.patron(&name);
}
RS

# --- raw strings into name-taking accessors MUST NOT compile ----------------
probe raw_str_vault_artifact fail <<'RS'
use hamlet_core::tree::HamletTree;
pub fn escape() {
    let tree = HamletTree::new(std::path::PathBuf::from("/v/.hamlet"));
    let _ = tree.vault_artifact("../../../etc/passwd");
}
RS

probe raw_string_vault_artifact fail <<'RS'
use hamlet_core::tree::HamletTree;
pub fn escape() {
    let tree = HamletTree::new(std::path::PathBuf::from("/v/.hamlet"));
    let hostile = String::from("../../outside");
    let _ = tree.vault_artifact(&hostile);
}
RS

probe raw_str_mail_recipient fail <<'RS'
use hamlet_core::tree::HamletTree;
pub fn cross_uid() {
    let tree = HamletTree::new(std::path::PathBuf::from("/v/.hamlet"));
    let _ = tree.courier_mail_recipient("../1000");
}
RS

probe raw_str_patron fail <<'RS'
use hamlet_core::tree::HamletTree;
pub fn traversal() {
    let tree = HamletTree::new(std::path::PathBuf::from("/v/.hamlet"));
    let _ = tree.patron(&"../evil".to_string());
}
RS

# --- direct construction / conversion bypasses MUST NOT compile -------------
probe construct_safename_tuple fail <<'RS'
use hamlet_core::sanitize::SafeName;
pub fn bypass() -> SafeName {
    SafeName("../evil".to_string())
}
RS

probe construct_safename_from fail <<'RS'
use hamlet_core::sanitize::SafeName;
pub fn bypass() -> SafeName {
    SafeName::from("../evil")
}
RS

probe construct_safename_deref_mut fail <<'RS'
use hamlet_core::sanitize::sanitize_name;
pub fn mutate() {
    let mut name = sanitize_name("safe").unwrap();
    // If SafeName leaked its interior, this would compile and allow
    // post-validation mutation into a traversal name.
    name.0 = "../evil".to_string();
}
RS

echo
if [ "$FAILURES" -ne 0 ]; then
  printf '\033[1;31me2e-01-safename-compile-fail: FAIL (%s probe(s) deviated)\033[0m\n' "$FAILURES"
  exit 1
fi
printf '\033[1;32me2e-01-safename-compile-fail: PASS (boundary is type-enforced)\033[0m\n'
