# 06 — 3D render (`3dam-render`)

Status: **Draft v0.1** · Scope: the `3dam-render` wgpu crate — one renderer, two entry points (headless render-to-PNG and a GUI-surface viewer), the software-raster fallback, and the deterministic multi-view render that feeds shape embeddings.

This file is the low-level design for the **`3dam-render`** crate: how a caller builds a
scene from pure-math types and gets either an offscreen RGBA8 image or draws into a
GUI-supplied surface, the offscreen render sequence, adapter selection / fallback, and the
versioned camera framing that keeps thumbnails reproducible. It implements two decisions and
does not re-open them:

- [ADR 0001](../adr/0001-3d-render-backend.md) — **wgpu is the sole backend**; headless is
  one path with no per-OS branch and no window/event loop.
- [ADR 0002](../adr/0002-3d-render-crate-boundary.md) — the **`3dam-core` (pure math) /
  `3dam-render` (wgpu, no windowing) / shell** split.

The algorithms it borrows (bounding-sphere auto-fit, Möller–Trumbore + lazy BVH picking,
mesh weld/cleanup + AABB, two-tier GLB read) are mined in
[3d-handler-notes.md](../3d-handler-notes.md).

**Where the borders are** (per [00-overview.md](00-overview.md)):

- [04-media-handlers.md](04-media-handlers.md) owns 3D **metadata extraction and geometry
  decode** — the cheap container scan and the full `gltf`/`fbx`/`obj` load. This file assumes
  a decoded mesh arrives; it does not parse files.
- [05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md) owns the **shape
  embedding**. This file produces the deterministic multi-view images that embedding consumes
  (§7) and guarantees they are stable across re-analysis; it does not run inference.
- [12-desktop-gui.md](12-desktop-gui.md) owns the **GUI shell** — the window, the event loop,
  the toolkit, and the viewer *widget*. This file draws *into* a surface the shell hands over;
  it never creates a window (ADR 0002).
- Pure math (camera, ray-pick, AABB, mesh cleanup) lives in **`3dam-core`**, not here; this
  crate calls it. See §6.

---

## 1. Two entry points, one renderer

The whole crate exists to make the thumbnailer and the viewer render **the same way by
construction** (ADR 0002): same shaders, same draw loop, same camera framing. They differ
only in *where the frame goes* (an offscreen texture vs a GUI surface) and *who drives the
camera* (a deterministic pose vs live orbit input).

```
                         3dam-core  (pure math, no GPU)
        Mesh · SceneGraph · Aabb · Camera math · pick (MT+BVH) · cleanup
                                   │  (owned data + math)
                                   ▼
   ┌──────────────────────────  3dam-render  (wgpu, NO windowing) ──────────────────────────┐
   │   Renderer { device, queue, shaders(PBR), pipelines, depth/msaa, GpuScene cache }      │
   │                         one shared draw loop  ·  one shader set                         │
   └───────────────┬───────────────────────────────────────────────────┬───────────────────┘
                   │                                                     │
        offscreen render target                              GUI-supplied surface
                   │                                                     │
                   ▼                                                     ▼
   ┌───────────────────────────────┐                     ┌───────────────────────────────┐
   │ THUMBNAIL / MULTI-VIEW WORKER │                     │        GUI VIEWER WIDGET       │
   │  (CLI + `3dam serve`)         │                     │  (egui-wgpu / Iced, in 12)     │
   │  render → Texture             │                     │  shell owns window+event loop  │
   │  → copy_texture_to_buffer     │                     │  hands us a TextureView+size   │
   │  → map_async → RGBA8          │                     │  + orbit input; we draw a frame│
   │  → PNG (thumb) / N imgs (→05) │                     │  live pick via 3dam-core (§6)  │
   └───────────────────────────────┘                     └───────────────────────────────┘
        no window · no event loop · no per-OS branch          window & loop belong to 12
```

