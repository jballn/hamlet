# ADR-0037 — Git push mediation: bundle spool, validation, conflict policy, GitDiffReady

Status: Accepted
Sprint: 06-commission-lifecycle · Prompt: 01/05-architect (step 01)
Refs: SPEC §4 (mount rows; `commission<n>/git` is the only rw repo mount),
§7 (stateless executive wakes), §14 (open question resolved); CONVENTIONS §2
(singular governor, sanitize at boundary), §3 r2 (typed paths); ADR-0007
(create-new, no symlinks), ADR-0019 cl. 1 (LF-only text at boundaries),
ADR-0028 cl. 3 (wake queue identity), ADR-0030 (provenance is checked, not
assumed)

## Context

Sprint 05 left one SPEC question open (§14): agents hold rw access to
`commission<n>/git` — the only rw repo mount in the village — yet "shared refs
are never written by an agent". Two families were on the table: **(a)** a
daemon-side push proxy, or **(b)** git smart-http served through the manor.

(b) is rejected outright: it routes repo writes through the Mayor's 0700 home
(a provenance nightmare ADR-0030 spent four fixes on), stands up a network
service inside the parcel, and needs auth plumbing the village has no business
having. (a) keeps every git fact where SPEC puts them — the typed tree — with
the daemon as the only writer, exactly the singular-governor shape CONVENTIONS
§2 demands. This ADR pins the proxy's contract so step 04 can build the
executor against a frozen schema: spool location, bundle format, validation
rules, conflict policy, and the `GitDiffReady` wake that closes the loop.

## Decision

1. **Spool = per-uid home directory.** `keep/census/parcel<uid>/home/git-spool/`
   (agent-visible at `/home/<uid>/git-spool/`), accessor
   `tree::parcel_git_spool(uid)`. Chosen over freight: ownership is per-uid by
   construction (the home is 0700 subuid-owned — no ACL ceremony, no audit
   policy to extend), and freight's sandbox/quarantine machinery exists for
   *delivered* artifacts, not in-flight pushes. The daemon watches the spool;
   only names matching `^[A-Za-z0-9._-]{1,128}\.bundle$` are candidates —
   anything else in that directory is ignored, never touched.

2. **Format = git bundle.** A bundle is self-contained and verifiable offline
   (`git bundle verify`) with no server state; patch series are rejected as the
   primary format because they cannot be validated against the target without
   applying them. Cap: 32 MiB per bundle (typed constant, not a heuristic).

3. **Validation ladder (all must pass, cheapest first).**
   (i) regular file, size ≤ cap; (ii) `git bundle verify` clean; (iii) exactly
   one ref — `refs/heads/agent/<uid>` — any other or extra ref is refused;
   (iv) every commit in the range carries committer **and** author email
   `<uid>@hamlet.local` (the identity the daemon seeds at provisioning);
   (v) fast-forward: the bundle's base must equal the current `agent/<uid>` tip,
   or the commission's `main` tip on first push. Each failure is a typed
   rejection (`Oversize · VerifyFailed · NotOwnBranch · ForeignIdentity ·
   NonFastForward`) — never a shell-exit mystery.

4. **No hooks, ever.** Application grafts bundle objects into the bare repo and
   updates `refs/heads/agent/<uid>` directly; the daemon never runs `git push`,
   receive-pack, or any hook path. The rw mount row stays true in practice, not
   just in the mount table: the only process that moves refs is the daemon.

5. **Conflict policy = branch-per-agent.** An agent writes only
   `agent/<uid>`; shared refs (`main`) move only by operator/Mayor action, out
   of this sprint's scope. Non-fast-forward is refused, not merged — a merge is
   a governance decision, and the daemon will not make one silently.

6. **Consumption is atomic and observable.** A consumed bundle leaves the spool:
   accepted bundles are archived under `keep/census/archives/git/<n>/<uid>/`
   (audit trail, same shape as freight archives); refused bundles are renamed in
   place to `<name>.bundle.<reason>` so the agent sees the receipt and the
   scanner cannot loop. Every refusal is also mailed to the slot with the full
   typed reason (refusals are never silent) and traced.

7. **`GitDiffReady` closes the loop.** On a successful apply the Mayor is woken
   with `EventKind::GitDiffReady { commission, base, head }`: `base` = previous
   `agent/<uid>` tip (or `main` tip on first push), `head` = new tip — both SHAs,
   which fit `SafeName`. Body = unified diff `base..head`; above 64 KiB the body
   degrades to the deterministic `--stat` summary of the same range. Never a
   truncated diff: the full bytes remain in the bare repo (tree truth) and the
   Mayor can ask for specifics by name.

## Consequences

- New core module `gitmed`: constants, ref/email helpers, bundle-name
  validator, rejection enum — the frozen schema step 04 builds against.
- The watcher gains one reaction row (`Reaction::GitBundle { uid, name }`);
  spool paths classify by typed accessor, not string sniffing.
- Branch state lives in the bare repo (tree truth), not the charter: the
  charter tracks commission *state*, the repo tracks *refs*; two truths, no overlap.
