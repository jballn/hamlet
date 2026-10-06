# ADR-0034 — The manor home brace is recursive: a private home owns its whole subtree

Status: Accepted
Sprint: 05-mayor-loop · Prompt: 04/04-daemon-engineer (QA04-23); cl. 5 added
  by the 04/07-container-engineer pass (QA04-29)
Amends: ADR-0011 cl. 2 (the `chown` gains `-R` for one caller; cl. 5 pins its
  traversal policy)
Refs: SPEC §4 (mount table), §7 (the manor); ADR-0026 cl. 2 (decree dir),
ADR-0030 cl. 4 (unshare seams); ADR-0010 cl. 5 (typed argv)

## Context

QA04-23: `manor_guarded::podman_manor_decrees_are_authored_only_by_uid_1000`
failed — `sh` as the real Mayor could not write
`keep/census/manor/home/decrees/probe.json`. The host was exonerated by hand
(same mount, `--user 1000:1000`, same host ownership ADR-0011 cl. 2 expects)
so the gap had to be in provisioning. The container's `ls -ln /home/1000` says
exactly where: `home/` is `1000 1000`, `decrees/` is `0 0` mode 0755.

Cause: `HamletTree::ensure` (and the provisioner) create `home/decrees/` as the
**daemon user**, and `HostPermissions::apply_manor_home` chowned the home
*non-recursively*. Ordering was never the bug — the brace was applied before
`podman run` — the brace itself was one level too shallow. On the host nothing
looks wrong (the daemon can always write there); inside the container the Mayor
owns its home and not the one directory in it that decides the village.

## Decision

1. **A slot's private home is owned as a whole, not at its root.**
   `apply_manor_home` applies `chmod 0700` to `manor/home/` **and** to
   `manor/home/decrees/`, then `podman unshare chown -R 1000:1000 -- <home>`
   (ADR-0011 cl. 2 with `-R`). Parcel homes and mail spools keep the
   non-recursive form: the daemon pre-creates no children inside them.
2. **`-R` is a typed seam, not a shell string.** `Runtime` gains
   `unshare_chown_recursive`, built by the pure
   `unshare_chown_recursive_argv` alongside `unshare_chown_argv`: same
   validation (absolute, UTF-8, no control chars, no leading `-`), same `--`
   guard, `-R` a fixed operand slot. Recorded and golden-tested on the fake
   seam like every other podman argv.
3. **The decree directory's mode is explicit, not inherited from a umask.**
   A directory every other slot must be unable to read may not depend on the
   daemon's umask.
4. **Guarded suites observe the Mayor's tree from the daemon's userns.** A
   bare-host read of a 0700 subuid-owned home is refused by the kernel *by
   design*; the assertions use the same `podman unshare` vantage ADR-0011 cl. 4
   already mandates for writes, and assert container-side ownership with
   `ls -ln`. An assertion that read the bare host would have asserted the model
   is broken.
5. **The recursive brace MUST NOT dereference symlinks; the argv says so.**
   The tree `apply_manor_home` walks with `-R` is the Mayor's **writable**
   home (cl. 1 exists precisely so the Mayor can author decrees), so it is,
   by construction, the one private tree in the village that an adversary
   populates with symlinks. `-R` recurses; what it does *at* a symlink is
   decided by a separate traversal policy:
   * `-P` (GNU default) — chown the link itself, never its target. **Required.**
   * `-L` — follow every symlink and chown what it points at.
   * `-H` — follow only command-line operands (equivalent to `-P` here, but
     leaves the default implicit for the rest).

   A dereferencing chown hands **the Mayor's subuid to any path the link
   names**, including paths outside the village tree and outside any mount the
   container can see — a write primitive against the host filesystem,
   executed with root-equivalent privilege inside the daemon's userns
   (ADR-0011 cl. 4). SPEC §4/ADR-0011 cl. 2 make ownership *the* isolation
   boundary, so that boundary may not rest on an undocumented flag default.

   Therefore `unshare_chown_recursive_argv` emits **`-RP`** as a fixed
   operand pair: the policy is in the argv, so a future "add `-L`/drop `-P`"
   edit is a diff against this clause, and a host whose `chown` lacks `-P`
   (BusyBox) **fails loudly** instead of silently dereferencing. The
   non-recursive brace (cl. 2) carries no traversal flag at all — it names a
   single path.

   *Measured (QA04-29, live rootless host, `podman unshare chown` over a
   manor-home-shaped tree containing `link -> …/target`):* `-RP` ⇒ rc=0, the
   link itself moves to the Mayor's host uid, `target/` and `target/f1` are
   **untouched**. `-RL` ⇒ rc=0, **no warning**, and `target/` *and*
   `target/f1` both move to the Mayor's host uid — the cross-tree
   escalation, real and silent. Both are pinned: hermetically by
   `podman.rs::chown_recursive_argv_pins_no_dereference` and
   `hostperm.rs::manor_home_is_0700_and_chowned_recursively_for_the_mayor_slot`
   (exact argv), and on a live host by
   `manor_guarded::podman_manor_home_brace_does_not_follow_symlinks_out_of_the_tree`.

   The test-side `reclaim` helpers (guarded suites handing a subuid-owned
   tempdir back to the daemon user) obey the same rule: they walk agent-
   writable trees too.

## Consequences

- The Mayor authors decrees in the real container; QA04-23's claim 1 passes on
  live podman, and claim 2 stays green (a recursive chown widens nothing: the
  subtree is the Mayor's alone).
- Re-provisioning re-asserts the same brace — idempotent, not one-shot.
- Two latent bad assertions in that suite died behind the original failure and
  were corrected here: the write-observability check (see cl. 4) and a
  "forgery" vector that asserted a slot is denied **its own** 0700 mail spool.
- Known gap, not fixed here (separate defect, needs its own seam and ADR):
  `DecreeExecutor`'s *reads* of the decree directory have no `unshare`
  fallback, while its writes do (ADR-0030). Under the spec'd service user a
  scan therefore cannot traverse the manor home.
- Naming `-P` trades a silent failure for a loud one: on a host whose `chown`
  is BusyBox (no `-P`) manor provisioning now fails at the brace instead of
  proceeding. That is the intended direction — the alternative is a
  privileged cross-tree chown with no error at all (cl. 5). The guarded
  battery's `guarded-service-user.sh --doctor` preflight should grow a
  `chown -P` capability probe if a BusyBox host ever shows up in CI.
6. **QA04-28 fixed the in-container ownership assertion** (`manor_guarded.rs`
   line 261) from `ls -ln … contains("1000")` to per-path `stat -c '%u %a'`
   asserting `1000 700`. The tautology was introduced by ADR-0034 cl. 4's own
   prescription; this clause records the correction and the rationale.
