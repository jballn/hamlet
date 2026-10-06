# ADR-0031 — The freight audit keeps the ACL listing's `# owner:` header

Status: Accepted
Sprint: 05-mayor-loop · Prompt: QA04-07/08/09 blocker resolution
Amends: ADR-0021 cl. 1
Refs: SPEC §4 (Freight ACLs), §8.3; ADR-0011 cl. 2 (subuid ownership), cl. 4
(host writes/reads through the userns); ADR-0021 cl. 6 (promotion-read);
CONVENTIONS §5 (guarded battery), §9 (testing doctrine)

## Context

Resolving QA04-07/08/09 meant finally running the guarded freight suite for
real on this host with a disk-backed `TMPDIR`. It failed immediately, and not
for a host reason:

```
freight ACL invariants hold inside real containers:
PodmanError { kind: InvalidSpec("getfacl listing: ACL listing has no '# owner:' line") }
```

`getfacl_cmd` emitted `getfacl -p -n -c`, and `-c`/`--omit-header` suppresses
**exactly** the `# owner:` / `# group:` comment lines. The pure audit's first
two rules — "owner is a charter slot", the group check — are derived from
those lines, so with `-c` *no real listing on any host could ever parse*. The
FakeRuntime fixtures carry the headers, so all 17 `tests/freight.rs` tests
were green: the seam hid a code path that had never executed. (The sprint-04
gate that reported this suite green on real podman had run it *without*
`-- --ignored`, i.e. zero tests; see ADR-0032.)

Verified on GNU acl 2.4.0, the `acl` package ADR-0021 cl. 2 names as the host
prerequisite: `getfacl -p -n -c` prints no `# owner:` line.

## Decision

1. **The audit command keeps the header**: `getfacl -p -n -- <path>`. `-p`
   (no leading-slash stripping), `-n` (numeric ids — in-userns slot UIDs via
   `podman unshare`) and the `--` separator stay; `-c` goes and never comes
   back.
2. **The parser stays strict.** `# owner:` and `# group:` remain mandatory
   (`MissingOwner` / `MissingGroup`); `# file:`, `# flags:` and blank lines are
   the only tolerated noise. "Owner is a charter slot" cannot be audited
   without an owner, so degrading the parser to accept a header-less listing
   would trade a loud failure for a silent wrong verdict.
3. **The flag is pinned by a test** (`getfacl_payload_keeps_the_owner_header`)
   and the argv golden updated, so a tidy-up cannot silently re-add it.

The same run exposed a second defect in the same plane, in the same way — a
path the fake-runtime seam had never executed. **QA04-13 (MEDIUM)**: see
ADR-0033.

## Consequences

- The freight audit is executable for the first time against a real listing.
  The guarded suite now genuinely verifies what 17 fixture tests only
  approximated: on a compliant host it must pass; if the parser is still too
  strict for a real listing, it fails loudly instead of never running.
- Output is two extra comment lines per audit. Irrelevant to a per-directory
  audit; the parse is line-oriented.
- No config, schema, tree or wire change. Nothing else in the workspace reads
  ACL listings: `parse_getfacl` has one caller, the freight audit.