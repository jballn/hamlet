#!/usr/bin/env bash
# village-reclaim.sh — the disk-leak policy for the guarded battery, in one
# executable place (CI config / host script; no product code).
#
# THE POLICY, stated explicitly because it is not obvious:
#
#   A guarded run builds its village tree with `tempfile::tempdir()` under
#   $TMPDIR (crates/hamlet-daemon/tests/support/mod.rs) and then hands paths
#   to SUBUIDS via `podman unshare chown` (ADR-0011 cl. 2). Subuid-owned files
#   (slot uids 1001-2000, the Mayor's 1000) are, by construction, files the
#   owner cannot read, write OR unlink without entering the userns. So when a
#   run is ABORTED — Ctrl-C, a RED suite, a killed CI job, a full disk — the
#   tree is stranded: a 0700 directory the invoking user cannot even `rm -rf`.
#
#   Measured cost of an aborted cold-bootstrap run on this workload: ~18 GB of
#   village trees. Ordinary `rm -rf` and even `sudo rm -rf` FAIL on them
#   (the parent dirs are subuid-owned 0700); the only reclaim is
#
#       podman unshare rm -rf <tree>
#
#   which is why this is a policy and not a cleanup afterthought:
#
#     1. A guarded job MUST start from a measured free-space baseline and MUST
#        print stranded trees *before* running, so "the disk filled up" is
#        diagnosable from the gate log.
#     2. An aborted guarded job MUST be reclaimed before the next one
#        (`--reclaim`). Budget for one full village tree per run; N stranded
#        trees is N runs' worth of disk still owed.
#     3. Reclamation is idempotent and never touches anything outside
#        $TMPDIR that is not a `.hamlet` village tree. `rm -rf` with a
#        mis-set TMPDIR is unrecoverable; the guard below is the reason this
#        is a script and not a cron alias.
#     4. If the reclaim cannot happen (no podman, no ownership), the stranded
#        trees are REPORTED as an environmental RED, never silently ignored.
#
# Usage:
#   scripts/village-reclaim.sh --audit      # measure + report, exit 0/1
#   scripts/village-reclaim.sh --reclaim    # audit + podman unshare rm -rf
#   scripts/village-reclaim.sh --policy     # print this policy, exit 0
set -euo pipefail
cd "$(dirname "$0")/.."

WORK_AREA="${HAMLET_WORK_AREA:-${TMPDIR:-/tmp}}"
# A tree smaller than this is not a village; refuse to guess.
MIN_TREE_BYTES="${HAMLET_RECLAIM_MIN_BYTES:-1048576}"   # 1 MiB
# Free space a guarded run needs before it starts (QA04-08 asks for 4 GiB in
# the work area; the cold bootstrap peaks far above it — see the 18 GB note).
MIN_FREE_BYTES="${HAMLET_RECLAIM_MIN_FREE:-$((4 * 1024 * 1024 * 1024))}"

yellow=$'\033[1;33m'; red=$'\033[1;31m'; green=$'\033[32m'; off=$'\033[0m'
fatal() { printf '%svillage-reclaim: %s%s\n' "$red" "$1" "$off" >&2; exit 2; }

policy() {
  sed -n '3,32p' "$0" | sed 's/^# \{0,1\}//'
}

# --- the guard --------------------------------------------------------------
# `podman unshare rm -rf` is unbounded by design (that is what makes it work
# on subuid-owned trees). It therefore gets exactly one input: a directory
# under a TMPDIR the operator named, containing a `.hamlet` tree.
check_area() {
  case "$WORK_AREA" in
    /|/bin|/boot|/dev|/etc|/lib|/lib64|/proc|/root|/run|/sbin|/srv|/sys|/usr|/var|/home|"")
      fatal "refusing to operate on WORK_AREA='${WORK_AREA}'. Set TMPDIR (or HAMLET_WORK_AREA) at a dedicated path." ;;
  esac
  [ -d "$WORK_AREA" ] || fatal "WORK_AREA '${WORK_AREA}' does not exist — mkdir it, or point HAMLET_WORK_AREA elsewhere."
}

# --- measurement ------------------------------------------------------------
# Lists village trees, with size measured by whoever can read them.
# A tree owned by another user (or subuid-owned) cannot be sized without
# entering its userns; that is reported as `?` rather than as 0.
survey() {
  [ -d "$WORK_AREA" ] || return 0
  shopt -s nullglob dotglob
  for d in "$WORK_AREA"/.tmp*; do
    [ -d "$d" ] || continue
    [ -d "$d/.hamlet" ] || continue          # only hamlet village trees
    local bytes owner
    owner="$(stat -c %U "$d" 2>/dev/null || echo '?')"
    if bytes="$(du -sb -- "$d" 2>/dev/null | cut -f1)"; then
      :
    else
      bytes="?"                             # unreadable as us -> stranded
    fi
    printf '%s\t%s\t%s\n' "$bytes" "$owner" "$d"
  done
  shopt -u nullglob dotglob
}

