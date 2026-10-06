#!/usr/bin/env bash
# e2e-02-isolation.sh — QA prompt 02/05, isolation & escape harness for the
# Sprint 02 container runtime (SPEC §4 isolation invariants, §11.4 secrets;
# CONVENTIONS §5 containers, §9 testing doctrine).
#
# Layers:
#   T1  FakeRuntime adversarial suite (always runs; no podman needed):
#         crates/hamlet-daemon/tests/adversarial_isolation.rs
#   T2  Source-level ratchets: no container-API crates in the lockfile,
#       daemon .env reads confined to dispatch.rs's single seam, forbidden
#       podman flags exist ONLY in the podman.rs audit/test sections.
#   T3  Committed golden vectors are privilege-free and secret-free.
#   T4  Guarded real-podman escape suite. Runs the suite directly when this
#       host/user can (see ci_guarded_direct_viable); otherwise the battery is
#       handed to scripts/guarded-entry.sh, which measures with
#       guarded-service-user.sh --doctor and runs it as the dedicated service
#       user. A skip is only recorded when NEITHER route can run — with the
#       remedy printed (ADR-0032 cl. 5), never a silent vacuous green.
set -euo pipefail
cd "$(dirname "$0")/.."
. ./scripts/ci-guarded-job.sh

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  ok: %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
skip()  { printf '  SKIP: %s\n' "$*"; }

# --- T1: FakeRuntime adversarial isolation suite ----------------------------
step "T1: adversarial isolation suite (FakeRuntime; SPEC §4/§11.4)"
if T1_OUT="$(cargo test -p hamlet-daemon --test adversarial_isolation --quiet 2>&1)"; then
  printf '%s\n' "$T1_OUT" | tail -3
  pass "adversarial_isolation: all tests green"
else
  printf '%s\n' "$T1_OUT"
  fail "adversarial_isolation suite"
fi

# --- T2: source-level ratchets ----------------------------------------------
step "T2: source-level infrastructure ratchets"

# No container-API/bindings crates anywhere in the dependency graph
# (CONVENTIONS §5: podman CLI through tokio pipes only — no libpod, no Docker).
API_CRATES="$(grep -E '^name = "(bollard|libpod|podman|podman-api|docker|docker_api|shiplift|bocker)"' Cargo.lock || true)"
if [ -z "$API_CRATES" ]; then
  pass "Cargo.lock: no podman/docker API crates"
else
  fail "Cargo.lock contains container-API crates: $API_CRATES"
fi

# The daemon reads .env through ONE seam: dispatch.rs's load_env_secrets
# (ADR-0020 cl. 6 secret resolution at send time; ADR-0022 cl. 1 webhook
# auth — both SPEC §8.4/§8.5 "endpoint auth via .env"). Any other file
# touching env_file() fails this gate until reviewed (provisioning/bootstrap
# still never hydrate secrets, SPEC §11.4).
ENV_HITS="$(grep -rn 'env_file()' crates/hamlet-daemon/src || true)"
if [ -z "$ENV_HITS" ]; then
  pass "hamlet-daemon src: zero env_file() references"
else
  ENV_UNEXPECTED="$(printf '%s\n' "$ENV_HITS" | grep -v 'crates/hamlet-daemon/src/dispatch.rs' || true)"
  if [ -z "$ENV_UNEXPECTED" ]; then
    pass ".env reads confined to dispatch.rs (single seam, ADR-0020 cl. 6 / ADR-0022 cl. 1)"
  else
    fail "daemon touches .env outside the single seam: $ENV_UNEXPECTED"
  fi
fi

