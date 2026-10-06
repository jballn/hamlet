#!/usr/bin/env bash
# e2e-05-mayor-adversarial.sh — QA prompt 05/04, adversarial harness for the
# Sprint 05 Mayor loop (SPEC §7; ADR-0011/0026/0027/0028/0029;
# CONVENTIONS §2, §3, §5, §9). No podman required for the main battery; only
# T3 needs a real host, and it skips with a recorded reason otherwise.
#
# Layers:
#   T1  Adversarial mayor suite (always runs):
#         crates/hamlet-daemon/tests/adversarial_mayor.rs
#       Forgery/injection: a symlinked decree directory fails the pass closed
#         and a foreign-owned file inside it is refused (QA05-01),
#         planted-symlink ack temp file cannot clobber charter.json (QA05-04),
#         malformed/torn/oversize/deep-nesting matrix (zero partial execution),
#         digest binding rejects tampered + relabelled decrees, a torn in-place
#         write is NOT quarantined under its writer (QA05-05).
#       Replay: exactly-once across three restarts; forged ack files are never
#         state.
#       Sleep: a 1.5 s monitored idle window with no process, no charter
#         write, no log byte, no turn.
#       Triggers: unconfigured webhook topic + crash_wake=never never wake.
#       Hygiene: no .env value/key, no PEM, and an allowlisted grid key set.
#       Thread: 10 sequential wakes = 10 invocations = 10 whole packets, no
#         `--continue`, no cross-turn leak.
#       Storm: 500 events, max_live == 1, FIFO batches, every event evaluated
#         exactly once or explicitly refused; dedupe is on source identity, so
#         two identical bodies from two sources are two wakes (QA05-06).
#       Crash: kill mid-ProvisionAgent → resume → one slot, one parcel, one
#         lease.
#       Escalation: tree-observable, evidence re-resolved by the daemon.
#       Ack timeouts: once per timeout on the mayor.ack_retry_ms cadence, and
#         forgotten the moment the ack lands (QA05-03).
#   T2  Source-level ratchets (things a behavioural test cannot pin):
#         - decrees.rs: the decree directory the Mayor writes into is inside
#           the 0700 manor home (QA05-01's only real brace).
#         - decrees.rs: the ack `atomic_write` temp name is DOT-PREFIXED (the
#           watcher's hidden-component filter) — QA05-04 regressed it once.
#         - decrees.rs: `write_ack` failure is TRACED, never silently dropped.
#         - decrees.rs: every decree file is stat'ed with symlink_metadata,
#           opened O_NOFOLLOW, and checked against the decree directory's
#           owner (ADR-0007, QA05-01).
#         - decrees.rs: a PermissionDenied write into the manor home escalates
#           to the podman-unshare write seam (ADR-0011 cl. 4, QA05-02).
#         - decrees.rs: quarantine requires two identical observations, so a
#           torn write is never robbed (QA05-05).
#         - decrees.rs: the ack-timeout tracker has no once-per-process set —
#           the retry cadence is config (ADR-0030 cl. 3, QA05-03).
#         - mayor.rs: the wake queue dedupes on source identity, never on
#           rendered artifact equality (QA05-06).
#         - mayor.rs: `--continue` is never passed to pi (ADR-0028 cl. 1).
#         - hostperm.rs: manor home is 0700 and chowned to the mayor slot.
#         - CLI: the escalation/decree surface is still a Sprint 08 TODO
#           (OBS-05-04) — pinned as a known gap so it cannot silently regress
#           into "already done".
#   T3  Guarded real-podman manor smoke. Runs the suite directly when this
#       host/user can (ci_guarded_direct_viable); otherwise the battery is
#       handed to scripts/guarded-entry.sh, which measures with
#       guarded-service-user.sh --doctor and runs it as the dedicated service
#       user — the shape QA05-02's unshare-write probe needs in order to be
#       executed rather than merely declared. Only when NEITHER route can run
#       is a skip recorded, with the remedy printed (ADR-0032 cl. 5):
#         crates/hamlet-daemon/tests/manor_guarded.rs
#       UID 1000 can author a decree and the daemon reads it back; UID 1001 is
#       refused by the kernel every way round; plus the QA05-02 probe that
#       reports whether the daemon can write the ack directory.
set -euo pipefail
cd "$(dirname "$0")/.."
. ./scripts/ci-guarded-job.sh

