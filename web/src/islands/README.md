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

The React wrapper (owned by the web-client work) supplies a `<canvas>`, drives the island's
lifecycle (`create` on mount → `free()` on unmount), and calls `setCamera` / `resize` from DOM
controls. **The DOM owns the data and chrome; the island owns pixels** — model preview bytes arrive
over [`@/api/client`](../api/client.ts), and the island never does its own networking.

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
(`"webgpu"` / `"webgl2"`) for logging.

## Status (not finalized)

These islands are intentionally unfinished pending the parallel web-client work and the shared
`dam-render` crate (tech-spec 06). Known follow-ups: touch/pointer orbit gestures wired from DOM,
MSAA/anti-aliasing, per-material batching, and reconciling the framing/shader with `dam-render` so
the browser viewer and the server thumbnail match "by construction" (ADR 0002).
