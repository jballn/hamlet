# Sprint 03 — Agent Engine

**Goal:** Live pi coding-agent sessions running inside provisioned parcels,
supervised by the daemon: JSON-RPC over podman-exec stdio, sequential stdin
mail delivery, out-of-band `interrupt_and_steer`, and daemon-parsed telemetry
into sled.

**Scope boundary:** No mail *routing* (Sprint 04 delivers messages; this
sprint builds the delivery pipe + queue). No Mayor loop (Sprint 05). Agents
are scripted fakes in tests; one guarded live smoke test only.

**Exit criteria (SPEC §6):**
- Session protocol types + ADR-0008 (`interrupt_and_steer` wire schema).
- `hamlet-daemon::supervise`: spawn pi session in parcel container, health
  tracking, restart policy (limits + backoff in config), crash → charter
  status + TITLE.md lineage stamp + sled trace.
- Per-recipient sequential stdin delivery queue: never interleaves messages
  mid-task; task-boundary semantics tested.
- `interrupt_and_steer` injection bypasses the queue on the live pipe.
- Telemetry parse: tokens/cost/latency/trace extracted daemon-side from the
  JSON-RPC stream into the Sprint 01 sled API. No agent self-reports.
- `FakeAgent` mock seam committed (scripted JSON-RPC stdio binary).
- Branch merged, tag `sprint/03-agent-engine`.

## Prompt sequence
1. `01-architect.md` — session protocol + interrupt schema (ADR-0008)
2. `02-runtime-engineer.md` — spawn & supervision
3. `03-runtime-engineer.md` — delivery queue + divine interruption
4. `04-runtime-engineer.md` — JSON-RPC telemetry parse → sled
5. `05-qa-engineer.md` — mock-agent adversarial suite
6. `06-ci-agent.md` — gate
7. `07-commit-agent.md` — merge & tag
