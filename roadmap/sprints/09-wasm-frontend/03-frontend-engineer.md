---
role: frontend-engineer
sprint: 09
step: 3
---

# 09/03 — Village canvas, sprites, chat bubbles

Read first: `roadmap/CONVENTIONS.md`, `SPEC.md` §12.3, ADR-0015, HANDOFF.md
(steps 1–2).

## Deliverable — the 8-bit village (render layer)

- Tile map + sprite sheets: guild hall, tavern, courier post, market stalls,
  keep/vault, manor, and census workshop sprites (one per active parcel),
  arranged per the committed tile map (village layout mirrors SPEC §2 —
  landmark↔tree mapping table in code comments).
- Agent sprites: state-driven animation (working = tool-swing, idle =
  loiter, sleeping/hibernated = zzz, crashed = fallen, steered = attention
  mark); sprite count/positions derive from state-delta frames ONLY.
- Chat bubbles over-the-head: bubble frames rendered with queueing +
  timeout; owner-only bodies enforced client-side as a display rule (server
  already sanitizes — defense in depth).
- Metrics HUD: per-sprite token/cost sparklines + a village-wide ticker from
  metrics frames; frame-budget guard (60fps target, degrade bubbles first —
  record policy).
- Canvas interaction: click/hover sprite → selection event (panels are
  step 04); keyboard navigation of landmarks (a11y: focusable landmark
  list, aria-live status region in the DOM shell).
- Resync: full re-render from snapshot frame; reconnect backoff.

All logic lives in the wasm view-model (pure: frames → scene graph) so CI
can golden-test scene output headlessly without a browser; thin JS glue does
blitting + input only.

Verify: view-model goldens (scripted frame sequences → scene snapshots);
manual browser smoke via `scripts/run-village-demo.sh` (daemon + demo
fixtures + open page); wasm size budget recorded. Finish: HANDOFF.md.
Commit: `feat(web): 8-bit village canvas + state-driven sprites (SPEC §12.3)`.
