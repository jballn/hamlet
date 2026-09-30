# ADR-0013 — pkgcache trust model: shared mirror, digest-verified staging

Status: Accepted
Date: Sprint 02 (step 04)
Refs: SPEC §4 (`market/pkgcache/` rw, "cache semantics"; isolation
invariants); ADR-0011 cl. 3 (pkgcache 01777), ADR-0012 cl. 4 (pin table),
ADR-0007 (never follow, never adopt), ADR-0006 (atomic writes)

## Context

Step 04 turns the shared pkgcache mount into the toolchain mirror, so its bytes
become **executables in other parcels' homes** — and ADR-0011 makes it mode
01777, so any agent UID may create or replace files there. Unverified, one
compromised agent hands a poisoned `cargo` tarball to every later parcel.

## Decision

1. `market/pkgcache/dist/` is the mirror, created **host-side** by bootstrap at
   01777 (sticky: every slot may publish, none may clobber another's entry).
   Paths only via `HamletTree::market_pkgcache_dist` / `pkgcache_artifact(file)`;
   the latter rejects separators, `..`, `.`/empty and leading `-`.
2. Every artifact follows **stage privately → verify → use**: HIT = `cp
   /var/pkgcache/dist/<f> $HOME/.hamlet/stage/<f>`; MISS = `curl -fsSL --retry 3
   -o <stage> <https url>`; then `sha256sum <staged>` vs the pin. A mismatch
   aborts (`DigestMismatch`) **before** any extract, exec or publish.
3. Publish is `ln <staged> /var/pkgcache/dist/<f>`: a hard link refuses an
   existing name instead of following a planted symlink, and publishes exactly
   the verified inode. Failure is non-fatal (it is a cache).
4. Host-side probes use `symlink_metadata`: only a regular, non-empty file is a
   hit — a planted symlink or partial download is a MISS, never followed.
5. Bootstrap writes nothing into a home from the host (ADR-0011): profile and
   manifest go through `podman exec … tee <path>` as the slot uid; the only
   host-side write is DEED.md (daemon-owned 0644), atomic per ADR-0006.
6. Idempotence is byte comparison of the plan's desired `toolchain.json` +
   `.profile` against what `cat` returns, plus tool probes: a match is a verified
   no-op (zero downloads/installs), and the DEED baseline is appended once per
   manifest digest.

## Consequences

- A poisoned mirror entry costs a re-download, never a compromise: the trust
  anchor is the reviewed pin table, not the cache.
- Rust installs via `rustup-init --default-toolchain none` plus pinned component
  tarballs `rustup toolchain link`ed offline: **every byte** is pin-verified.
- Cargo registries are per-slot (`CARGO_HOME` in the home); a shared registry
  mirror is a Sprint 07 concern and needs the same digest discipline.
- Step-05 QA: assert the digest refusal, the symlinked-mirror miss, and that no
  bootstrap argv writes outside `/home/<uid>/` + `/var/pkgcache/dist/`.
