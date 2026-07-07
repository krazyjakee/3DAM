# WASM viewer islands (DOM side)

**WASM viewer islands.** The `wgpu` viewer islands live in the Rust crate
[`crates/3dam-viewer`](../../../crates/3dam-viewer) and are compiled to `wasm32-unknown-unknown`
with `wasm-pack`. This folder is the **DOM-side boundary** the React chrome builds on
(tech-spec 09 §B.3, ADR 0009 §9).

## What's here

- [`index.ts`](index.ts) — framework-agnostic loaders and typed handles:
  - `createModelViewer(canvas) → ModelViewerHandle` — interactive 3D (server-decoded `DMSH` preview
    mesh; every Assimp format, textured).
  - `createWaveform(canvas) → WaveformHandle` — audio waveform (a "hot render path").
  - The `.wasm` is fetched **lazily** on first use, so the thumbnail grid pays nothing for it.

The React wrapper (owned by the web-client work) supplies a `<canvas>`, drives the island's
lifecycle (`create` on mount → `free()` on unmount), and calls `setCamera` / `setProgress` /
`resize` from DOM controls. **The DOM owns the data and chrome; the island owns pixels** — fetch
model bytes / waveform samples over [`@/api/client`](../api/client.ts) and hand them in; the island
never does its own networking.

## Building the WASM

The generated pkg lands in `web/src/wasm/` (gitignored build artifact). Build it with either:

```
pnpm wasm          # → runs `cargo xtask wasm`
cargo xtask wasm   # wasm-pack build --target web → web/src/wasm/
```

`cargo xtask web` (and `ci`) build it automatically before the Vite bundle, so one `rust-embed`
step ships the React bundle **and** the `.wasm` in the single `3dam` binary (§A.4). Requires the
`wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`) and `wasm-pack`.

## Backend

WebGPU with a **WebGL2 fallback** (ADR 0009 §9); `handle.backend` reports which was chosen
(`"webgpu"` / `"webgl2"`) for logging.

## Status (not finalized)

These islands are intentionally unfinished pending the parallel web-client work and the shared
`dam-render` crate (tech-spec 06). Known follow-ups: touch/pointer orbit gestures wired from DOM,
MSAA/anti-aliasing, per-material batching, and reconciling the framing/shader with `dam-render` so
the browser viewer and the server thumbnail match "by construction" (ADR 0002).
