# ADR 0011 — Assimp as the 3D import backend for thumbnails

Status: **Accepted** · Date: 2026-07-07 · Deciders: 3DAM core
Supersedes: — · Related: [ADR 0001](0001-3d-render-backend.md), [ADR 0002](0002-3d-render-crate-boundary.md), [tech-spec 06](../tech-spec/06-render-and-viewer.md)

## Context

3DAM targets professional game studios, whose libraries span the full 3D interchange range — **FBX**
above all (the dominant DCC/engine interchange), plus OBJ/MTL, Collada (DAE), 3DS, glTF/GLB, PLY,
STL, and `.blend`. A grid preview that only decoded glTF, or that rendered untextured grey clay,
does not meet that bar: studios expect **materially-accurate, textured** thumbnails across the
formats they actually ship.

The v1 renderer decoded only glTF/GLB (via the pure-Rust `gltf` crate) and shaded a flat base
colour. Extending that per-format in pure Rust is not realistic: FBX-with-materials has no mature
pure-Rust decoder, and Collada/3DS/LWO/etc. crates are absent or unmaintained. The one tool that
covers this range in a single, battle-tested code path — with geometry **and** materials/textures —
is **Assimp** (Open Asset Import Library), the de-facto standard importer used across the industry.

## Decision

Use **Assimp** (via the maintained [`russimp-ng`](https://crates.io/crates/russimp-ng) bindings) as
the single model-import backend in `dam-render`. Assimp is **built from source and statically
linked** (`russimp-ng/static-link`), so the shipped `3dam` binary carries no runtime `libassimp`
dependency. The importer produces world-space submeshes (node transforms baked via
`PreTransformVertices`, which also normalises FBX axis/unit conventions) grouped by material, plus
metallic-roughness PBR materials (base-colour / metallic-roughness / normal / emissive — factors and
textures, whether embedded or resolved from sibling files). `dam-render` then renders them with a
textured Cook-Torrance PBR pipeline (fixed studio light rig + hemispheric ambient, tangent-space
normal mapping, mipmapped textures).

This is a deliberate, scoped departure from the "pure-Rust where possible" preference (CLAUDE.md):
the breadth and material fidelity Assimp delivers for the target users outweigh the cost of one
vendored C++ dependency. The dependency is confined to `dam-render`, which is already the crate that
owns "not-pure-logic" concerns (GPU), and is gated behind the `render` feature so pure-library
consumers of `dam-core` remain free of it.

## Consequences

- **Formats covered:** FBX, OBJ/MTL, DAE, 3DS, glTF/GLB, PLY, STL, `.blend`, X, LWO, and more — one
  code path, with materials and textures. `supports_format()` in `dam-render` is the source of truth.
- **The interactive web viewer shares this decode.** `dam-render::model_preview_blob()` serialises the
  same decoded model (geometry + PBR materials + downscaled textures) into a compact, self-contained
  `DMSH` blob (`preview` module), served at `/api/v1/assets/{id}/preview-mesh` and uploaded straight by
  the WASM island (`dam-viewer`). One CPU-only decode now feeds *both* the turntable thumbnail and the
  browser's textured PBR orbit view, so every format above previews interactively **with textures** —
  and the old browser-side glTF-only decoder (with its fragile external-buffer resolution, the source
  of the "missing or unreadable buffers" error) is retired. The blob path needs no GPU, so interactive
  previews work even where headless PNG rendering can't.
- **Draco compression is decoded.** `KHR_draco_mesh_compression` glTF/GLB — what Unreal, Blender,
  `gltfpack`, and most modern asset packs export — needs Assimp's bundled Draco decoder, which is
  **off by default**. Assimp's glTF2 importer otherwise hard-errors (*"GLTF: Draco mesh compression
  not supported."*) and the model falls through to the typed tile. We force it on: a CMake toolchain
  file (`crates/3dam-render/assimp-draco.cmake`) sets `ASSIMP_BUILD_DRACO_STATIC=ON`, wired into the
  `russimp-sys-ng` cmake build via `CMAKE_TOOLCHAIN_FILE` in the workspace `.cargo/config.toml`
  (the only reproducible hook — that sys crate hardcodes its cmake defines and builds *before* any
  `dam-*` script). `dam-render/build.rs` then links the resulting `libdraco.a` (russimp-sys-ng links
  only assimp + zlib). This is the sole reason the repo carries a `.cargo/config.toml`.
- **The one gap — USD.** Assimp has no USD importer, so `usd`/`usda`/`usdc`/`usdz` are **not**
  rendered; they fail soft to the honest typed tile (they are still catalogued with cheap metadata).
  A dedicated USD path (a future USD crate, or Pixar tooling shelled out on hosts that have it) is a
  follow-up. This is the deliberate, documented boundary of "all formats".
- **`.blend` — the file's own embedded preview first; no full re-render.** Assimp's Blender importer
  only understands *legacy* Blender DNA (≤ 2.7x); a 2.8+/3.x/4.x `.blend` — essentially every file
  authored today — fails to decode outright, and a full render of a large `.blend` is unreasonable
  anyway. But an artist-saved `.blend` almost always **embeds Blender's own preview image** (the
  `TEST` file-block that OS file managers show). The thumbnail path uses *that* directly:
  `blend::embedded_thumbnail_png` walks the file-block stream (bounded, so file size is irrelevant),
  extracts the RGBA preview, and returns it as PNG — no Blender process, no GPU, milliseconds. When a
  `.blend` carries no embedded preview it falls through to the honest typed tile. Because `.blend`
  yields no decodable geometry, the web Inspector shows this preview image rather than the interactive
  3D island (there is no orbit view for `.blend`, by design).
- **`.blend` — an optional headless Blender bridge (fallback).** For the rarer case of wanting real
  geometry from a modern `.blend` (no embedded preview, or the interactive path), `dam-render` retains
  a **headless Blender bridge** (`blend::convert_to_glb`): when the in-process Assimp decode fails and
  a `blender` binary is resolvable (`DAM_BLENDER_BIN`, else `blender` on `PATH`), it runs Blender
  `--background --factory-startup --disable-autoexec` to export the scene to a temporary GLB, which the
  normal Assimp path then decodes. **Off by default and fail-soft** (a host with no Blender never
  spawns it; 90 s timeout; `--disable-autoexec` blocks a hostile file's scripts). Not exercised by the
  default `.blend` thumbnail (the embedded preview wins first); provisioning the binary is the switch.
- **Build cost:** a first build compiles Assimp from source (cmake + C++ toolchain required —
  documented as a prerequisite alongside `wasm-pack`/`pnpm`). Subsequent builds are cached. Binary
  size grows by the static Assimp.
- **Trust boundary:** Assimp parses untrusted asset files in-process. Imports run inside the engine's
  `spawn_blocking` thumbnail path and fail soft per-asset (ADR 0001 §fail-soft), so a malformed file
  degrades to a typed tile rather than aborting a scan. Sandboxing the importer is a hardening
  follow-up worth tracking for a network-exposed `serve`.
