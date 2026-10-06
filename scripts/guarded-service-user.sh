#!/usr/bin/env bash
# guarded-service-user.sh — run the guarded real-podman battery as the
# dedicated service user (ADR-0011 cl. 2 host precondition; ADR-0032 cl. 1).
#
# WHY THIS EXISTS. The guarded suites can only assert the SPEC §4 per-slot uid
# policy when the host can actually realise it: `podman unshare chown` must be
# able to hand a path to a subuid, or parcel homes stay daemon-owned and every
# ownership assertion in the battery is vacuously true. A user for whom it
# cannot be done gets a recorded SKIP (ADR-0032 cl. 1); this script is the
# remedy's other half — it runs the suites as the dedicated service user, which
# is also the deployment shape ADR-0011 cl. 2 describes.
#
# Note what this is NOT: a check that the user must not be the podman owner.
# Rootless podman maps `0 -> <invoking uid> 1` in every container it starts, so
# no rootless host can satisfy that, and a requirement phrased as an
# impossibility just manufactures a permanent skip.
#
# It does NOT need cargo: the test binaries are built once by the developer
# and this script executes them directly (the service user gets read+execute
# on the build tree, never write, via ACL — see --prepare).
#
#   # one-time, as the developer:
#   ./scripts/guarded-service-user.sh --prepare
#
#   # why is it not running? (as the service user — pass the SAME TMPDIR the
#   # run will use, or the work-area check measures the wrong directory):
#   sudo -H -u hamlet env TMPDIR=/var/hamlet/tmp \
#     ./scripts/guarded-service-user.sh --doctor
#
#   # every run, as the service user (the operator types the password):
#   sudo -H -u hamlet env TMPDIR=/var/hamlet/tmp HAMLET_PODMAN_TESTS=1 \
#     ./scripts/guarded-service-user.sh
#
# Exit codes: 0 = every suite that ran is green (skips are reported, not
# failures); 1 = a suite genuinely failed; 2 = misuse.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET_DIR="${HAMLET_TARGET_DIR:-$PWD/target/debug}"
SERVICE_USER="${HAMLET_SERVICE_USER:-hamlet}"
SUITES=(
  podman_guarded
  bootstrap_guarded
  provision_guarded
  escape_guarded
  freight_guarded
  supervise_guarded
  manor_guarded
)

if [ "${1:-}" = "--prepare" ]; then
  command -v setfacl >/dev/null || { echo "need setfacl (acl package)"; exit 2; }
  # Traverse into the repo, and read+execute the build output. Nothing in the
  # developer's home becomes world-readable: this is a named-user ACL only,
  # and it grants `rX` (no write) on target/debug.
  for d in "$HOME" "$(dirname "$PWD")" "$PWD" "$PWD/target" "$TARGET_DIR" "$TARGET_DIR/deps"; do
    setfacl -m "u:${SERVICE_USER}:--x" "$d"
  done
  setfacl -R -m "u:${SERVICE_USER}:rX" "$TARGET_DIR"
  # Rebuild FIRST: cargo relinking a binary drops the ACL, so granting execute
  # rights to a tree that is about to be relinked is a trap.
  if command -v cargo >/dev/null 2>&1; then
    cargo test -p hamlet-daemon --no-run >/dev/null 2>&1 || true
  fi
  echo "prepared: ${SERVICE_USER} may execute $TARGET_DIR; run the battery with:"
  echo "  sudo -H -u ${SERVICE_USER} env TMPDIR=/var/hamlet/tmp HAMLET_PODMAN_TESTS=1 $0"
  exit 0
fi

# Resolve each suite's freshly built binary (cargo keeps old hashes around;
# the newest executable is the current one).
#
# The glob must stay UNQUOTED past the variable parts: quoting the whole word
# hands `ls` a literal `*` (ls does not glob), which made every suite report
# "not built" — and the run then printed PASS having executed nothing. The
# variable parts stay quoted, so a path with spaces is still safe.
resolve() {
  local name="$1" candidate
  shopt -s nullglob
  local candidates=("$TARGET_DIR"/deps/"${name}"-*)
  shopt -u nullglob
  for candidate in $(ls -t -- "${candidates[@]}" 2>/dev/null); do
    case "$candidate" in *.d) continue ;; esac
    [ -f "$candidate" ] && [ -x "$candidate" ] || continue
    printf '%s\n' "$candidate"
    return 0
  done
  return 1
}

if [ "${1:-}" = "--check" ]; then
  # Resolve-only dry run: validates path traversal, the glob and the execute
  # ACLs without spending a battery run.
  rc=0
  for suite in "${SUITES[@]}"; do
    if bin="$(resolve "$suite")"; then
      printf '  %-22s %s\n' "$suite" "$bin"
    else
      printf '  %-22s UNRESOLVED (%s)\n' "$suite" "$TARGET_DIR/deps"
      rc=1
    fi
  done
  exit "$rc"
fi