Both paths call `Renderer::draw(&GpuScene, &Camera, target)`. Framing (§5) is a pure function
of the mesh bounds plus a versioned parameter set, so a thumbnail and the viewer's default
pose *are the same pose*.

---

## 2. Crate shape, dependencies, and version pinning

`3dam-render` is a library crate. It depends on `3dam-core` (types + math), `wgpu`, `bytemuck`
(POD vertex/uniform structs), `pollster` (block on `map_async` in the headless worker), and
`image` (PNG encode). It has **no `winit`, no toolkit, no I/O beyond producing bytes** —
consistent with ADR 0002. `oxipng` (PNG optimisation) is applied by the thumbnail worker in
[04](04-media-handlers.md)/[08](08-convert-pipeline.md), not here; this crate emits a plain
RGBA8 buffer and (optionally) an encoded PNG.

**Version pinning (ADR 0001 risk: wgpu churns).** wgpu's API breaks across minor releases.
The consequence we implement:

- Pin `wgpu` to an exact version in the workspace `Cargo.toml`; bump it deliberately, never via
  a `^` range. `01` (architecture) records the pinned version as the single source of truth.
- **All wgpu types stay behind this crate's public API.** No `wgpu::Device`, `wgpu::Texture`,
  etc. appear in `3dam-core` or in caller signatures except the two deliberate surface-sharing
  seams in §4.2 (which are `#[cfg(feature = "…")]`-gated). A wgpu upgrade therefore touches only
  `3dam-render` internals.
- The `egui-wgpu` / Iced integration lives behind cargo features (§4.2) so the toolkit
  dependency — still undecided (ADR 0001/0002 follow-up) — does not leak into the headless build
  path. The thumbnailer builds with **neither** feature on.

### Optional shape of the crate

```
3dam-render/
  Cargo.toml            # exact-pinned wgpu; features: gui-egui, gui-iced, software-fallback
  src/
    lib.rs              # public API (§3): Renderer, SceneBuilder, GpuScene, RenderTarget…
    device.rs           # adapter/device selection + fallback ladder (§4.1)
    pipeline.rs         # pipelines, bind-group layouts, depth/MSAA, shader module load
    scene.rs            # GpuScene: upload Mesh→vertex/index buffers, materials, cache
    draw.rs             # the shared draw loop (used by BOTH entry points)
    offscreen.rs        # headless: texture target → copy-to-buffer → map → RGBA8/PNG (§4)
    views.rs            # deterministic multi-view camera set for embeddings (§7)
    shaders/pbr.wgsl    # the ONE shader set both paths use
    integ_egui.rs       # #[cfg(feature="gui-egui")] paint-callback glue (§4.2)
    integ_iced.rs       # #[cfg(feature="gui-iced")] custom-shader-widget glue (§4.2)
```

---

## 3. Public API (Rust-ish pseudocode)

Indicative, not frozen ([00](00-overview.md) conventions). Names in **bold** are the seam
that files [04](04-media-handlers.md), [05](05-analysis-similarity-dedup.md), and
[12](12-desktop-gui.md) depend on — see the summary at the foot for the contract.

### 3.1 Inputs — built from `3dam-core` pure-math types

```rust
// Re-exported from 3dam-core; pure data, no GPU. Owned by 04 (decode) / core (math).
pub use dam_core::{Mesh, SceneGraph, Aabb, Transform, Material, Camera, CameraFraming};

/// A CPU-side scene the caller assembles from decoded geometry (from 04).
/// Still pure data — no wgpu here.
pub struct SceneDesc {
    pub root:   SceneGraph,          // nodes, transforms, mesh/material refs
    pub meshes: Vec<Mesh>,           // positions/normals/uv/indices as decoded
    pub materials: Vec<Material>,    // base colour / metallic / roughness (PBR)
    pub bounds: Aabb,                // precomputed by core (feeds framing §5)
}
```

`Mesh`/`Aabb`/etc. are whatever the loader in [04](04-media-handlers.md) yields — 3DAM ingests
arbitrary assets and takes what the loader gives (no fixed interleaved vertex format; see
[3d-handler-notes.md](../3d-handler-notes.md) §"do NOT take"). `3dam-render` uploads that as-is.

