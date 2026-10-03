# ADR-0025 — The derived image CLEARS its parent's ENTRYPOINT/CMD

Status: Accepted
Sprint: 04 · Prompt: qa-engineer finding QA04-06 (fixed by container-engineer)
Amends: ADR-0012 cl. 2 only (cl. 1, 3–5 stand)
Refs: SPEC §4 (no init system, daemon-supplied keep-alive); ADR-0010 cl. 4
(hardening flags), ADR-0011; `bootstrap::containerfile`, `podman::KEEP_ALIVE_COMMAND`

## Context

QA04-06 observed that the current upstream `alpine:3.24` (built 2026-09-17)
carries an explicit `CMD ["/bin/sh"]` layer, while the generated Containerfile
only *omitted* CMD. ADR-0012 cl. 2 declared the derived image to have "no
ENTRYPOINT/CMD/USER" — an invariant that omission cannot guarantee, because
Containerfile instructions are additive: a derived image inherits whatever body
its `[meta].base_image` declared. Every parcel image built since Sprint 02
therefore inherited an interactive root shell. Nothing broke operationally
because the daemon's `run` argv always appends an explicit non-empty command
(`KEEP_ALIVE_COMMAND`, and `validate_cmd` refuses an empty one), so the
inherited CMD was unreachable — but the invariant was false, and it is exactly
the kind of gap a base-image refresh can turn into a live shell.

## Decision

1. The generated Containerfile **explicitly clears** the inherited body:
   `ENTRYPOINT []` and `CMD []` after the `RUN apk add` layer. "Declares no
   entrypoint/cmd" is now enforced by construction for *any* permitted parent,
   not by the accident of a parent shipping none.
2. This is the **fail-closed** direction: a `run` that somehow reached the
   image with no daemon-supplied command now *errors* (the OCI runtime cannot
   find an executable) instead of quietly starting `/bin/sh` as root. The
   guarded suite pins both halves: the commandless run must fail, and the
   daemon's real paths (`<image> sleep 0.1`, detached `sleep infinity`) must
   still work.
3. `USER` stays *omitted*, deliberately — it is not inherited-sensitive in the
   same way, and the run argv's `--user <uid>` is the authority (ADR-0010 cl.
   3). A future `USER` instruction would be a spec change to this clause.
4. The fingerprint moves as a consequence: the new definition hashes to
   `localhost/hamlet/village-alpine:v1-f2-c0a8b8a8b1160d5c8`, so every
   existing parcel image is superseded and rebuilt once (same mechanism as
   ADR-0024 cl. 1). No manual tag re-pin exists; the tag is derived.
5. This is an **amendment**, not a reversal: the intent of ADR-0012 cl. 2 is
   unchanged. Only the mechanism moves from "omit" to "clear", and the two
   lines are part of the golden Containerfile, so a future rewrite that drops
   them fails the unit test, not a production parcel.

## Consequences

- The pinned invariant is finally true: `podman image inspect` shows
  `Cmd=[]`/`Entrypoint=[]` for the derived image even though its parent
  declares a shell — asserted by a guarded test that first pins the premise
  (the parent really does declare a CMD).
- A future base image that ships an `ENTRYPOINT` can no longer wrap every
  parcel body with it, which would have bypassed ADR-0010 cl. 4 hardening.
- Blueprint `version` is untouched (ADR-0009 cl. 1): the fingerprint, not the
  version, is what changes.
- Containerfiles are now four instructions instead of two; anyone reading a
  generated build context needs the comment block to see why.