if [ "${1:-}" = "--doctor" ]; then
  # Why the battery cannot run here, in one pass — measured, not guessed.
  # Every line is a fact about THIS user on THIS host; the remedies are the
  # host-side actions only an operator/root can take.
  ok=0; bad=0
  line() { printf '  %-34s %s\n' "$1" "$2"; }
  good() { printf '  \033[32mok\033[0m   %-31s %s\n' "$1" "$2"; ok=$((ok+1)); }
  ill()  { printf '  \033[1;31mFAIL\033[0m %-31s %s\n' "$1" "$2"; bad=$((bad+1)); }

  echo "== identity"
  line "user" "$(id -un) (uid $(id -u), groups: $(id -Gn))"
  line "HOME / TMPDIR" "$HOME / ${TMPDIR:-<unset>}"

  echo "== userns / subuid (QA04-07)"
  su="$(id -un)"
  sub="$(grep "^${su}:" /etc/subuid 2>/dev/null | head -1)"
  subg="$(grep "^${su}:" /etc/subgid 2>/dev/null | head -1)"
  if [ -z "$sub" ]; then ill "subuid range" "no /etc/subuid entry for ${su} (root: add '${su}:231072:65536')"; else good "subuid range" "$sub"; fi
  if [ -z "$subg" ]; then ill "subgid range" "no /etc/subgid entry for ${su}"; else good "subgid range" "$subg"; fi
  # Either privilege form counts: the setuid bit, or an equivalent file
  # capability (`cap_setuid=ep` / `cap_setgid=ep`). Checking only for `-u`
  # cried "NOT setuid" on a host that maps subuids perfectly.
  for b in newuidmap:cap_setuid newgidmap:cap_setgid; do
    bin="${b%%:*}"; cap="${b##*:}"
    path="/usr/bin/$bin"
    [ -x "$path" ] || { ill "$bin" "missing at $path"; continue; }
    if [ -u "$path" ]; then good "$bin" "setuid present"
    elif command -v getcap >/dev/null 2>&1 && getcap "$path" 2>/dev/null | grep -q "${cap}=ep"; then
      good "$bin" "${cap}=ep (equivalent to setuid)"
    else
      ill "$bin" "neither setuid nor ${cap}=ep (root: chmod u+s $path)"
    fi
  done
  maxns="$(cat /proc/sys/user/max_user_namespaces 2>/dev/null || echo '?')"
  [ "$maxns" = "0" ] && ill "max_user_namespaces" "0 (rootless userns disabled)" || good "max_user_namespaces" "$maxns"

  echo "== subuid reachability (the precondition that actually decides QA04-07)"
  probe="$(mktemp -d 2>/dev/null || echo /tmp/probe.$$)" ; mkdir -p "$probe"; : > "$probe/f"
  if out="$(podman unshare chown 1000:1000 -- "$probe/f" 2>&1)"; then
    got="$(stat -c %u "$probe/f" 2>/dev/null || echo '?')"
    if [ "$got" != "$(id -u)" ] && [ "$got" != "?" ]; then
      good "unshare chown -> subuid" "inside 1000 became host uid ${got}"
    else
      ill "unshare chown -> subuid" "file stayed uid ${got}: the chown no-op'd in the userns"
    fi
  else
    ill "unshare chown -> subuid" "$out"
  fi
  rm -rf "$probe" 2>/dev/null || true

  echo "== cgroups (podman container creation)"
  if ! command -v podman >/dev/null 2>&1; then
    ill "podman" "not on PATH"
  elif ! podman info >/dev/null 2>&1; then
    ill "podman info" "podman is not usable as $(id -un)"
  else
    good "podman info" "usable"
    cgm="$(podman info --format '{{.Host.CgroupsVersion}} v2 / manager {{.Host.CgroupManager}}' 2>/dev/null)"
    line "cgroup v2 / manager" "$cgm"
    if loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null | grep -q '^yes$'; then
      good "systemd lingering" "enabled for $(id -un)"
    else
      ill "systemd lingering" "not enabled — rootless systemd scope creation will fail; root: loginctl enable-linger $(id -un)"
    fi
    # Informational only: a running systemd *user session* is NOT required —
    # a container host with lingering but no session bus starts containers
    # fine (verified on this dev host). Lingering above is the decisive fact,
    # because without it runc cannot create the transient scope at all.
    if systemctl --user is-system-running >/dev/null 2>&1; then
      good "systemd --user" "a user session exists"
    else
      line "systemd --user" "no user session (informational: not required if lingering is on)"
    fi
  fi

  echo "== work area (QA04-08)"
  # Which directory the battery would actually build in. TMPDIR if set, else
  # the operator's explicit override, else /tmp — but never silently: a
  # doctor that measures /tmp while the run uses /var/hamlet/tmp reports a
  # blocker that does not exist, which is worse than not checking.
  wa="${HAMLET_WORK_AREA:-${TMPDIR:-/tmp}}"
  if [ -z "${TMPDIR:-}" ] && [ -z "${HAMLET_WORK_AREA:-}" ]; then
    line "TMPDIR" "unset — measuring /tmp, but the battery run builds wherever TMPDIR points"
    line "export it" "export TMPDIR=/var/hamlet/tmp   (the documented disk-backed work area)"
  else
    line "TMPDIR" "${TMPDIR:-<unset>} (HAMLET_WORK_AREA=${HAMLET_WORK_AREA:-<unset>})"
  fi
  fst="$(df --output=fstype -- "$wa" 2>/dev/null | tail -1)"
  avail="$(df -B1 --output=avail -- "$wa" 2>/dev/null | tail -1 | tr -d ' ')"
  case "$fst" in
    tmpfs|ramfs) ill "work area fs" "$wa is $fst — RAM-backed; point TMPDIR at disk" ;;
    "") ill "work area fs" "$wa does not exist (or df failed) — mkdir it or point TMPDIR elsewhere" ;;
    *) good "work area fs" "$wa is ${fst}" ;;
  esac
  [ -n "$avail" ] && [ "$avail" -ge $((4*1024*1024*1024)) ] 2>/dev/null     && good "work area free" "$((avail/1024/1024/1024)) GiB"     || ill "work area free" "${avail:-?} bytes free, need >= 4 GiB"

  echo "== binaries"
  "$0" --check >/tmp/.hamlet-check.$$ 2>&1 && { good "test binaries" "all seven resolve + executable"; rm -f /tmp/.hamlet-check.$$; } \
    || { ill "test binaries" "see /tmp/.hamlet-check.$$"; sed 's/^/    /' /tmp/.hamlet-check.$$; }

  printf '\n  %d ok, %d to fix (each FAIL line carries its root remedy)\n' "$ok" "$bad"
  exit "$bad"
