# ADR-0010 — Podman invocation policy: CLI-only, argv-only, rootless

Status: Accepted (cl. 3 amended by ADR-0011)
Date: Sprint 02 (step 02/01)
Refs: SPEC §4, §14 [DEV]; CONVENTIONS §2 (singular governor), §5
Note: Sprint 02 prompts cite this as "ADR-0007" — see ADR-0009's
renumbering note.

## Context

SPEC §4 locks the runtime to rootless Podman invoked by the daemon through
tokio process pipes — no libpod bindings, no Docker, no privileged mode.
Sprint 02's exec layer (step 02) and mount rendering (step 01) need one
binding policy for *how* podman is called: argument construction,
user-namespace mapping, forbidden flags.

## Decision

1. **CLI only.** Every podman interaction is `tokio::process::Command` on
   the configured `podman_bin` with piped stdio. No shell is ever involved:
   no `sh -c`, no command strings — arguments are built as `Vec<String>`
   and passed via `Command::args`; interpolating untrusted data into a
   shell line is prohibited workspace-wide.
2. **Args from typed sources only.** Volume flags come exclusively from
   `MountSet::to_podman_args` (SPEC §4 table, closed enum); env flags from
   validated `Blueprint::env`; image refs from `BlueprintMeta`; names from
   `SafeName` or numeric UIDs. Raw paths/specs never reach argv.
3. **Rootless + UID mapping.** The container user equals the charter UID
   slot: `--userns=keep-id:uid=<uid>,gid=<uid>` plus `--user <uid>:<uid>`
   (Mayor: uid 1000, `MountSet::for_mayor`). Never rootful, never `sudo`,
   never `--userns=host`.
4. **Forbidden flags** (QA-grepped at the Sprint 02 gate): `--privileged`,
   `--userns=host`, `--pid=host`, `--network=host`, any volume/device
   mounting a socket (`podman.sock`, `docker.sock`, `/run`, `/var/run`),
   any `--authfile` on agent containers. Required hardening:
   `--security-opt=no-new-privileges`, `--cap-drop=ALL` (caps added only
   by future ADR).
5. **Failure policy.** Failed invocations surface a structured error with
   podman's stderr verbatim and the full argv (never env values/secrets);
   `run` is detached (`-d`); lifecycle via `stop`/`kill`/`state`.
6. **Volume values** are `host:container[:ro]`; a village root containing
   `:` fails hard at render (`MountError::HostPathColon`), never quoted.

## Consequences

- The `Runtime` trait seam (step 02) records argv goldens under
  `FakeRuntime`; real podman runs only behind `HAMLET_PODMAN_TESTS=1`.
- Isolation invariants (ro `/var/git/` + `/var/context/`, no cross-UID
  reads) are enforceable by grepping argv — no hidden mount channels.
- Resource limits (Sprint 07) and restart/OOM policy (Sprint 03) stay
  `[DEV]`; this ADR fixes construction, mapping, and prohibitions only.