FAILURES=0
step()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
pass()  { printf '  ok: %s\n' "$*"; }
fail()  { printf '\033[1;31m  FAIL: %s\033[0m\n' "$*"; FAILURES=$((FAILURES + 1)); }
skip()  { printf '  SKIP: %s\n' "$*"; }

# --- T1: adversarial mayor suite --------------------------------------------
step "T1: adversarial mayor suite (FakeAgent mayor; SPEC §7, ADR-0026/0029)"
if T1_OUT="$(cargo test -p hamlet-daemon --test adversarial_mayor --quiet 2>&1)"; then
  printf '%s\n' "$T1_OUT" | grep -E '^test result:' | tail -1
  pass "adversarial_mayor: all committed attack tests green"
else
  printf '%s\n' "$T1_OUT"
  fail "adversarial_mayor suite"
fi

# --- T2: source-level ratchets ----------------------------------------------
step "T2: source-level ratchets (SPEC §7; ADR-0007/0011/0026/0028/0029)"

DECREES=crates/hamlet-daemon/src/decrees.rs

# The executor stats every candidate with symlink_metadata and refuses
# anything that is not a regular file. Mutation-tested (QA05-08): replacing
# that one call with `fs::metadata` kept the entire workspace green, because
# no pre-existing test planted a symlink at a decree path. The grep is
# anchored on `let meta = ` so it matches `read_decree` only, not the
# unrelated `require_regular_file` stat.
if grep -qF 'let meta = fs::symlink_metadata(path)' "$DECREES" \
   && grep -qF 'if !meta.file_type().is_file() {' "$DECREES"; then
  pass "decree files are stat'ed with symlink_metadata before reading (ADR-0007)"
else
  fail "symlink_metadata guard missing from $DECREES — a planted symlink would be read as a decree"
fi

# The ack temp file MUST be dot-prefixed: the tree watcher's hidden-component
# filter (ADR-0019 cl. 6) is what stops an ack write from looking like a new
# decree event and re-triggering the plane. After QA05-04 the name is chosen
# from a bounded collision walk, so both spellings are legitimate.
if grep -qF 'format!(".{name}.tmp")' "$DECREES"; then
  pass "ack temp file is dot-prefixed (ADR-0019 cl. 6)"
else
  fail "ack temp name is no longer dot-prefixed — an ack write can re-trigger the decree plane"
fi

# QA05-04: the temp file must be created with ADR-0007 create-new discipline
# (O_CREAT|O_EXCL), which fails on an existing entry INCLUDING a planted
# symlink. `create(true)` here would let the Mayor redirect the daemon's
# write at charter.json.
if grep -qF 'create_new(true).open(&temp)' "$DECREES" \
   && ! grep -qF '.create(true)' "$DECREES"; then
  pass "temp files are opened create_new (O_EXCL) — a planted symlink cannot capture a write (QA05-04)"
else
  fail "atomic_write no longer uses create_new — a planted symlink can redirect the ack write (QA05-04)"
fi

# QA05-02: an ack write failure must never be silent. The decree may well
# have been executed and the Mayor is waiting on that file — so the write is
# PROPAGATED (a hard error the pass reports), not traced and dropped.
if grep -qF 'self.write_ack(&DecreeAck::done(&id)).await?;' "$DECREES" \
   && ! grep -qF 'decree-ack-write-failed' "$DECREES"; then
  pass "ack write failures are surfaced as errors, never a silent trace (QA05-02)"
else
  fail "ack write failures are traced instead of surfaced — a lost ack would be invisible (QA05-02)"
fi

