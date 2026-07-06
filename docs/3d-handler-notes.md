# 3D handler — reusable algorithms & references (from MoGen)

Status: **Notes** · Date: 2026-07-06 · Scope: implementation reference for the 3D media
handler. Companion to [ADR 0001](adr/0001-3d-render-backend.md) (wgpu) and
[ADR 0002](adr/0002-3d-render-crate-boundary.md) (crate boundary).

**MoGen** (`../godot-projects/mogen`) is a production Rust 3D generator. To *generate* it had
to solve every hard problem 3DAM's 3D handler needs on the *read* side. These notes capture
the **backend-agnostic algorithms** worth reusing (pure `glam`/CPU math — not its
glow/OpenGL code, which we replace with wgpu per ADR 0001), with source paths for reference.

All paths below are relative to `../godot-projects/mogen/`.

## Feature mapping

| 3DAM need (spec §) | MoGen equivalent | Source |
|---|---|---|
| §5 3D attributes (counts, bbox, rig/anim/UV) | `mogen inspect` | `crates/mogen/src/commands/inspect.rs`, `crates/mogen/src/format.rs` |
| §6.2/§6.4 thumbnail / multi-view render | `mogen thumbnail` | `crates/mogen-render/src/headless.rs`, `camera.rs` |
| §6.4 3D orbit viewer | Studio viewer | `crates/mogen-studio/src/viewer.rs`, `viewer/camera.rs`, `pick.rs` |
| §6.5 mesh optimisation / stats | `mogen-geom` cleanup + AABB | `crates/mogen-geom/src/cleanup.rs`, `crates/mogen-core/src/aabb.rs` |

## 1. Two-tier GLB read

MoGen's `inspect` does **not** fully parse a GLB to summarise it. It hand-walks the GLB
container: 12-byte header (magic `glTF` = `0x46546C67`, version, length), then chunks —
JSON chunk (`0x4E4F534A`) parsed as `serde_json::Value`, BIN chunk (`0x004E4942`) skipped.
Counts (nodes / meshes / materials / accessors / skins), per-node mesh+skin refs, and
material base-colour come straight out of the JSON — **no geometry upload**.

**For 3DAM:** this is the ingest-time metadata scan for §5. Two tiers:

- **Ingest / catalog:** cheap JSON-chunk scan → vertex/tri estimate, mesh/material/texture
  counts, `has rig/anim`, UV presence, bbox. Cheap enough to run on a million assets.
- **Preview only:** full decode via the `gltf` crate (features `import, names, utils`) when
  the user actually opens the asset — that's when you pay for vertex buffers.

Note: exact triangle count needs accessor `count` fields (present in the JSON chunk), so
even precise geometry stats don't require decoding the BIN buffer.

## 2. Stateless camera auto-fit (reproducible framing)

From `crates/mogen-render/src/camera.rs` + the thumbnail path:

```
center = mesh.center
radius = mesh.radius.max(0.001)          // bounding sphere
fit_distance = radius * 2.8              // frames model in a 45° FOV
eye = target + dist * (spherical from yaw/pitch)   // dist = fit_distance * zoom
```

Key idea: **`fit_distance` is derived from bounds; user `zoom` is a separate multiplier.**
So the default framing is a deterministic function of the mesh — critical for §6.2 thumbnails
being **reproducible and versioned** (re-render the same asset → byte-similar framing).
Defaults: `yaw = π/4`, `pitch ≈ 0.5 rad` (gentle downward gaze).

## 3. Click-picking: Möller–Trumbore + lazy BVH

`crates/mogen-studio/src/pick.rs` — pure math, no GL, port directly into `3dam-core`:

- **Ray–triangle:** Möller–Trumbore (`intersect_tri`, ~20 lines, `EPS = 1e-6`).
- **Acceleration:** median-split **BVH** built lazily from the flattened mesh; flat arena,
  interior nodes reference children by index, leaves hold ≤4 contiguous triangles.
- **Ray–AABB slab test** before descending — one reciprocal per ray, skips whole subtrees
  farther than the current best hit.
- **Entry points** convert egui screen coords → NDC → unproject near/far → world-space ray →
  walk BVH → nearest `NodeId`. Supports a filtered variant (predicate) for picking only
  certain node kinds — useful if 3DAM ever highlights sub-meshes/materials.

## 4. Mesh cleanup + AABB

`crates/mogen-geom/src/cleanup.rs` — the order matters:

1. **`weld_vertices(mesh, eps)`** — merge within ε using **hash-grid bucketing at scale
   `1/eps`** (O(n), not O(n²)); renormalise averaged normals.
2. **`recompute_normals`** — face-normal averaging over adjacent tris, *after* welding.
3. **`cull_degenerate`** — drop tris with near-zero area (cross-product < `1e-10`), cheap
   insurance.

Relevant to §6.5 (mesh optimisation) and to reporting **honest** tri/vertex counts after
normalisation. (MoGen's per-face-UV and CSG-specific passes are generation concerns — skip.)

**AABB** — `crates/mogen-core/src/aabb.rs`: `Aabb { min, max }`, `from_mesh`, `center()`, and
`transformed(Mat4)` via **transform-8-corners-then-re-axis-align** (correct under rotation).
Feeds camera framing (§2), the geometry-stats column, and later similarity/normalisation.

## 5. Dependency versions MoGen runs in production

Confirms 3DAM's §7 candidate stack is buildable in pure Rust today:

| Crate | Version | Use |
|---|---|---|
| `glam` | 0.30 | Vec3/Mat4/Quat math (core) |
| `gltf` | 1 (`import, names, utils`) | full glTF/GLB decode for preview |
| `image` | 0.25 | PNG/JPEG encode (thumbnails) |
| `meshopt` | 0.6 | mesh LOD / simplification (§6.5) |
| `oxipng` | 9 | PNG optimisation |
| `fbxcel` | 0.9 | FBX read (§6.5 FBX↔glTF) |
| `manifold-csg` | 0.1.8 | boolean ops — *generation only, likely N/A for 3DAM* |

`gltf` + `fbxcel` + `meshopt` together confirm the §6.5 convert/optimise pipeline
(FBX↔glTF + simplification) is achievable without leaving Rust.

## What we deliberately do NOT take from MoGen

- **glow / glutin / winit / `khronos-egl` headless stack** — replaced by wgpu (ADR 0001);
  MoGen's per-OS EGL-vs-hidden-window split is exactly what wgpu lets us delete.
- **`egui_glow` custom-paint viewer wiring** — GL-specific; our viewer sits on wgpu.
- **Interleaved 20-float vertex format** (pos/normal/uv/joints/weights/color) — MoGen authors
  its meshes and is skinning-first; 3DAM ingests arbitrary assets and takes whatever the
  loader yields.
- **All DSL / LLM / CSG generation** — out of scope for a manager.