# Forbidden-flag ratchet: privilege/host-namespace literals may only exist
# in the podman.rs audit + injection-matrix sections (and tests). A new
# product file mentioning them fails this gate until reviewed.
PRIV_HITS="$(grep -rln -- '--privileged\|--userns=host\|--pid=host\|--network=host\|--ipc=host' crates/*/src || true)"
for f in $PRIV_HITS; do
  if [ "$f" = "crates/hamlet-daemon/src/podman.rs" ]; then
    pass "forbidden-flag literals confined to podman.rs (audit/ratchet)"
  else
    fail "forbidden-flag literal outside podman.rs: $f"
  fi
done
if [ -z "$PRIV_HITS" ]; then
  fail "ratchet broken: podman.rs audit no longer mentions the forbidden flags"
fi

# Image-policy ratchet (SPEC §4 base image, ADR-0012 cl. 2, ADR-0024,
# ADR-0025). The Containerfile golden already fails if the generator drifts,
# but two things it CANNOT catch are ratcheted here:
#   (a) a floor edited WITHOUT bumping FLOOR_VERSION — the derived tag would
#       stay identical and every already-built image would be reused stale,
#       silently (this is the exact QA04-05 failure mode);
#   (b) a parent-supplied body: the derived image must CLEAR ENTRYPOINT/CMD
#       rather than merely omit them (QA04-06).
BOOT="crates/hamlet-daemon/src/bootstrap.rs"
FLOOR_VER="$(sed -n 's/^pub const FLOOR_VERSION: u32 = \([0-9]*\);/\1/p' "$BOOT")"
FLOOR_SET="$(sed -n '/^pub const BOOTSTRAP_FLOOR/,/^];/p' "$BOOT" \
  | tr -d ' \n' | sed 's/.*\[//;s/\].*//;s/"//g;s/,$//')"
# The (floor version → floor contents) pairs this repo has blessed. Editing
# the floor without updating this line fails the gate: bump FLOOR_VERSION,
# then update the blessed mapping here in the same commit.
EXPECTED_V2="ca-certificates,curl,gzip,libgcc,libstdc++,tar,xz"
if [ -z "$FLOOR_VER" ] || [ -z "$FLOOR_SET" ]; then
  fail "image-policy ratchet broken: could not read FLOOR_VERSION/BOOTSTRAP_FLOOR from $BOOT"
elif [ "$FLOOR_VER" != "2" ]; then
  fail "FLOOR_VERSION is $FLOOR_VER, expected 2 (ADR-0024) — a floor change without a version bump serves stale images"
elif [ "$FLOOR_SET" != "$EXPECTED_V2" ]; then
  fail "BOOTSTRAP_FLOOR drifted from the blessed floor-v2 set (ADR-0024): got [$FLOOR_SET], want [$EXPECTED_V2]"
  fail "  — if this is an intentional floor change: bump FLOOR_VERSION in $BOOT and update EXPECTED_V2 here in the same commit"
else
  pass "floor v2 == blessed set (musl runtime libs present: $FLOOR_SET)"
fi
if grep -q '"libstdc++"' "$BOOT" && grep -q '"libgcc"' "$BOOT"; then
  pass "apk floor carries the pinned musl runtime libraries (QA04-05: node DT_NEEDED)"
else
  fail "apk floor lost libgcc/libstdc++ — pinned musl `node` cannot load (QA04-05)"
fi
CF_BODY="$(sed -n '/^pub fn containerfile(/,/^}/p' "$BOOT")"
if printf '%s' "$CF_BODY" | grep -q 'ENTRYPOINT \[\]' && printf '%s' "$CF_BODY" | grep -q 'CMD \[\]'; then
  pass "derived Containerfile explicitly CLEARS the parent's ENTRYPOINT/CMD (ADR-0025, QA04-06)"
else
  fail "containerfile() no longer emits ENTRYPOINT [] / CMD [] — a parent body (alpine:3.24 ships CMD [\"/bin/sh\"]) would be inherited (QA04-06)"
fi

