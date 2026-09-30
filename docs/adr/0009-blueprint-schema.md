# ADR-0009 — Blueprint TOML schema & immutability encoding

Status: Accepted
Date: Sprint 02 (step 02/01)
Refs: SPEC §4, §5, §9.4, §11.4, §14 [DEV]; CONVENTIONS §2, §9
Note: Sprint 02 prompts cite this as "ADR-0006"; 0006–0008 were consumed by
Sprint 01 (numbers are never reused). Mapping: prompt-0006 → **0009**,
prompt-0007 → **0010**.

## Context

SPEC §5 fixes blueprints as versioned TOML in `guild/blueprints/`, parsed by
`hamlet-core`, executed by the daemon; the schema is `[DEV]` (SPEC §14).
TOML parsing needs a parser crate; the locked stack (SPEC §3) has none, and
CONVENTIONS §2 requires an ADR for dependency additions.

## Decision

1. **Schema** (`hamlet-core::blueprint`): `[meta]` = `name` (SafeName per
   ADR-0002), `version` (u32 ≥ 1, bumped only by promotion), `base_image`
   (`^[A-Za-z0-9][A-Za-z0-9._/@:-]*$`, ≤ 256). `[[tools]]` =
   `{ kind: pixi|nvm|cargo|apk, name, version_spec? }`; tool names match
   `^[A-Za-z0-9][A-Za-z0-9._+-]*$` (apk needs `+`, e.g. `g++`), version
   specs `^(?:\*|[A-Za-z0-9][A-Za-z0-9._+*^~=<>,-]*)$` — shell-safe by
   construction, never leading `-`. `[env]` = **non-secret** defaults: keys
   match `^[A-Za-z_][A-Za-z0-9_]*$`, rejected if they match
   `(?i)(key|secret|token|passw|credential)` (SPEC §11.4: secrets live only
   in `.env`). `[skills] refs` = SafeNames into `market/skills/`. All
   sections `deny_unknown_fields`; `[meta]` required, rest default empty.
2. **Errors**: typed `BlueprintError` variants each carry the exact field
   path (`tools[2].version_spec`, `env.KEY`, `skills.refs[1]`); TOML
   syntax/type errors wrap the `toml` crate message (key path + position).
   First semantic violation wins; order meta → tools → env → skills.
3. **Immutability is structural**: private fields, accessors only, no
   setter, no `&mut self` method. SPEC §9.4 evolution (Sprint 06) builds a
   *new* `Blueprint` with `version + 1`; in-place mutation is unrepresentable.
4. **Dependency**: `toml 0.8` joins the runtime allowlist
   (`check-deps.sh`) and workspace deps — the only TOML parser permitted.
5. **Default**: `defaults/village-alpine.toml` (Alpine 3.20 + uutils + git +
   acl + Pixi/NVM/Cargo, caches → `/var/pkgcache/`) ships committed, is
   `include_str!`-embedded as `DEFAULT_BLUEPRINT_TOML`, golden-tested.

## Consequences

- Sprint 04 bootstrap executes `ToolSpec`s verbatim; no free-form install
  strings exist → no injection surface from blueprint content.
- Sprint 06 promotion emits whole new TOML documents (version bump), never
  patches in place; a `Blueprint` → TOML render path is deferred to it.
- The env-key heuristic may reject benign keys containing "key" — accepted:
  false positives fail closed with a precise error; secrets never fail open.

