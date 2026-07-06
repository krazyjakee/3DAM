# Spike — headless wgpu render-to-PNG + software-raster fallback

Status: **Passed** · Date: 2026-07-06 · Resolves the gate in
[ADR 0001](../../docs/adr/0001-3d-render-backend.md) follow-up and the open question in
[tech-spec 06 §4](../../docs/tech-spec/06-3d-render.md) / PRODUCT_SPEC §10.

## Question

ADR 0001 picked **wgpu** as the sole 3D backend on the promise that headless thumbnailing is
**one code path, no per-OS branch** — request an adapter with *no surface*, render to an
offscreen texture, read it back, encode a PNG. The named risk: on a **GPU-less server** wgpu
still needs an adapter, so it must fall back to a **software rasteriser** (Mesa
lavapipe / llvmpipe). *"Spike this early on a headless Linux box — it is the one place wgpu can
surprise us."* This spike is that test.

## What it does

[`src/main.rs`](src/main.rs) — ~260 lines, `wgpu = "30"`. Requests an adapter with
`compatible_surface: None`, creates a device, draws one vertex-coloured triangle over a cleared
background into an `Rgba8UnormSrgb` offscreen texture, does `copy_texture_to_buffer` (with the
256-byte `COPY_BYTES_PER_ROW_ALIGNMENT` row padding), `map_async` + `poll(Wait)`, un-pads the
rows, and writes a PNG. It prints the adapter actually chosen (name, backend, device type,
`software` = `device_type == Cpu`).

## Environment

Dev box: Rust 1.95, NVIDIA RTX 3090 (Vulkan 1.3), **and** Mesa lavapipe installed
(`/usr/share/vulkan/icd.d/lvp_icd.json`). Having both lets us render on the real GPU *and*
force the software path on the same machine.

## Results

| Run | Invocation | Adapter chosen | Backend | Device | software | PNG |
|-----|-----------|----------------|---------|--------|----------|-----|
| Real GPU | *(default)* | NVIDIA GeForce RTX 3090 | Vulkan | DiscreteGpu | false | ✅ 512² |
| Fallback ladder rung 2 | `--fallback` (`force_fallback_adapter: true`) | llvmpipe (LLVM 20.1) | Vulkan | **Cpu** | **true** | ✅ 512² |
| GPU-less sim (rung 3) | `VK_ICD_FILENAMES=…/lvp_icd.json WGPU_BACKEND=vulkan` | llvmpipe (LLVM 20.1) | Vulkan | **Cpu** | **true** | ✅ 512² |

- The two software runs land on **lavapipe** — Mesa's Vulkan software rasteriser reports its
  device name as `llvmpipe (LLVM …)` but is served through the **Vulkan** backend. This is the
  exact path a headless Linux server would use.
- **GPU and software outputs are visually identical** (see `out-gpu.png` vs `out-lavapipe.png`);
  only ~32 bytes of PNG differ, from sRGB rounding between the two rasterisers.
- Cold end-to-end (adapter → device → render → readback → PNG): ~130–230 ms, software vs GPU
  comparable at this trivial scale (dominated by init, not raster).

## Verdict

**The gate passes.** wgpu renders headless to a PNG with no surface and no per-OS branch, and
lavapipe produces a correct frame with our pipeline when no GPU is used. ADR 0001's
software-raster fallback is viable; the `3dam-render` fallback ladder
([tech-spec 06 §4.1](../../docs/tech-spec/06-3d-render.md)) can be built as specified.

## Caveats / follow-ups (not yet covered)

- **Real GPU-less hardware.** We simulated "no GPU" by forcing lavapipe on a box that *has* a
  GPU. A true GPU-less server (only lavapipe present) should be simpler, but confirm on real
  headless hardware / a container before calling it done-done.
- **Env caveat for the ladder.** `InstanceDescriptor::new_without_display_handle()` does **not**
  read `WGPU_BACKEND`; `.with_env()` is required for env-driven backend pinning. In production,
  `3dam-render` should select backends/adapters **explicitly** (the ladder in 06 §4.1), not rely
  on env — the ladder should try HighPerformance, then `force_fallback_adapter`, then an explicit
  software instance.
- **Not yet tested:** depth buffer + MSAA, a real glTF mesh (not just a triangle), the
  multi-view render set for embeddings, actual thumbnail throughput/latency at batch scale, and
  behaviour when the ladder is fully exhausted (rung 4 → `NoAdapter` → caller degradation).

## Run it

```sh
cargo run                       # real GPU
cargo run -- out.png --fallback # force software (lavapipe)
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json WGPU_BACKEND=vulkan cargo run -- lvp.png
```