### 3.2 The renderer

```rust
pub struct Renderer { /* device, queue, pipelines, depth+msaa, wgsl module, scene cache */ }

pub enum RenderMode { Headless, Gui }  // selects adapter constraints (§4.1); same shaders.

impl Renderer {
    /// One-time GPU init. Headless: request Adapter/Device with NO surface (§4).
    /// Runs the fallback ladder (§4.1); Err only if even software raster is unavailable.
    pub async fn new(mode: RenderMode, opts: RenderOptions) -> Result<Self, RenderError>;

    /// Which adapter/backend we actually got — surfaced so 04/serve can log or degrade.
    pub fn adapter_info(&self) -> AdapterInfo;   // { backend, name, is_software, device_type }

    /// Upload a CPU scene to GPU buffers once; reuse across many frames/views.
    pub fn build_scene(&self, desc: &SceneDesc) -> GpuScene;

    /// THE shared draw call. Both entry points funnel through here → framing & shading match.
    pub fn draw(&self, scene: &GpuScene, camera: &Camera, target: &mut RenderTarget);
}
```

### 3.3 Entry point A — headless / offscreen (thumbnailer + multi-view)

```rust
pub struct ImageSize { pub w: u32, pub h: u32 }
pub struct Rgba8 { pub w: u32, pub h: u32, pub pixels: Vec<u8> } // tightly packed, no padding

impl Renderer {
    /// Deterministic single thumbnail. Framing = auto-fit from scene.bounds (§5) at the
    /// versioned default pose. Blocking read-back (pollster) — no event loop.
    pub fn render_thumbnail(&self, scene: &GpuScene, size: ImageSize,
                            framing: &FramingVersion) -> Result<Rgba8, RenderError>;

    /// Convenience: render_thumbnail → PNG bytes (image crate). oxipng is applied by 04/08.
    pub fn render_thumbnail_png(&self, scene: &GpuScene, size: ImageSize,
                                framing: &FramingVersion) -> Result<Vec<u8>, RenderError>;

    /// Multi-view for embeddings (§7). Deterministic camera set → N images, feeds 05.
    /// Same draw loop, same shaders — the embedding "sees" the same shading as the thumbnail.
    pub fn render_multiview(&self, scene: &GpuScene, size: ImageSize,
                            views: &ViewSet) -> Result<Vec<Rgba8>, RenderError>;
}
```

### 3.4 Entry point B — GUI viewer (draws into a shell surface)

Toolkit-agnostic core; the actual `egui-wgpu` / Iced glue is feature-gated (§4.2). The shell
([12](12-desktop-gui.md)) owns the window, event loop, and surface; it hands us a
`TextureView` + size each frame and the current orbit `Camera`.

```rust
impl Renderer {
    /// Draw one viewer frame into a GUI-provided target. No window created here.
    pub fn draw_viewer(&self, scene: &GpuScene, camera: &Camera, target: GuiTarget);
}

/// Toolkit-neutral handle the shell fills in (from egui-wgpu paint callback / Iced widget).
pub struct GuiTarget<'a> {
    pub view: &'a wgpu::TextureView,   // the ONLY deliberate wgpu leak (feature-gated seam)
    pub size: ImageSize,
    pub format: wgpu::TextureFormat,   // shell's surface format; we adapt the pipeline
}
```

Picking is **not** a render call — it is pure math in `3dam-core` (§6). The widget converts a
click to a world ray and asks core; `3dam-render` is uninvolved.

---

## 4. The headless render-to-PNG path

This is the load-bearing path (PRODUCT_SPEC §6.8): it runs from the CLI and from
`3dam serve` on a box that may have **no display server**. Per ADR 0001 it is **one code
path with no per-OS branch and no window/event loop**.

### 4.1 Adapter / device selection and the software-raster fallback