# chown traversal ratchet (ADR-0034 cl. 5, QA04-29). The manor-home brace is
# `podman unshare chown -RP` over an AGENT-WRITABLE tree: `-R` recurses and
# `-P` says never dereference. Drop `-P` and the safety reverts to an unpinned
# GNU flag default; add `-L`/`-H` and the brace chowns whatever a symlink
# inside the home points at — a privileged, attacker-chosen chown with rc=0
# and no warning (measured). The golden unit test pins the argv; this ratchet
# pins the *source* so a rewrite of that golden cannot quietly re-open it.
PODMAN_RS="crates/hamlet-daemon/src/podman.rs"
CHOWN_ARGV_FN="$(sed -n '/^fn unshare_chown_argv_inner(/,/^}/p' "$PODMAN_RS")"
if [ -z "$CHOWN_ARGV_FN" ]; then
  fail "chown-traversal ratchet broken: unshare_chown_argv_inner is gone from $PODMAN_RS"
elif printf '%s' "$CHOWN_ARGV_FN" | grep -q 'argv.push("-RP".to_owned())'; then
  pass "the recursive userns chown emits -RP (ADR-0034 cl. 5: recurse, never dereference)"
else
  fail "the recursive userns chown no longer emits '-RP' — the manor-home brace must not \
dereference (ADR-0034 cl. 5 / QA04-29); a bare -R is an unpinned coreutils default"
fi
if printf '%s' "$CHOWN_ARGV_FN" | grep -qE '"-(L|H)"|--dereference'; then
  fail "the userns chown argv contains a dereferencing flag — see ADR-0034 cl. 5"
else
  pass "no dereference flag (-L/-H/--dereference) anywhere in the userns chown argv"
fi

# Guarded container names are per-run (QA04-26): podman names are global to
# the store, so a fixed test name means two overlapping batteries provision
# each other's parcels and the loser's failure text reads like a SPEC §4
# defect. Every guarded suite's names must go through `support::scoped`, and
# no guarded suite may declare a bare name constant again. `manor` is the
# product identity (MAYOR_IDENTITY, ADR-0028) and is never scoped.
GUARDED_SUITES="escape_guarded bootstrap_guarded freight_guarded provision_guarded \
supervise_guarded podman_guarded manor_guarded"
NAME_BAD=""
for suite in $GUARDED_SUITES; do
  f="crates/hamlet-daemon/tests/${suite}.rs"
  [ -f "$f" ] || { NAME_BAD="$NAME_BAD\n  missing $f"; continue; }
  grep -q 'support::scoped(' "$f" || NAME_BAD="$NAME_BAD\n  $suite: no support::scoped() call — container names are not per-run (QA04-26)"
  # A bare name constant (one that is NOT a documented `*_BASE` fed to
  # support::scoped) is the pre-QA04-26 shape.
  bare="$(grep -nE '^const [A-Z_]+: &str = "(qa-|hamlet-)' "$f" | grep -v '_BASE: &str' || true)"
  [ -z "$bare" ] || NAME_BAD="$NAME_BAD\n  $suite: unscoped container-name constant(s):\n$(printf '%s\n' "$bare" | sed 's/^/    /')"
  # The identity container must stay literal, not derived from a base const.
  if grep -qE '"manor"' "$f"; then
    grep -q 'MAYOR_IDENTITY' "$f" \
      || NAME_BAD="$NAME_BAD\n  $suite: uses the literal \"manor\" without referencing MAYOR_IDENTITY (ADR-0028)"
  fi
done
if [ -z "$NAME_BAD" ]; then
  pass "guarded container names are per-run; the Mayor identity name is untouched (QA04-26)"
else
  fail "guarded container-name ratchet (QA04-26):"
  printf "$NAME_BAD\n"
fi

# --- T3: golden vectors are privilege-free and secret-free -------------------
step "T3: committed golden vectors"
GOLDEN_BAD="$(grep -rn -- '--privileged\|--userns\|\.sock\|BEGIN [A-Z ]*PRIVATE KEY\|sk-[A-Za-z0-9]\{8\}' crates/*/tests/golden || true)"
if [ -z "$GOLDEN_BAD" ]; then
  pass "goldens: no privilege flags, sockets, or secret material"