fi

[ "$(id -u)" != "0" ] || { echo "run as the service user (sudo -H -u ${SERVICE_USER}), not root: the battery is rootless-only"; exit 2; }
[ "${HAMLET_PODMAN_TESTS:-0}" = "1" ] || { echo "export HAMLET_PODMAN_TESTS=1"; exit 2; }
command -v podman >/dev/null || { echo "SKIP: podman not on PATH"; exit 0; }
podman info >/dev/null 2>&1 || { echo "SKIP: podman is on PATH but not usable here"; exit 0; }

echo "service user: $(id -un) (uid $(id -u))"
echo "work area:    ${TMPDIR:-/tmp} ($(df --output=fstype -- "${TMPDIR:-/tmp}" | tail -1))"
echo "target dir:   ${TARGET_DIR}"
echo

failures=0
skipped=0
for suite in "${SUITES[@]}"; do
  bin="$(resolve "$suite" || true)"
  if [ -z "$bin" ]; then
    # A suite that cannot even be started has proven nothing. Per ADR-0032
    # cl. 5 that is a FAILURE, never a skip: the remedy is mechanical
    # (build it, or re-run --prepare so the execute ACLs cover the fresh
    # binaries — cargo relinking drops them), not a host condition.
    printf '\033[1;31m==> %s: not runnable as %s\033[0m\n' "$suite" "$(id -un)"
    printf '      no executable %s-* under %s/deps, or it is not executable here.\n' "$suite" "$TARGET_DIR"
    printf '      Remedy (as the developer): cargo test -p hamlet-daemon --no-run && %s --prepare\n' "$0"
    failures=$((failures + 1))
    continue
  fi
  if [ ! -x "$bin" ]; then
    printf '\033[1;31m==> %s: %s is not executable here (stale ACLs?)\033[0m\n' "$suite" "$bin"
    printf '      Remedy (as the developer): %s --prepare\n' "$0"
    failures=$((failures + 1))
    continue
  fi
  printf '\n\033[1m==> %s\033[0m\n' "$suite"
  out="$("$bin" --ignored podman --nocapture 2>&1)" && rc=0 || rc=$?
  # Surface the recorded skip reasons: a suite that skipped everything has
  # proven nothing (ADR-0032 cl. 4), and a red line must be readable as
  # "environmental" or "product" without guessing.
  printf '%s\n' "$out" | grep -E '^(SKIP |test result:)' | sed 's/^/  /'
  if [ "$rc" -ne 0 ]; then
    printf '\033[1;31m  FAIL: %s\033[0m\n' "$suite"
    printf '%s\n' "$out" | tail -20 | sed 's/^/    /'
    failures=$((failures + 1))
  fi
done

printf '\n'
if [ "$failures" -gt 0 ]; then
  printf '\033[1;31mguarded-service-user: FAIL (%d suite(s))\033[0m\n' "$failures"
  exit 1
fi
printf '\033[1;32mguarded-service-user: PASS\033[0m'
[ "$skipped" -gt 0 ] && printf ' (%d suite(s) not built)' "$skipped"
printf ' — skip lines above are recorded host preconditions, not passes\n'