`Renderer::new(Headless, …)` walks a **fallback ladder** and stops at the first rung that
yields a device. There is no windowing and no surface at any rung.

```rust
async fn acquire_device(opts: &RenderOptions) -> Result<(Device, Queue, AdapterInfo), RenderError> {
    let instance = wgpu::Instance::new(/* all backends: Vulkan | Metal | DX12 | GL */);

    // Rung 1 — best real GPU, no surface (compatible_surface: None).
    if let Some(a) = instance.request_adapter(&RequestAdapterOptions {
        power_preference: HighPerformance, compatible_surface: None,
        force_fallback_adapter: false,
    }).await { return finish(a); }

    // Rung 2 — wgpu's fallback adapter (software path where the backend exposes one, e.g.
    // DX12 WARP; on Vulkan this is where lavapipe is picked up if present as an ICD).
    if let Some(a) = instance.request_adapter(&RequestAdapterOptions {
        force_fallback_adapter: true, compatible_surface: None, ..
    }).await { return finish(a); }

    // Rung 3 — explicit software rasteriser: a Vulkan instance that enumerates lavapipe,
    // or a GL instance backed by Mesa llvmpipe. Selected by env/config on the serve host
    // (e.g. VK_ICD_FILENAMES → lavapipe, or LIBGL_ALWAYS_SOFTWARE=1 for llvmpipe).
    if let Some(a) = software_only_adapter(&instance, opts).await { return finish(a); }

    // Rung 4 — nothing renders here. Return a typed error; the CALLER degrades (§4.3).
    Err(RenderError::NoAdapter)
}
```

`adapter_info().is_software` is set on rungs 2–3 so [04](04-media-handlers.md)/serve can log
"software raster" and, if desired, down-tier quality (smaller sample count, lower MSAA) for
throughput. The chosen backend is recorded once at startup, not re-probed per asset.

> **Gate.** This ladder is *designed* here but its viability on a real GPU-less headless
> Linux box is the open **spike** from ADR 0001 (follow-up) / PRODUCT_SPEC §10 — confirming
> lavapipe/llvmpipe actually produce a frame with our pipeline. Until that spike passes, the
> `software-fallback` behaviour is provisional; see [Open questions](#open-questions).

### 4.2 The offscreen render sequence (no surface, no loop)

Once a device is in hand, one straight-line sequence produces RGBA8 — identical on Linux,
Windows, macOS (ADR 0001):

```rust
fn render_offscreen(r: &Renderer, scene: &GpuScene, cam: &Camera, size: ImageSize) -> Rgba8 {
    // 1. Offscreen colour target — a Texture, NOT a surface.
    let color = r.device.create_texture(&TextureDescriptor {
        size: size.into(), format: Rgba8UnormSrgb,
        usage: RENDER_ATTACHMENT | COPY_SRC, sample_count: r.msaa, ..
    });
    let depth = r.device.create_texture(/* Depth32Float, RENDER_ATTACHMENT, size */);
    // (if msaa > 1: a resolve target Texture at sample_count 1 that COPY_SRC reads from)

    // 2. Encode the SAME draw loop the viewer uses (draw.rs) into this target.
    let mut enc = r.device.create_command_encoder(..);
    r.draw_into(&mut enc, scene, cam, &color_view, &depth_view /*, resolve_view*/);

    // 3. Copy the (resolved) colour texture into a mappable buffer.
    //    IMPORTANT: bytes_per_row must be padded to COPY_BYTES_PER_ROW_ALIGNMENT (256).
    let padded_bpr = align_256(size.w * 4);
    let buf = r.device.create_buffer(&BufferDescriptor {
        size: padded_bpr * size.h, usage: COPY_DST | MAP_READ, .. });
    enc.copy_texture_to_buffer(color_or_resolve.as_image_copy(),
        ImageCopyBuffer { buffer: &buf, layout: TexelCopyBufferLayout {
            bytes_per_row: Some(padded_bpr), rows_per_image: Some(size.h), .. } },
        size.into());
    r.queue.submit([enc.finish()]);

    // 4. Map, block (pollster — NO event loop), copy out, DROP the row padding.
    let slice = buf.slice(..);
    slice.map_async(MapMode::Read, |res| { /* signal */ });
    r.device.poll(Maintain::Wait);          // synchronous drain; headless, so we just wait
    pollster::block_on(/* the map future */);
    let rows = slice.get_mapped_range();
    let pixels = unpad_rows(&rows, size.w * 4, padded_bpr, size.h); // tight RGBA8
    Rgba8 { w: size.w, h: size.h, pixels }
}
```

PNG encode (`image`) is a thin wrapper on step 4's output in `render_thumbnail_png`. Two
padding pitfalls are handled explicitly and are the only fiddly parts: the 256-byte
`bytes_per_row` alignment on copy, and stripping that padding on read-back.

**No `winit`, no `EventLoop`, no `Surface`, no `#[cfg(target_os)]`** appears anywhere in this
sequence — that is the whole point of ADR 0001 versus MoGen's `headless.rs` /
`headless_egl.rs` / hidden-window trichotomy.

### 4.3 Graceful degradation when nothing can render

If `Renderer::new` returns `RenderError::NoAdapter` (ladder exhausted — rung 4), rendering is
**not** an error the caller crashes on (fail-soft, PRODUCT_SPEC §8). Degradation is the
caller's policy, not this crate's — this crate only reports the capability. Per §6.8 the serve
host then:

- **Serves metadata + geometry stats** (from [04](04-media-handlers.md)'s cheap container
  scan — which never touches the GPU) and any previews rendered elsewhere.
- **Defers or skips on-server renders** — marks the thumbnail derivative "pending, no
  renderer" rather than failing the asset. A GPU-capable client (desktop GUI, §6.8
  render-on-demand) or a re-run on a GPU host can fill it later.