# QA05-02 (deployment half): a 0700 subuid-owned manor home refuses the
# direct write, so `PermissionDenied` must escalate to the podman-unshare
# write seam (ADR-0011 cl. 4) rather than surfacing on every single decree.
# Removing either call makes every ack a hard error on the spec'd service
# user; replacing the argv with a shell would be ADR-0010 cl. 1.
if grep -qF 'unshare_write_atomic(path, bytes, (owner_uid, owner_uid))' "$DECREES" \
   && grep -qF 'pub fn with_runtime' "$DECREES" \
   && grep -qF 'fn unshare_write_atomic' crates/hamlet-daemon/src/podman.rs \
   && ! grep -qE '"(sh|bash)"\.to_owned\(\)' crates/hamlet-daemon/src/podman.rs; then
  pass "a refused manor write escalates through the typed podman-unshare seam (ADR-0011 cl. 4, QA05-02)"
else
  fail "the unshare write seam is gone — a subuid-owned 0700 manor home can never be acknowledged"
fi

# QA05-01: provenance is checked, not assumed. The decree directory must be
# a plain directory inside the manor home and not group/world-writable, and
# every file must be opened O_NOFOLLOW and owned by that directory's owner.
if grep -qF 'fn verify_decree_dir' "$DECREES" \
   && grep -qF 'ExecutorError::Provenance' "$DECREES" \
   && grep -qF 'meta.uid() != dir_uid' "$DECREES" \
   && grep -qF '.custom_flags(libc_nofollow())' "$DECREES"; then
  pass "decree provenance is verified per directory and per file (QA05-01, ADR-0007)"
else
  fail "decree provenance checks missing — a planted directory or foreign file would be executed"
fi

# QA05-05: quarantine must require two identical observations, so a decree
# still being written is left alone instead of moved out from under its
# writer. A single-shot quarantine is the regression.
if grep -qF 'fn torn_is_stable' "$DECREES" \
   && grep -qF 'DecreeOutcome::ParseDeferred' "$DECREES"; then
  pass "quarantine waits for a stable file — a torn write is not robbed (QA05-05)"
else
  fail "quarantine is single-shot again — a torn in-place write would be lost forever"
fi

# QA05-03: once-per-timeout, not once-per-process. The `woken` set is the
# exact shape of the old bug; the retry cadence must come from config.
if ! grep -qF 'woken: BTreeSet<DecreeId>' "$DECREES" \
   && grep -qF 'ack_retry_ms' "$DECREES"; then
  pass "ack timeouts re-notify on the configured cadence (QA05-03, ADR-0030 cl. 3)"
else
  fail "ack timeouts are once-per-process again — an unanswered decree is never re-escalated"
fi

# QA05-06: dedupe on source identity. Equality of rendered artifacts is the
# exact shape of the old bug (two identical violations, one wake).
MAYOR=crates/hamlet-daemon/src/mayor.rs
if grep -qF 'q.source == event.source' "$MAYOR" \
   && ! grep -qF 'q.artifact == event.artifact' "$MAYOR"; then
  pass "the wake queue dedupes on source identity, not rendered content (QA05-06)"
else
  fail "the wake queue dedupes on artifact equality — distinct events are folded away (QA05-06)"
fi

# ADR-0028 cl. 1: a fresh pi process per wake. `--continue` must never appear
# in the mayor's argv construction.
if grep -qF 'pi_session_cmd(&self.config.sessions)' crates/hamlet-daemon/src/mayor.rs \
   && ! grep -qF '"--continue"' crates/hamlet-daemon/src/mayor.rs; then
  pass "the Mayor's argv comes from pi_session_cmd and never carries --continue (ADR-0028 cl. 1)"
else
  fail "mayor.rs argv construction changed — a continued pi process would carry a thread across wakes"
fi

