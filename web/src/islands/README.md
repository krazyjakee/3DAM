# Model viewer and waveform renderers

The heavyweight `wgpu` model viewer lives in the Rust crate
[`crates/3dam-viewer`](../../../crates/3dam-viewer) and is compiled to `wasm32-unknown-unknown`
with `wasm-pack`. The audio waveform is deliberately a lightweight Canvas2D renderer. This folder
is the **DOM-side boundary** the React chrome builds on
(tech-spec 09 §B.3, ADR 0009 §9).

## What's here

- [`index.ts`](index.ts) — framework-agnostic loaders and typed handles:
  - `createModelViewer(canvas) → ModelViewerHandle` — interactive 3D (server-decoded `DMSH` preview
    mesh; every Assimp format, textured).
- [`WaveformIsland.tsx`](WaveformIsland.tsx) — Canvas2D audio waveform using server-produced peaks,
  with Web Audio decoding only as a fallback for assets awaiting analysis.
- The `.wasm` is fetched **lazily only for a 3D preview**. Browsing and the first audio interaction
  never download or instantiate wgpu.

The React wrapper supplies a focusable `<canvas>`, drives the island's lifecycle (`create` on mount
→ `free()` on unmount), and calls `setCameraPose` / `resize` from DOM controls. **The DOM owns the
data and chrome; the island owns pixels** — model preview bytes arrive over
[`@/api/client`](../api/client.ts), and the island never does its own networking.

Controls are input-agnostic: mouse/pen drag or arrow keys orbit; Shift/middle/right drag or
Shift+arrows pan; horizontal one-finger touch orbits while vertical one-finger movement remains
page scrolling; and two-finger drag/pinch pans/zooms. Focused wheel/trackpad and `+`/`-` zoom, Home
resets, and Escape returns wheel scrolling to the page. The canvas exposes these instructions to
assistive technology and has a visible focus ring. Auto-orbit is unavailable under
`prefers-reduced-motion`; all manual controls remain usable. Failed GPU setup shows an honest,
retryable fallback rather than a blank canvas.

## Building the WASM

The generated pkg lands in `web/src/wasm/` (gitignored build artifact). Build it with either:

```
pnpm wasm          # → runs `cargo xtask wasm`
cargo xtask wasm   # wasm-pack build --target web → web/src/wasm/
```

`cargo xtask web` (and `ci`) build it automatically before the Vite bundle, so one `rust-embed`
step ships the React bundle **and** the `.wasm` in the single `3dam` binary (§A.4). Requires the
`wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`) and
`wasm-pack 0.13.1`. Release builds run the checked `wasm-opt -Oz --enable-bulk-memory` profile
from the crate manifest; the explicit feature matches the workspace's Rust 1.91 WASM output.

The Vite build then runs `pnpm bundle:check`. Its manifest-based raw and Brotli artifact budgets
live in [`bundle-budgets.json`](../../bundle-budgets.json); see
[`docs/web-performance.md`](../../../docs/web-performance.md) for measurement and update policy.

## Backend

WebGPU with a **WebGL2 fallback** (ADR 0009 §9); `handle.backend` reports which was chosen
(`"webgpu"` / `"webgl2"`) for logging. The surface format is probed rather than guessed by backend:
the renderer uses 4× MSAA when supported, then 2×, then an explicit 1× fallback. The effective
sample count is available as `handle.antialiasingSamples` and `data-viewer-msaa` for regression
capture.

## Render parity and batching

The browser and headless renderer implement **framing convention v1** (40° vertical FOV,
aspect-aware bounding-sphere fit, one canonical yaw/pitch/margin). A CPU-only parity contract pins
those constants and the studio PBR light/material conventions across both implementations. Opaque
and masked submeshes sharing a material are merged at upload; transparent meshes remain separate
for per-frame depth sorting. Source/actual draws are exposed as `data-viewer-source-draws` and
`data-viewer-batched-draws`.

Visual cases, backend capture steps, the synthetic 126-submesh batching measurement, and the
real-device touch/Tauri checklist live in
[`docs/viewer-validation.md`](../../../docs/viewer-validation.md). Hardware capture is an explicit
final-validation gate, not something a source-only change can honestly claim to have executed.