- Never blocks browse/search on the missing thumbnail.

So `3dam-render` exposes the *capability* (`adapter_info`, `NoAdapter`); the degradation
*policy* lives in the thumbnail worker / serve host per §6.8. The choice between
lavapipe/llvmpipe, a CPU thumbnail path, and render-on-demand-by-a-GPU-client is the open
question below.

---

## 5. Camera framing — versioned, reproducible auto-fit

Framing math is pure and lives in **`3dam-core`** (ADR 0002); `3dam-render` calls it and never
re-implements it. It is mined from MoGen ([3d-handler-notes.md](../3d-handler-notes.md) §2):

```rust
// in 3dam-core (pure glam):
pub struct CameraFraming { pub yaw: f32, pub pitch: f32, pub fov_deg: f32, pub fit_mul: f32 }

fn auto_fit(bounds: &Aabb, f: &CameraFraming, zoom: f32) -> Camera {
    let center = bounds.center();
    let radius = bounds.bounding_sphere_radius().max(1e-3);
    let fit_distance = radius * f.fit_mul;        // MoGen: radius * 2.8 at 45° FOV
    let dist = fit_distance * zoom;               // user zoom is a SEPARATE multiplier
    let eye = center + dist * dir_from(f.yaw, f.pitch);
    Camera::look_at(eye, center, f.fov_deg)
}
```

Two things make thumbnails **reproducible and versioned** (PRODUCT_SPEC §8, the reason 05 can
trust them):

- **`fit_distance` is derived from bounds; user `zoom` is separate.** The default framing is a
  deterministic function of the mesh — re-render the same asset → byte-similar framing. The
  interactive viewer applies live `zoom`/orbit on top *without changing* the stored default.
- **A `FramingVersion`.** The defaults (`fit_mul = 2.8`, `yaw = π/4`, `pitch ≈ 0.5`,
  `fov = 45°`, plus background/light rig and image size) are captured in a small versioned
  struct. The thumbnail derivative records which `FramingVersion` produced it; the
  extractor-versioning scheme in [05](05-analysis-similarity-dedup.md) bumps it when framing
  changes, so re-analysis is deterministic and stale thumbnails are detectable. **Same
  `FramingVersion` in → same pixels out** (modulo GPU driver rounding), which is what makes the
  multi-view render (§7) safe to embed against.

