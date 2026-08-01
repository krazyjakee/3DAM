# ADR 0002 — 3D render crate boundary

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Related: [0001 — 3D render backend](0001-3d-render-backend.md),
[PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §4.3, [3d-handler-notes.md](../3d-handler-notes.md)

> **Shipped-graph amendment (2026-08-01):** the ownership boundary remains accepted, but the
> original “core must not depend on render” follow-up described below was too strict for the
> implemented orchestration. `dam-core` has an **optional direct dependency** on `dam-render`,
> activated by its `render` feature; `dam-server` enables that feature to produce server-side 3D
> thumbnails. Thus a default `dam-core` library build remains GPU-free, while the server-enabled
> dependency tree contains wgpu. The invariant is that GPU/window types do not enter core's public
> contract, `dam-render` alone owns wgpu, and it owns no window or event loop. See
> [tech-spec 01 §2](../tech-spec/01-architecture-and-crates.md#2-the-shipped-direct-graph).

## Context

3DAM is "one engine, three roles" ([PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §4.2): `3dam-core`
must stay **UI- and transport-agnostic**, yet the 3D handler needs GPU rendering for two
surfaces that must not diverge:

- the **headless thumbnailer** (CLI + `3dam serve`), and
- the **interactive orbit viewer** (desktop GUI).

If those grow separate renderers, thumbnails and live preview drift (different framing,
different shading), and `3dam-core` risks pulling in windowing/GPU deps it must not have.

MoGen already demonstrates the working shape of this split:

- `mogen-core` is **pure data** — `Mesh`, `SceneGraph`, `Aabb`, `Transform` — with **no I/O
  and no GPU** (`../godot-projects/mogen/crates/mogen-core/`).
- `mogen-render` is a **standalone renderer crate** that serves *both* the headless CLI
  thumbnail path and the live Studio viewer from one shader set / draw loop
  (`crates/mogen-render/`).
- The GUI (`mogen-studio`) keeps **pure-math viewer logic separate from GL drawing**:
  `pick.rs` (ray/BVH), `viewer/camera.rs` (orbit/free-fly) are math; `viewer/renderer/` is
  the GL code.

## Decision

Adopt the same three-layer boundary, on wgpu:

```
  3dam-core        pure data + math: Mesh, SceneGraph, Aabb, camera math,
   (no GPU,         pick (Möller–Trumbore + BVH), mesh cleanup.
    no windowing)   UI- and transport-agnostic. Satisfies §4.3.
        │
        ▼
  3dam-render      wgpu renderer: device/queue, shaders, PBR draw loop,
   (wgpu, no        offscreen render-to-texture. ONE renderer used by both
    windowing)      the thumbnailer and the viewer. No winit dependency.
        │
        ├─────────────────────┬───────────────────────────
        ▼                     ▼
  thumbnail worker      GUI viewer widget
  (CLI + serve):        (egui-wgpu / Iced): supplies a surface + input;
  offscreen render      3dam-render draws into it. Camera/pick math comes
  → RGBA8 → PNG.        from 3dam-core, not re-implemented here.
```

Rules:

- **`3dam-core` never depends on `wgpu`, `winit`, or any GPU/windowing crate.** Camera math,
  ray-picking, AABB, and mesh cleanup live here as pure `glam` code, reusable by both the
  renderer and headless logic (e.g. CLI geometry-stats without a GPU).
- **`3dam-render` owns wgpu and the shaders, but not the window.** It renders to an
  offscreen texture (thumbnailer) or into a surface handed in by the GUI (viewer). It does
  **not** create windows or run an event loop — that belongs to the GUI shell.
- **One renderer, two entry points.** Thumbnailer and viewer call the same draw path with
  the same shaders, so framing and shading match by construction.

## Consequences

**Positive**

- Server-mode thumbnails and desktop preview come from one codebase — no drift.
- `3dam-core` stays clean enough to link into the CLI and the API server without dragging in
  GPU deps (upholds §4.3 "UI- and transport-agnostic").
- Camera/pick/AABB math is unit-testable without a GPU or a window.

**Negative / risks**

- Requires discipline: it's tempting to let a `wgpu::Device` leak into core for convenience.
  The shipped dependency guard therefore checks that the edge stays optional and that no other
  GPU/windowing edge is introduced.
- The GUI toolkit must expose a wgpu surface we can render into; both egui-wgpu and Iced do,
  but this couples the boundary to that assumption — revisit if the toolkit choice changes.

**Follow-ups**

- When the toolkit ADR lands, confirm the surface-sharing mechanism (egui-wgpu
  `CallbackTrait` / paint callback, or Iced custom shader widget).
- ~~Add a dependency graph guard.~~ Done: `cargo xtask check-deps` enforces the reviewed direct
  graph and the optional status of `dam-core → dam-render`; it runs as part of `cargo xtask ci`.
