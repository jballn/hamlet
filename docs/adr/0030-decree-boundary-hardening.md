# ADR-0030 — Decree boundary hardening: provenance, unshare writes, stable files, wake identity

Status: Accepted
Sprint: 05-mayor-loop · Prompt: 05/05-daemon-engineer (QA05-01/02/03/05/06 fixes)
Refs: SPEC §1 (file-as-truth), §2 (typed paths), §4 (manor mount row), §7
(stateless executive); CONVENTIONS §2 (singular governor, sanitize at the
boundary), §3 r1/r2; ADR-0007 (no symlinks, create-new), ADR-0011 cl. 2–4
(subuid ownership; daemon writes through `podman unshare`), ADR-0026 cl. 3/5
(digest binding, ack vocabulary, quarantine), ADR-0028 cl. 3 (wake queue),
ADR-0029 cl. 1–5

## Context

Step 04's adversarial suite found five defects in code this sprint shipped;
step 03b fixed the symlink capture (QA05-04) and made a lost ack loud. Four
remained, all on the same boundary — **the seam between the Mayor's
untrusted input and the daemon's writes**:

- **QA05-01 (MEDIUM).** `read_decree` checked "regular file, size, parse" and
  nothing else. The only provenance brace was a *permission* (0700 manor
  home) and an *absent mount* — both true today, neither enforced by the
  executor. A decree directory replaced by a symlink, or a file in the
  Mayor's own directory written by somebody else, executed and was acked
  `done`, and the daemon filed an escalation notice for a decision it could
  not attribute. It fails open the moment a mount widens.
- **QA05-02 (HIGH).** `manor/home/` is 0700 and owned by subuid 1000, so
  under the spec'd dedicated service user the daemon's direct ack write is
  refused by the kernel. ADR-0011 cl. 4 says outright that such trees must
  be written through `podman unshare`; step 03b made the failure a hard error
  but did not give the executor a way to succeed.
- **QA05-03 (MEDIUM).** The ack-timeout tracker fired **once per process**
  per decree (`woken: BTreeSet<DecreeId>`, never cleared while the file
  lived). The prompt asked for "exactly once per timeout"; what shipped was
  "once per deployment", with a restart as the only escape.
- **QA05-05 (LOW).** Quarantine was a single-shot `fs::rename`: a torn
  in-place write was moved to `.rejected/` on the first parse failure, the
  writer's remaining bytes landed in the quarantine inode, and the decree was
  lost forever with no ack.
- **QA05-06 (LOW).** `WakeQueue::push` treated rendered-artifact equality as
  event identity, so two genuinely distinct events that serialize identically
  collapsed into one wake.

The common thread: every one of these is a decision made on the *wrong*
fact — content instead of origin, one-shot instead of observed-stability,
process lifetime instead of operator cadence.

## Decision

1. **Provenance is checked, not assumed (QA05-01).** Before reading a single
   file, `verify_decree_dir` requires `manor/home/decrees/` to be a plain
   directory (never a planted symlink), to canonicalize *inside* the manor
   home, and to not be group- or world-writable; it returns the directory's
   owning uid. Each file must then be that uid's, must `lstat` as a regular
   file, and must be opened `O_NOFOLLOW` with the *opened handle's* metadata
   re-checked — the lstat/open gap is closed, not assumed away. A file that
   fails attribution yields `foreign_provenance` and is **left exactly where
   it is**: not executed, not acknowledged, not quarantined, no sequence
   consumed. The daemon is not entitled to move a file it cannot attribute,
   and a directory it could not vouch for is exactly where a well-meaning
   "cleanup" write does the most damage. A directory failure fails the whole
   pass closed (`ExecutorError::Provenance`).

   The uid compared against is the *directory's* owner, not a hardcoded
   `MAYOR_UID`: under rootless podman, subuid 1000 appears on the host as
   some other number (ADR-0011 cl. 2), so an absolute comparison would fail
   every real deployment while passing every test.

2. **Writes into subuid-owned trees go through the userns (QA05-02).**
   `Runtime::unshare_write_atomic` (ADR-0011 cl. 4) stages the bytes in a
   daemon-private file, then composes five typed `podman unshare` payloads —
   `mktemp` (the exclusive, unpredictable staging name: ADR-0007 create-new
   semantics the daemon cannot have in a directory the Mayor controls),
   `cp`, `chmod 0600`, `chown <mayor>:<mayor>`, `mv` (atomic). Never a shell
   (ADR-0010 cl. 1); every path slot sits behind `--`; `mktemp`'s printed
   path is re-validated before it is used. The executor tries the direct
   write first — cheaper, and the whole job where the daemon is the owner —
   and escalates **only** on `PermissionDenied`, because any other error is a
   real error that four more processes would merely obscure. With no runtime
   installed the original error surfaces unchanged: loud, never silent.

3. **Quarantine requires an observation, not an assumption (QA05-05).** A
   parse failure records the file's `(len, mtime)` and reports
   `ParseDeferred`. Only a *later* pass that sees the identical shape
   quarantines it — the writer has demonstrably stopped. A file that changes
   in between is still being written and is simply re-read next pass. This is
   the same evidence standard as everything else here: no decision on a fact
   the daemon has not observed twice.

4. **Ack timeouts are per timeout, on a configured cadence (QA05-03).** Two
   clocks replace the conflated one: `mayor.ack_timeout_ms` (has the Mayor
   had a fair chance?) and the new `mayor.ack_retry_ms` (how often is it
   worth asking again? — config v9 → v10, range 1 s…2 h, default 10 min). The
   wake artifact carries the notice number, so "still waiting" and "you have
   been told six times and have not answered" are distinguishable states for
   a stateless Mayor. ADR-0021 cl. 5's actual worry — one pathological decree
   spinning the wake queue — is answered by a number an operator can read,
   not by process lifetime.

5. **Wake identity is per-plane source identity (QA05-06).** `WakeEvent`
   carries a `WakeSource`: spool path for mail, correlation for webhooks,
   `decree_id#notice` for ack timeouts, `uid/session/restarts` for crashes,
   audit id for freight. Dedupe is on that identity, so a watcher double fire
   is still one event while two identical bodies from two sources are two
   wakes. The freight escalation gained the `audit_id` it never had — the
   plane could not previously distinguish two audits of one violation at all.
   The identity never reaches the packet: it is queue bookkeeping, not state.

## Consequences

- Config v9 → v10 is additive (`mayor.ack_retry_ms`), so an existing village
  parses unchanged and gets the default cadence rather than a silently
  different one. Core + CLI goldens updated.
- `DecreeExecutor::process` is now two calls (`process` verifies the
  directory, `process_with` does the work) and `write_ack`/`quarantine` are
  async: the executor holds a `Runtime` seam, installed with `with_runtime`.
  Callers that only need refusals keep the old constructors.
- Two-step quarantine means a genuinely bad file is quarantined one pass
  later than before. Tests that asserted a single scan quarantines now scan
  twice — which is the observable difference between "destroyed" and "given
  a chance to finish".
- `RejectReason` gains one variant (`foreign_provenance`); tokens are
  additive-only, so no existing ack token changes.
- Same shape, same risk, still **not** fixed here: `mailroute.rs` writes
  into 0700 subuid-owned mail spools with plain `fs` calls. The seam now
  exists and can be reused; Sprint 04/07 owns that wiring.