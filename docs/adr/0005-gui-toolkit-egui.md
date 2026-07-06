# ADR 0005 — GUI toolkit: egui / eframe

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Supersedes: — · Related: [0001 — 3D render backend](0001-3d-render-backend.md),
[0002 — 3D render crate boundary](0002-3d-render-crate-boundary.md),
[PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §7, §10, [tech-spec 12](../tech-spec/12-desktop-gui.md)

## Context

The native desktop GUI ([tech-spec 12](../tech-spec/12-desktop-gui.md)) is a thin front-end
over `LibraryService`. PRODUCT_SPEC §7 named two native-Rust candidates, both on **wgpu**:
**egui/eframe** (immediate-mode) and **Iced** (retained, Elm-like). [ADR 0001](0001-3d-render-backend.md)
explicitly deferred this pick to "a later ADR"; tech-spec 12 was written toolkit-agnostic and
flagged a rendering/perf spike as the intended tie-breaker.

The product shape that drives the choice: a **dense, dark, virtualised tooling UI** — grid +
sortable attribute table at 100k+ rows at 60 fps — with an embedded wgpu 3D viewer, and full
parity with the shared engine. The web client (React) ships first and settles interaction
patterns (PRODUCT_SPEC §9, web-first front-end sequence); the desktop GUI follows.

## Decision

**Use `egui` / `eframe` as the desktop GUI toolkit.** We adopt it now on the tradeoffs below
rather than gating on the perf spike; the spike becomes a **validation** step (confirm 60 fps
and idle CPU on the real virtualised grid), not a prerequisite for committing.

Rationale:

- **Immediate-mode fits virtualised, data-dense views.** `ScrollArea::show_rows(total, …)`
  builds only visible rows directly — the 100k-row grid/table (tech-spec 12 §4) is idiomatic
  rather than hand-built, and selection/filter/scroll state is trivial to keep across a
  grid↔table toggle.
- **First-class wgpu embedding.** `egui-wgpu` hands a paint callback a wgpu render pass into a
  rect, so [ADR 0002](0002-3d-render-crate-boundary.md)'s `3dam-render` viewer draws onto the
  **same wgpu device the toolkit already owns** — no second GPU context (tech-spec 12 §6.1).
- **Ecosystem for tooling UIs.** `egui_extras::TableBuilder`, broad widget coverage, and a large
  body of dev-tool precedent (the sibling `mogen-studio` uses egui) lower risk and match the
  game-dev aesthetic.
- **Iteration speed.** Immediate-mode's edit-run loop is fast, and the mental model is simple
  for a small team building a lot of dense surfaces.

## Consequences

**Positive**
- One documented viewer-embedding path (`egui-wgpu` paint callback) shared with the headless
  renderer's device; no cross-context copy.
- Virtualisation and stateful view-switching are cheap in immediate mode.
- Aligns with tech-spec 12's egui-flavoured pseudocode, which becomes normative rather than
  illustrative.

**Negative / risks**
- **Accessibility is the weaker side of immediate-mode.** egui integrates `accesskit`, but
  screen-reader/keyboard-tree fidelity needs explicit attention (tech-spec 12 §7). Tracked, not
  blocking.
- **Retained-mode ergonomics forfeited.** Complex stateful widgets are more manual than in Iced;
  acceptable given the UI is mostly dense data views, not bespoke controls.
- Perf at 100k+ is asserted, not yet measured — see follow-up.

**Follow-ups**
- **Validation spike (not a gate):** the tech-spec 12 §1 rendering/perf spike — virtualised
  grid + table at 100k+ rows over the real derivative cache, embedded [06](../tech-spec/06-3d-render.md)
  viewer in a mid-layout rect, sustained scroll frame time *and* idle CPU. Confirms the 60 fps
  target; if egui misses it irrecoverably, revisit this ADR.
- Settle the `3dam-frontend` shared-helper crate placement (tech-spec 12 / [01](../tech-spec/01-architecture-and-crates.md)
  Open questions) now that the toolkit is fixed — `3dam-gui` links `egui`/`eframe`.
- Confirm `accesskit` coverage meets DESIGN_GUIDELINES §3.5.