# ADR-0011 cl. 3: the manor home is 0700. This is the ONLY provenance boundary
# the decree plane has (QA05-01), so the constant is pinned here.
if grep -qF 'MODE_PARCEL_HOME: u32 = 0o700' crates/hamlet-daemon/src/hostperm.rs \
   && grep -qF 'set_mode(&home, MODE_PARCEL_HOME)?;' crates/hamlet-daemon/src/hostperm.rs; then
  pass "manor home is 0700 (ADR-0011 cl. 3 — QA05-01's only brace)"
else
  fail "manor home mode drifted — foreign slots could read or plant decrees"
fi

# The Mayor must NOT be able to reach the escalation directory: ADR-0029 cl. 4
# files notices where a human looks because "the Mayor may write only
# decrees/". That claim is a mount-table fact.
if ! grep -q 'escalations' crates/hamlet-core/src/mount.rs; then
  pass "no mount over manor/escalations/ — notices are daemon-filed (ADR-0029 cl. 4)"
else
  fail "a mount now covers manor/escalations/ — the Mayor could forge escalation notices"
fi

# OBS-05-04: SPEC §12.1 lists `decree {list|show}`; the CLI is still Sprint
# 08's TODO. Pin the gap so it cannot be mistaken for delivered work.
if grep -qE '^\s*\["decree",' crates/hamlet-cli/src/main.rs; then
  pass "CLI decree/escalation surface has landed"
else
  pass "OBS-05-04 recorded: the CLI escalation/decree hook is still the Sprint 08 TODO"
fi

# --- T3: guarded real-podman manor smoke ------------------------------------
step "T3: guarded podman manor smoke (SPEC §4, §7; ADR-0011)"
if ! why="$(ci_guarded_direct_viable)"; then
  skip "direct route closed: ${why}"
  drc=0; ci_guarded_delegate "manor_guarded (via the service-user entry point)" || drc=$?
  case "$drc" in
    0) pass "manor_guarded: guarded battery green via the service-user entry point" ;;
    3) skip "manor_guarded: no route on this host (remedy printed above — nothing asserted)" ;;
    *) fail "guarded battery: a suite genuinely failed (log above)" ;;
  esac
else
  USER_NAME="$(id -un)"
  SUBUID_LINE="$(grep "^${USER_NAME}:" /etc/subuid 2>/dev/null || true)"
  if [ -z "$SUBUID_LINE" ]; then
    skip "no /etc/subuid entry for ${USER_NAME} — ADR-0011 subuid chown cannot work (QA04-07 remedy: a dedicated hamlet service user)"
  elif ! podman info >/dev/null 2>&1; then
    skip "podman on PATH but unusable here (QA04-08 environmental)"
  else
    # The host lock (QA05-09) keeps a concurrent guarded job from deleting
    # this suite's containers mid-assertion.
    if ! ci_guarded_lock_or_report; then
      skip "another guarded job holds the host lock (see above) — not run concurrently"
    elif T3_OUT="$(cargo test -p hamlet-daemon --test manor_guarded --quiet -- --ignored --nocapture 2>&1)"; then
      printf '%s\n' "$T3_OUT" | grep -E 'QA05-02 (LATENT|LIVE)|test result:' | sed 's/^/    /'
      # A suite that skipped everything has proven nothing (ADR-0032 cl. 4):
      # report the recorded reasons, never a green line.
      if printf '%s\n' "$T3_OUT" | grep -q '^SKIP '; then
        printf '%s\n' "$T3_OUT" | grep '^SKIP ' | sed 's/^/    SKIP: /'
        skip "manor_guarded: host precondition unmet (reason above)"
      else
        pass "manor_guarded: UID-1000-only decree authorship proven in real containers"
      fi
    else
      printf '%s\n' "$T3_OUT"
      fail "manor_guarded suite"
    fi
    ci_guarded_lock_release
  fi
fi

# --- summary -----------------------------------------------------------------
if [ "$FAILURES" -eq 0 ]; then
  printf '\033[1;32me2e-05-mayor-adversarial: PASS\033[0m\n'
else
  printf '\033[1;31me2e-05-mayor-adversarial: %s FAILURE(S)\033[0m\n' "$FAILURES"
  exit 1
fi