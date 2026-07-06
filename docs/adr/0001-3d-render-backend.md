# ADR 0001 — 3D render backend: wgpu, not OpenGL

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Supersedes: — · Related: [0002 — 3D render crate boundary](0002-3d-render-crate-boundary.md),
[3d-handler-notes.md](../3d-handler-notes.md)

## Context

3DAM's 3D media handler must do two rendering jobs (see
[PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.2, §6.4):

1. **Headless thumbnail / multi-view render** — offscreen render-to-image, run from the
   CLI *and* from `3dam serve` on a machine that may have **no display server** (a NAS, a
   CI box). This is load-bearing: server-mode preview generation (§6.8) depends on it.
2. **Interactive orbit viewer** — a live preview embedded in the desktop GUI.

Both must work across Linux, Windows, and macOS from one codebase, and must not regress the
"fail-soft on one bad asset" reliability requirement (§8).

We have a strong reference implementation to learn from: **MoGen**
(`../godot-projects/mogen`), a production Rust 3D generator that solves exactly these two
jobs. Its choices are instructive precisely because it hit the platform edges we will:

- MoGen renders on **glow (OpenGL 3.3) + glutin + winit**.
- To render headless it needs a **per-platform split**: surfaceless **EGL**
  (`khronos-egl` + `libloading`) on Linux (`crates/mogen-render/src/headless_egl.rs`), and a
  **hidden winit window + glutin context + FBO** on macOS/Windows
  (`crates/mogen-render/src/headless.rs`). Two code paths, two sets of failure modes, to get
  one offscreen framebuffer.

Our candidate stack ([PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §7) already names **wgpu** for
both the embedded viewer and thumbnail rendering. This ADR confirms that against the glow
alternative that MoGen proves is viable.

## Decision

**Use `wgpu` as the sole 3D rendering backend**, for both the headless thumbnail path and
the interactive viewer. Do **not** adopt MoGen's glow/glutin/EGL stack.

Rationale:

- **Headless is one path, not three.** wgpu requests an `Adapter`/`Device` with **no
  surface** and renders to an offscreen `Texture`, then `copy_texture_to_buffer` +
  `map_async` reads RGBA8 back to the CPU for PNG encoding. There is no window, no event
  loop, and **no per-OS branch** — the same code runs on a headless Linux server, a Windows
  workstation, and macOS. This directly deletes the `headless.rs` / `headless_egl.rs` /
  hidden-window trichotomy that MoGen carries.
- **Backend portability without our code changing.** wgpu targets Vulkan / Metal / DX12 /
  GL under one API, so we inherit the platform matrix instead of hand-porting a GL context.
- **GUI integration is first-class.** `egui-wgpu` (if we pick egui) and Iced-on-wgpu both
  render into a wgpu surface, so the viewer and the thumbnailer **share one renderer and one
  shader set** rather than the GL-callback wiring MoGen uses (`egui_glow` custom paint in
  `crates/mogen-studio/src/viewer.rs`).
- **Future compute.** §6.2's per-view shape embedding may want GPU compute; wgpu exposes
  compute shaders, GL 3.3 does not.

What we still **borrow from MoGen** — because these are backend-agnostic algorithms, not GL
code (captured in [3d-handler-notes.md](../3d-handler-notes.md)):

- Bounding-sphere auto-fit camera framing (`radius * 2.8` at 45° FOV; `fit_distance` stored
  separately from user `zoom`) — reproducible framing for versioned thumbnails.
- The *idea* of a two-tier GLB read: cheap chunk-level metadata scan on ingest, full
  geometry decode only on preview.
- Möller–Trumbore + lazy median-split BVH for click-picking (pure `glam` math).
- Mesh cleanup + AABB math.

## Consequences

**Positive**

- One offscreen-render code path across all three OSes; headless serve-mode thumbnails work
  by construction, not by platform luck.
- Viewer and thumbnailer share a renderer + shaders (see
  [ADR 0002](0002-3d-render-crate-boundary.md)).
- Aligns with the spec's stated direction; no re-litigation later.

**Negative / risks**

- **Headless wgpu still needs a GPU adapter.** On a truly GPU-less server we must fall back
  to a software adapter (e.g. Vulkan **lavapipe** / Mesa llvmpipe, or a `fallback_adapter`
  request). Spike this early on a headless Linux box — it is the one place wgpu can surprise
  us, and it is exactly the environment §6.8 targets. MoGen's surfaceless EGL sidesteps this
  by using whatever GL the box has; we trade that for portability and must verify the
  software-raster path.
- wgpu's API churns faster than GL; pin versions and isolate behind our render crate.
- Slightly more boilerplate than glow for a first triangle; irrelevant at project scale.

**Follow-ups**

- Spike: headless wgpu render-to-PNG on a Linux box with **no display and no discrete GPU**
  (confirm lavapipe/llvmpipe fallback). This is the gate on the whole decision.
- Record the GUI toolkit choice (egui vs Iced) separately; both sit on wgpu, so this ADR is
  independent of that one.