fmt_bytes() { # fmt_bytes <n|"?">
  [ "$1" = "?" ] && { printf '%s' 'unreadable'; return; }
  awk -v b="$1" 'BEGIN{split("B KiB MiB GiB TiB",u," ");i=1;while(b>=1024&&i<5){b/=1024;i++}printf "%.1f %s",b,u[i]}'
}

audit() {
  check_area
  local fstype free rows=0 unknown=0 total=0
  fstype="$(df --output=fstype -- "$WORK_AREA" 2>/dev/null | tail -1)"
  free="$(df -B1 --output=avail -- "$WORK_AREA" 2>/dev/null | tail -1 | tr -d ' ')"
  printf '\n== village-tree survey: %s (%s, %s free)\n' \
    "$WORK_AREA" "${fstype:-unknown fs}" "$(fmt_bytes "${free:-0}")"
  while IFS=$'\t' read -r bytes owner dir; do
    [ -n "${dir:-}" ] || continue
    rows=$((rows + 1))
    [ "$bytes" = "?" ] && unknown=$((unknown + 1)) || total=$((total + bytes))
    printf '  %-12s owner %-10s %s\n' "$(fmt_bytes "$bytes")" "$owner" "$dir"
  done < <(survey)
  if [ "$rows" -eq 0 ]; then
    printf '  %sno stranded village trees%s\n' "$green" "$off"
  else
    printf '  %d tree(s), %s measured, %d not measurable without its userns\n' \
      "$rows" "$(fmt_bytes "$total")" "$unknown"
    printf '  reclaim (the ONLY thing that works on subuid-owned trees):\n'
    printf '    sudo -H -u <owner> env TMPDIR=%s podman unshare rm -rf <tree>\n' "$WORK_AREA"
    printf '  or, for the whole area at once:\n'
    printf '    ./scripts/village-reclaim.sh --reclaim\n'
  fi
  if [ "$rows" -gt 0 ]; then
    printf '\n  %sPOLICY: an aborted guarded run strands these. Reclaim before the next run.%s\n' "$yellow" "$off"
  fi
  if [ "${free:-0}" -lt "$MIN_FREE_BYTES" ] 2>/dev/null; then
    printf '\n  %sPOLICY: %s free in the work area, below the %s floor (QA04-08).%s\n' \
      "$red" "$(fmt_bytes "${free:-0}")" "$(fmt_bytes "$MIN_FREE_BYTES")" "$off"
    printf '  Reclaim stranded trees above, or point TMPDIR at a larger disk.\n'
    return 1
  fi
  return 0
}

reclaim() {
  check_area
  command -v podman >/dev/null 2>&1 || fatal "podman not on PATH: subuid-owned trees can only be reclaimed with 'podman unshare rm -rf'."
  local targets=() owners=() bytes owner dir
  while IFS=$'\t' read -r bytes owner dir; do
    [ -n "${dir:-}" ] || continue
    targets+=("$dir"); owners+=("${owner:-?}")
  done < <(survey)
  if [ ${#targets[@]} -eq 0 ]; then
    printf 'village-reclaim: nothing to reclaim under %s\n' "$WORK_AREA"
    return 0
  fi
  printf 'reclaiming %d tree(s) under %s:\n' "${#targets[@]}" "$WORK_AREA"
  local i=0
  for dir in "${targets[@]}"; do
    owner="${owners[$i]}"; i=$((i+1))
    printf '  %s (owner %s) ... ' "$dir" "$owner"
    # Unshare first, then rm: inside the userns the subtree's subuid ownership
    # maps to our own uid, so the unlink is permitted. `--` guards the path.
    if podman unshare rm -rf -- "$dir" 2>/dev/null || rm -rf -- "$dir" 2>/dev/null; then
      printf '%sremoved%s\n' "$green" "$off"
    else
      printf '%sSTILL THERE%s — needs its owner: sudo -H -u %s podman unshare rm -rf %s\n' \
        "$red" "$off" "$owner" "$dir"
    fi
  done
}

case "${1:---audit}" in
  --audit)   audit ;;
  --reclaim) reclaim ;;
  --policy)  policy ;;
  *) fatal "unknown argument '$1' (want --audit | --reclaim | --policy)" ;;
esac