else
  fail "golden vectors contain forbidden material:"
  printf '%s\n' "$GOLDEN_BAD"
fi

# --- T4: guarded real-podman escape suite ------------------------------------
step "T4: guarded podman escape suite (SPEC §4)"
if ! why="$(ci_guarded_direct_viable)"; then
  # The direct route is closed on this host/user. Before recording a skip, ask
  # the entry point: it re-measures as the service user and may run the whole
  # guarded battery there (ADR-0032 cl. 4) — which is the difference between
  # "T4 asserted nothing" and "T4 ran on another user".
  skip "direct route closed: ${why}"
  drc=0; ci_guarded_delegate "escape_guarded (via the service-user entry point)" || drc=$?
  case "$drc" in
    0) pass "escape_guarded: guarded battery green via the service-user entry point" ;;
    3) skip "escape_guarded: no route on this host (remedy printed above — nothing asserted)" ;;
    *) fail "guarded battery: a suite genuinely failed (log above)" ;;
  esac
else
  # Preflight: ADR-0011 needs a subuid range covering all 1000 slots, and
  # podman must be rootless. Fail loudly here — a guarded suite that fails
  # mid-run wastes a ~cold image pull.
  USER_NAME="$(id -un)"
  SUBUID_LINE="$(grep "^${USER_NAME}:" /etc/subuid 2>/dev/null || true)"
  if [ -z "$SUBUID_LINE" ]; then
    skip "no /etc/subuid entry for ${USER_NAME} — ADR-0011 subuid chown cannot work (QA04-07 remedy: a dedicated hamlet service user)"
  else
    SUBUID_COUNT="$(echo "$SUBUID_LINE" | head -1 | cut -d: -f3)"
    if [ "$SUBUID_COUNT" -ge 2001 ]; then
      pass "/etc/subuid range for ${USER_NAME}: ${SUBUID_COUNT} ids (>= 2001)"
    else
      fail "/etc/subuid range too small: ${SUBUID_COUNT} < 2001 (slots 1001-2000 must map)"
    fi
  fi
  ROOTLESS="$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null || echo unknown)"
  if [ "$ROOTLESS" = "true" ]; then
    pass "podman is rootless"
  else
    fail "podman reports rootless=${ROOTLESS} (SPEC §4: rootless only)"
  fi
  # One guarded job per host (QA05-09): this suite's container names are
  # global per podman user, so a concurrent guarded job would delete this
  # one's containers mid-assertion and the failure would read as a product
  # defect. Refuse rather than collide.
  if ! ci_guarded_lock_or_report; then
    skip "another guarded job holds the host lock (see above) — not run concurrently"
  elif E2E_OUT="$(cargo test -p hamlet-daemon --test escape_guarded -- --ignored podman --nocapture 2>&1)"; then
    printf '%s\n' "$E2E_OUT" | tail -3
    # A suite that skipped every test has not passed anything: report the
    # recorded reasons instead of a green line (CI04-01 doctrine).
    if printf '%s\n' "$E2E_OUT" | grep -q '^SKIP '; then
      printf '%s\n' "$E2E_OUT" | grep '^SKIP ' | sed 's/^/  SKIP: /'
      skip "escape_guarded: host precondition unmet (reason above; see tests/support/mod.rs)"
    else
      pass "escape_guarded suite"
    fi
  else
    printf '%s\n' "$E2E_OUT"
    fail "escape_guarded suite"
  fi
  ci_guarded_lock_release
fi

if [ "$FAILURES" -eq 0 ]; then
  printf '\n\033[1;32me2e-02-isolation: PASS\033[0m\n'
else
  printf '\n\033[1;31me2e-02-isolation: FAIL (%d)\033[0m\n' "$FAILURES"
  exit 1
fi