---

## 6. Ray-picking — pure math in core, not here

Picking is **`3dam-core`** math (ADR 0002; [3d-handler-notes.md](../3d-handler-notes.md) §3),
`glam`-only, unit-testable with no GPU or window. `3dam-render` contributes nothing but the
camera/viewport used to build the ray. Listed here only so the boundary is explicit:

```rust
// in 3dam-core:
fn intersect_tri(orig, dir, v0, v1, v2) -> Option<f32>;   // Möller–Trumbore, EPS = 1e-6
struct Bvh { /* flat arena, median-split, leaves ≤4 tris, ray-AABB slab prune */ }
impl Bvh { fn build_lazy(mesh: &Mesh) -> Self; fn raycast(&self, ray: &Ray) -> Option<Hit>; }
// entry: screen coords → NDC → unproject near/far (viewer Camera) → world ray → bvh.raycast
```

The GUI widget ([12](12-desktop-gui.md)) does screen→NDC→world-ray using the same `Camera`
`3dam-render` drew with, then calls `Bvh::raycast`. The BVH is built lazily on first pick and
cached alongside the `GpuScene`. A filtered variant (predicate over `NodeId`) supports picking
only certain node kinds if the GUI ever highlights sub-meshes/materials.

---

## 7. Multi-view render for embeddings (feeds 05)

[05](05-analysis-similarity-dedup.md) computes the **per-view shape embedding**; this crate
gives it a **deterministic set of rendered views**. Because it is the *same* renderer, shaders,
and framing math as the thumbnail (§1), the images the embedding sees match what the user sees
— no drift between "what it looks like" and "what it's indexed as".

```rust
pub struct ViewSet { pub framing: FramingVersion, pub poses: Vec<CameraFraming> }

/// Canonical set: a fixed ring of yaw angles at a gentle downward pitch (+ optional top),
/// all at the versioned fit distance. Deterministic → reproducible embeddings.
pub fn canonical_views(v: FramingVersion) -> ViewSet;   // in views.rs
```

Determinism requirements this crate honours for [05](05-analysis-similarity-dedup.md):

- **Fixed, versioned camera set.** The pose list is a pure function of `FramingVersion`.
  Bump the version → re-analysis produces a comparable set; leave it → identical poses. The
  embedding's extractor version (05) is composed with this `FramingVersion` so a framing change
  invalidates cached embeddings correctly.
- **Bounds-relative, not asset-relative-in-pixels.** Every pose auto-fits from the mesh's
  bounding sphere (§5), so a tiny prop and a huge terrain fill the frame the same way — the
  embedding compares *shape*, not scale.
- **Same shading as the thumbnail.** One shader set (`pbr.wgsl`), one light rig, one
  background. No embedding-only rendering mode that could diverge from the visible thumbnail.
- **Software-raster-safe.** The multi-view path is just N offscreen renders (§4.2), so it runs
  on the fallback adapter on a GPU-less serve host, subject to the same §4.3 degradation.

`render_multiview` returns `Vec<Rgba8>` in pose order; [05](05-analysis-similarity-dedup.md)
feeds those to the shape model. This crate does **no** inference and stores **no** embeddings.

---

## 8. Surface sharing with the GUI (toolkit-agnostic, feature-gated)

The GUI shell ([12](12-desktop-gui.md)) owns the window, event loop, and wgpu surface; the
toolkit is still undecided (ADR 0001/0002 follow-up). `3dam-render` therefore **never creates a
window** and integrates behind cargo features so the headless build carries neither toolkit:

- **egui-wgpu** (`feature = "gui-egui"`): the shell registers an egui paint callback
  (`egui_wgpu::CallbackTrait` / paint callback). Inside it we receive the render pass /
  target and call `draw_viewer`. This replaces MoGen's `egui_glow` custom paint (which we do
  **not** take — [3d-handler-notes.md](../3d-handler-notes.md) §"do NOT take").
