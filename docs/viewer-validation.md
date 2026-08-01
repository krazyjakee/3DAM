# 3D viewer parity, performance, and device validation

Issue #104's source changes are testable without a GPU, but its WebGPU/WebGL2 pixels and touch
ergonomics must be checked on real renderers and hardware. This document is the reproducible final
gate. Do not replace unavailable hardware with an unqualified “verified” claim.

## Fixed fixtures and observable diagnostics

The versioned case list is
`crates/3dam-render/tests/fixtures/viewer-visual-cases.json`:

- `textured_cube.gltf`: bounds framing, UV orientation, saturated base colour, silhouette AA;
- `glass_cube.gltf`: alpha and ordered transparent pass;
- `multi_material_grid.obj`: four material factors reused over twelve visible parts.

The viewer root publishes `data-viewer-framing`, `data-viewer-backend`, `data-viewer-msaa`,
`data-viewer-source-draws`, and `data-viewer-batched-draws`. Record these with every capture; a
“WebGL2” screenshot that silently ran WebGPU is not a fallback fixture.

## Final visual-regression capture

1. Run the repository's final validation/build gate, start a render-enabled server, and ingest the
   three fixtures. Use a square 512 CSS-pixel inspector and DPR 2.
2. Capture the untouched default viewer and its server thumbnail for each case. Do not orbit first.
   Confirm framing convention v1 has the same centre, pose, margin, material colours, transparency,
   and texture orientation. Small raster/SSAA differences are acceptable; camera or material
   differences are not.
3. Capture Chromium with WebGPU enabled. Record backend/sample attributes and the browser/GPU.
4. Force the browser's WebGL2 path by disabling WebGPU, reload from a clean tab, and repeat. Record
   the attributes; require an anti-aliased multisample count where the adapter reports it, otherwise
   record the honest `1` fallback and inspect silhouettes at 200%.
5. Store approved PNGs under the validation artifact named
   `viewer-parity-<browser>-<backend>-<fixture>.png`. Compare later captures at equal viewport/DPR;
   reject changes in framing, UV orientation, material identity, blend ordering, or gross edge
   aliasing. Driver-level subpixel differences are reviewed rather than hidden by an over-broad
   pixel threshold.

## Batching measurement

The CPU fixture in `crates/3dam-viewer/src/batching.rs` models 120 opaque submeshes cycling four
materials plus six transparent panes. The deterministic plan is 126 source draws to 10 actual
draws (four opaque material batches plus six sortable transparent draws), a 92.1% reduction.

For the real multi-material fixture, record the two draw attributes before profiling. In Chromium
Performance, capture 300 manual-orbit frames after warm-up and record median frame time and GPU
task time. Compare a temporary local no-batching build only during validation; do not commit that
variant. Batching passes when output is unchanged, actual draws never exceed source draws, and a
fixture with reusable opaque materials shows fewer draws without regressing median frame time.

## Interaction matrix

Run every row with the page tall enough to scroll:

| Target | Required checks |
|---|---|
| Desktop mouse/trackpad | Left drag orbits; Shift/middle/right drag pans; unfocused wheel scrolls the page; click then wheel zooms; Escape releases it; context menu works away from the focused viewer. |
| Keyboard/screen reader | Tab reaches canvas with a visible ring and reads the instructions; arrows orbit, Shift+arrows pan, `+`/`-` zoom, Home resets; Tab continues to controls; failure state is announced and retryable. |
| iOS Safari phone | One-finger vertical motion scrolls the page; horizontal motion orbits; two-finger drag/pinch pans/zooms where Pointer Events permit; controls meet 44px coarse-pointer targets. |
| Android Chrome phone/tablet | Same touch checks, including pointer cancellation when the browser takes vertical scrolling. |
| Tauri touch hardware | Repeat touch, keyboard, fullscreen, resize, suspend/resume, and selection/unmount checks in the shipped shell. |

Enable `prefers-reduced-motion: reduce` for one pass: auto-orbit must be disabled, manual controls
must remain functional, and no control transition should animate. Finally disable WebGPU and
WebGL2 (or use an unsupported environment) and confirm the honest retryable fallback leaves the
thumbnail and metadata usable.
