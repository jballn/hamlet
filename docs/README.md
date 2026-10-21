# Hamlet Documentation Index

Welcome to the Hamlet technical documentation. This directory and the repository root contain specifications, architecture records, developer conventions, and roadmaps guiding the design, implementation, and maintenance of Hamlet.

---

## Documentation Overview

| Document / Directory | Purpose | Audience |
|---|---|---|
| [**`SPEC.md`**](../SPEC.md) | The normative architectural and behavioural specification for Hamlet. Defines village filesystem layout, agent lifecycle, IPC mechanisms, security policies, and resource bounds. | Core developers, auditors, integrations |
| [**`docs/adr/`**](adr/) | Architecture Decision Records (ADRs 0001 through 0044). Documents every key technical decision, architectural pivot, wire encoding, and security boundary. | All contributors |
| [**`docs/BACKLOG.md`**](BACKLOG.md) | Tracked post-1.0 enhancements, architectural extensions, and future-work backlog items. | Roadmap planners, contributors |
| [**`roadmap/CONVENTIONS.md`**](../roadmap/CONVENTIONS.md) | Engineering conventions, code layout rules, purity seams, error handling doctrine, atomic write guarantees, and commit formatting. | All contributors |
| [**`roadmap/sprints/`**](../roadmap/sprints/) | Sprint-by-sprint execution plans and handoff records (Sprint 01 through Sprint 10). | Historical reference, audit traceability |

---

## Architecture Decision Records (ADRs)

All architectural decisions are cataloged sequentially in [`docs/adr/`](adr/).
Each ADR follows the immutable RFC format (*Context, Decision, Consequences*).

Key milestone ADRs include:
- **Workspace & Layout**: [ADR-0001](adr/0001-workspace-layout.md) (5-crate split & purity seam), [ADR-0003](adr/0003-tree-layout-api.md) (Tree layout API), [ADR-0004](adr/0004-charter-schema.md) (Charter schema v1).
- **Isolation & Containers**: [ADR-0010](adr/0010-podman-invocation-policy.md) (Podman invocation), [ADR-0011](adr/0011-host-permission-model.md) (Host permissions & subuid mapping), [ADR-0012](adr/0012-blueprint-execution-derived-images.md) (Derived images), [ADR-0021](adr/0021-freight-acl-sandboxes.md) (Freight ACL sandboxes).
- **Communication & Protocols**: [ADR-0019](adr/0019-mail-format-and-watcher-vocabulary.md) (Mail format & spools), [ADR-0020](adr/0020-dispatch-protocol.md) (Dispatch protocol), [ADR-0026](adr/0026-decree-protocol.md) (Decree protocol & acks).
- **Governance & Mayor**: [ADR-0028](adr/0028-mayor-session-lifecycle.md) (Mayor lifecycle), [ADR-0029](adr/0029-decree-execution-and-wake-triggers.md) (Decree execution & wakes), [ADR-0030](adr/0030-decree-boundary-hardening.md) (Decree boundary hardening).
- **Commission Lifecycle & Git**: [ADR-0036](adr/0036-commission-state-machine.md) (Commission state machine), [ADR-0037](adr/0037-git-push-mediation.md) (Git push mediation), [ADR-0038](adr/0038-deed-alterations-promotion.md) (DEED alterations promotion).
- **Stasis & Resource Policy**: [ADR-0039](adr/0039-stasis-formats-and-census.md) (Stasis formats & census), [ADR-0040](adr/0040-resource-bounds-and-hibernation-policy.md) (Hibernation & memory bounds).
- **Interfaces & Observability**: [ADR-0041](adr/0041-daemon-local-api.md) (Daemon local HTTP/WS API), [ADR-0042](adr/0042-cli-command-tree-and-tui-architecture.md) (CLI & TUI architecture), [ADR-0043](adr/0043-tui-rooms-landmark-projection.md) (TUI landmark projections), [ADR-0044](adr/0044-ws-protocol-and-web-arch.md) (WebSocket protocol & Web canvas).

Refer to the complete index in [docs/adr/README.md](adr/README.md).

---

## Verifying the Codebase

All changes must pass the full verification battery prior to landing:

```bash
# Run complete verification battery
./scripts/ci-local.sh

# Run multi-cycle village soak test
./scripts/soak-village.sh
```