- **Iced** (`feature = "gui-iced"`): a custom **shader widget** (`iced_wgpu` primitive) whose
  draw hook forwards its target to `draw_viewer`.

Both funnel into the same `draw_viewer` → same `draw.rs` → same shaders as the thumbnailer,
so the live viewer and the stored thumbnail frame and shade identically (ADR 0002). The
`GuiTarget.format` seam lets us adapt the pipeline to whatever surface format the shell chose
(the viewer surface may be `Bgra8` while offscreen is `Rgba8`). **Flagged dependency:** the
concrete integration hardens once the toolkit ADR lands; the seam (`draw_viewer` +
`GuiTarget`) is stable regardless of which side wins.

---

## 9. The shared shader set and draw loop

One WGSL module (`shaders/pbr.wgsl`) and one `draw.rs` are used by **both** entry points —
this is the ADR 0002 "match by construction" mechanism made concrete:

- **PBR-lite shading**: base colour + metallic/roughness from `Material`, a fixed key/fill
  light rig, neutral background. Simple and stable — the point is *consistent* framing/shading
  across thumbnail and viewer, not photoreal.
- **One draw loop**: bind camera uniforms → per-node model matrix → bind material → draw
  indexed. `draw.rs::draw_into(encoder, scene, camera, color_view, depth_view)` is the single
  body; `render_offscreen` (§4.2) and `draw_viewer` (§8) both call it, differing only in the
  target views they pass and (offscreen) the copy-to-buffer that follows.
- **MSAA / depth** are configured identically for both paths (down-tiered on software
  adapters, §4.1) so silhouettes match between preview and thumbnail.

Because there is exactly one draw body and one shader, a change to shading changes both
surfaces together — divergence is structurally impossible, not merely discouraged.

---

## Open questions

- **Headless render on a GPU-less server** *(carried from PRODUCT_SPEC §10 / ADR 0001
  follow-up — the gate on the whole render backend decision)*. §4.1's fallback ladder and
  §4.3's degradation policy are *designed* but unproven on the target environment. The spike:
  **render-to-PNG on a headless Linux box with no display and no discrete GPU** — does the
  Vulkan **lavapipe** ICD (rung 3) or `force_fallback_adapter` / Mesa **llvmpipe** actually
  produce a correct frame through `pbr.wgsl`, at acceptable speed, or must we choose a CPU
  thumbnail path or **render-on-demand by a GPU-capable client** instead? Until this passes,
  the `software-fallback` feature is provisional. This decides whether §6.8 serve-mode
  thumbnails work by construction or must degrade to metadata-only.
- **wgpu version cadence.** The exact pin (§2) lives in [01](01-architecture-and-crates.md);
  open: how often we bump, and whether a CI job asserts no wgpu type escapes `3dam-render`'s
  public API (the ADR 0002 dependency-direction guard, applied to render→core rather than only
  core→render).
- **GUI toolkit → surface-sharing mechanism.** The `gui-egui` vs `gui-iced` glue (§8) is
  feature-gated but only one hardens once the toolkit ADR lands (ADR 0001/0002 follow-up).
  Open: whether `GuiTarget` needs to widen for the web client's WASM/wgpu viewer island
  ([09](09-server-and-web-client.md)), which is a third surface consumer of this same crate.

---

See also: [00-overview.md](00-overview.md) · [04-media-handlers.md](04-media-handlers.md) ·
[05-analysis-similarity-dedup.md](05-analysis-similarity-dedup.md) ·
[12-desktop-gui.md](12-desktop-gui.md) · [ADR 0001](../adr/0001-3d-render-backend.md) ·
[ADR 0002](../adr/0002-3d-render-crate-boundary.md) ·
[3d-handler-notes.md](../3d-handler-notes.md) · [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.4, §6.8
