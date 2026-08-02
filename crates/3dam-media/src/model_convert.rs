//! 3D container transcode — any Assimp-readable model to GLB, glTF, or OBJ (issue #49,
//! tech-spec 08 §3.3).
//!
//! ## Why this is nearly free
//!
//! Assimp ships exporters as well as importers, and the static library `dam-render` already builds
//! for thumbnails has them compiled in (`ASSIMP_NO_EXPORT` defaults off, and nothing in this tree
//! turns it on). `russimp-ng` re-exports the generated bindings, so the whole path is raw FFI over
//! a library that is already linked — no new native dependency, no second cmake build.
//!
//! ## Multi-file targets
//!
//! Assimp returns a blob chain: GLB is one part, textual glTF adds `.bin`, and OBJ adds `.mtl`.
//! [`convert_bundle`] retains that structure and rebases Assimp's generic companion names to the
//! requested output stem. The core pipeline stages and publishes the complete family together;
//! writing only the primary would create a convincing but broken handoff.
//!
//! ## The texture trap
//!
//! `aiProcess_EmbedTextures` has to be passed at **import** time. Passing it as the *exporter's*
//! preprocessing argument — which is what the name suggests — silently produces a GLB whose images
//! are `uri` references to files sitting next to the *original*, i.e. a broken asset the moment it
//! leaves the output directory. Import-time embedding is what makes the output self-contained.
//!
//! ## What this is, and is not
//!
//! It is "give me a portable open-format copy of this model" — for preview, web delivery, or
//! handoff. GLB is self-contained; glTF and OBJ carry their companions as an explicit bundle. The
//! output is **indexed**, which is not automatic here: see the `aiCopyScene` comment in `convert`
//! for the flag interaction that otherwise triples the vertex count.
//! It is **not** lossless interchange. Assimp's exporters re-interpret: a one-material textured
//! cube comes back with two materials, because a default is appended. Custom properties and
//! non-PBR material extensions are lost. That is acceptable only because the pipeline is
//! non-destructive by construction — the original is never touched (tech-spec 08 §5.1) — so a
//! lossy convert is always an addition, never a replacement.
//!
//! ## Optimisation and compression
//!
//! `optimize` adds [`OPTIMISE_FLAGS`] to the import: redundant materials go, meshes and nodes are
//! merged, degenerate faces are deleted, and the vertices a merge duplicates are re-joined. That is
//! *topological* optimisation — fewer draw calls and fewer vertices for the same picture. For the
//! glTF family, the result is then encoded as required `KHR_draco_mesh_compression` by the
//! document-preserving `draco-gltf` encoder, with ordinary geometry removed. Assimp's exporter is
//! decode-only for Draco; keeping this as a separate post-export stage makes that limitation clear.
//! OBJ has no equivalent standard compression extension, so its optimise path is topology-only.

use std::collections::HashSet;
use std::ffi::CString;
use std::path::Path;

use crate::{HandlerError, ModelCompanion, ModelOutput};
use base64::Engine as _;

/// Assimp post-processing applied at import.
///
/// Close to what `dam-render` uses for thumbnails, with two deliberate differences:
/// - **`EmbedTextures` is added**, so the exported GLB carries its images (see the module docs).
/// - **`PreTransformVertices` is *not* used**, though `dam-render` does. Assimp's own
///   documentation for it reads "Removes the node graph and pre-transforms all vertices…
///   *Animations are removed during this step*". That is the right trade for a thumbnail, which
///   only needs pixels, and the wrong one for a handoff artefact: glTF represents hierarchy and
///   animation natively, so baking them away would discard content the target format can hold.
///
/// Values come from Assimp's `postprocess.h`; `russimp-ng` exposes them as `u32` constants, but they
/// are spelled out here so the set is readable without chasing the binding.
const IMPORT_FLAGS: u32 = AI_PROCESS_TRIANGULATE
    | AI_PROCESS_JOIN_IDENTICAL_VERTICES
    | AI_PROCESS_GEN_SMOOTH_NORMALS
    | AI_PROCESS_EMBED_TEXTURES;

/// The curated optimisation set, added to [`IMPORT_FLAGS`] when a request opts in.
///
/// All of it is *import*-time work, and that is not a stylistic choice: Assimp's post-process chain
/// runs `OptimizeGraph` → `OptimizeMeshes` → … → `JoinIdenticalVertices` in that fixed order
/// (`PostStepRegistry.cpp`), so the join that recovers the vertices a merge duplicated only happens
/// if the merge ran first, in the same pass. Handing these to the exporter instead would be too
/// late — and the exporter's own preprocessing is subtracted against what the importer already did,
/// which is the trap documented on `aiCopyScene` below.
///
/// Why each one:
/// - **`RemoveRedundantMaterials`** — identical materials collapse to one, which is what lets
///   `OptimizeMeshes` merge the meshes that referenced them. On its own it is nearly free.
/// - **`OptimizeGraph`** — collapses nodes that carry nothing (no animation, bone, light or
///   camera). This is the step that makes optimisation structurally lossy: node names and hierarchy
///   are how some downstream tools address parts of a model, so it is opt-in rather than default.
///   Assimp explicitly preserves animated/bone/light/camera nodes, so animation survives.
/// - **`OptimizeMeshes`** — merges meshes sharing a material into one, i.e. fewer draw calls, which
///   is the headline win for a web/preview handoff.
/// - **`FindDegenerates` + `SortByPType`** — delete zero-area triangles rather than render them.
///   Both need configuration to behave; see [`optimise_props`].
/// - **`ImproveCacheLocality`** — reorders triangles for vertex-cache hit rate. Pure win, invisible
///   in the file's shape, measurable on the GPU.
const OPTIMISE_FLAGS: u32 = AI_PROCESS_REMOVE_REDUNDANT_MATERIALS
    | AI_PROCESS_OPTIMIZE_GRAPH
    | AI_PROCESS_OPTIMIZE_MESHES
    | AI_PROCESS_FIND_DEGENERATES
    | AI_PROCESS_SORT_BY_PTYPE
    | AI_PROCESS_IMPROVE_CACHE_LOCALITY;

const AI_PROCESS_JOIN_IDENTICAL_VERTICES: u32 = 0x2;
const AI_PROCESS_TRIANGULATE: u32 = 0x8;
const AI_PROCESS_GEN_SMOOTH_NORMALS: u32 = 0x40;
const AI_PROCESS_IMPROVE_CACHE_LOCALITY: u32 = 0x800;
const AI_PROCESS_REMOVE_REDUNDANT_MATERIALS: u32 = 0x1000;
const AI_PROCESS_SORT_BY_PTYPE: u32 = 0x8000;
const AI_PROCESS_FIND_DEGENERATES: u32 = 0x1_0000;
const AI_PROCESS_OPTIMIZE_MESHES: u32 = 0x20_0000;
const AI_PROCESS_OPTIMIZE_GRAPH: u32 = 0x40_0000;
const AI_PROCESS_EMBED_TEXTURES: u32 = 0x1000_0000;

/// `aiPrimitiveType_POINT | aiPrimitiveType_LINE` — what `SortByPType` is told to throw away.
const AI_PRIMITIVE_TYPE_POINT_AND_LINE: i32 = 0x1 | 0x2;

/// Target formats this module accepts, mapped to Assimp's exporter ids.
///
/// Assimp exporter ids used by the single- and multi-file encode seams.
fn exporter_id(target: &str) -> Option<&'static str> {
    match target {
        "glb" => Some("glb2"),
        "gltf" => Some("gltf2"),
        "obj" => Some("obj"),
        _ => None,
    }
}

/// Platform-independent filename validation. `Path::components()` on Unix deliberately treats a
/// backslash as ordinary text, but a bundle created there may later be unpacked on Windows.
fn is_safe_file_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && !value.chars().any(char::is_control)
}

/// Is `target` a 3D container this build can write?
pub fn supports_target(target: &str) -> bool {
    exporter_id(target).is_some()
}

/// Owns the imported `aiScene` so every exit path releases it, including the error ones.
struct Scene(*const russimp_ng::sys::aiScene);

impl Drop for Scene {
    fn drop(&mut self) {
        // SAFETY: the pointer came from one of the `aiImportFile*` entry points (both release the
        // same way) and is released exactly once, here.
        unsafe { russimp_ng::sys::aiReleaseImport(self.0) }
    }
}

/// Owns a scene produced by `aiCopyScene`, which is freed with `aiFreeScene` rather than
/// `aiReleaseImport` — different allocator bookkeeping, and mixing them is a double-free.
struct SceneCopy(*mut russimp_ng::sys::aiScene);

impl Drop for SceneCopy {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `aiCopyScene` and is freed exactly once, here.
        unsafe { russimp_ng::sys::aiFreeScene(self.0) }
    }
}

/// Owns an import property store so it outlives the import call and is freed on every exit path.
struct Props(*mut russimp_ng::sys::aiPropertyStore);

impl Drop for Props {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `aiCreatePropertyStore` and is released exactly once, here.
        unsafe { russimp_ng::sys::aiReleasePropertyStore(self.0) }
    }
}

/// Configure the two optimisation steps whose *defaults* are wrong for an export target.
///
/// `FindDegenerates` does not remove a degenerate triangle by default — it rewrites it as a line or
/// a point, and `SortByPType` then splits those into primitives of their own. Left alone, the naive
/// flag set would therefore trade a handful of invisible triangles for **extra draw calls**, which
/// is the opposite of the request. `PP_FD_REMOVE` deletes them instead, and `PP_SBP_REMOVE` drops
/// any point/line primitive that reaches the sort anyway (some importers produce them directly).
///
/// The keys are spelled out rather than taken from a binding because `config.h` is not in
/// `russimp-sys-ng`'s `wrapper.h`, so its `AI_CONFIG_*` string macros are not generated.
fn optimise_props() -> Result<Props, HandlerError> {
    // SAFETY: allocates a store; null means the allocation failed and is handled below.
    let store = unsafe { russimp_ng::sys::aiCreatePropertyStore() };
    if store.is_null() {
        return Err(HandlerError::Encode(
            "Assimp could not allocate an import property store".into(),
        ));
    }
    let props = Props(store);
    for (key, value) in [
        ("PP_FD_REMOVE", 1),
        ("PP_SBP_REMOVE", AI_PRIMITIVE_TYPE_POINT_AND_LINE),
    ] {
        let key_c = CString::new(key).expect("config keys are NUL-free literals");
        // SAFETY: `props.0` is a live store and `key_c` outlives the call, which copies the name.
        unsafe { russimp_ng::sys::aiSetImportPropertyInteger(props.0, key_c.as_ptr(), value) };
    }
    Ok(props)
}

/// Owns an export blob chain so it is released even if we bail while copying it out.
struct Blob(*const russimp_ng::sys::aiExportDataBlob);

impl Drop for Blob {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `aiExportSceneToBlob` and is released exactly once, here.
        unsafe { russimp_ng::sys::aiReleaseExportBlob(self.0) }
    }
}

/// Transcode `path` to `target_format`, returning the encoded bytes.
///
/// `optimize` opts into the mesh optimisation described in the module docs — off means a plain
/// container transcode.
///
/// EXPENSIVE tier — the convert pipeline only, never at ingest.
pub fn convert(path: &Path, target_format: &str, optimize: bool) -> Result<Vec<u8>, HandlerError> {
    let output = convert_bundle(path, target_format, optimize, "model")?;
    if !output.companions.is_empty() {
        return Err(HandlerError::Unsupported(format!(
            "'{target_format}' is a multi-file model target; use the bundle encode seam"
        )));
    }
    Ok(output.primary)
}

/// Transcode to a complete output family. `output_stem` must be a file stem, not a path.
pub fn convert_bundle(
    path: &Path,
    target_format: &str,
    optimize: bool,
    output_stem: &str,
) -> Result<ModelOutput, HandlerError> {
    if !is_safe_file_component(output_stem) {
        return Err(HandlerError::Unsupported(
            "model output stem must be one safe path component".into(),
        ));
    }
    let id = exporter_id(target_format).ok_or_else(|| {
        HandlerError::Unsupported(format!(
            "3D target '{target_format}' is not supported (this build writes: glb, gltf, obj; \
             FBX and USD encode are post-v1)"
        ))
    })?;

    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| HandlerError::Unsupported("path contains a NUL byte".into()))?;
    // `id` is one of this module's own string literals, so this cannot fail.
    let id_c = CString::new(id).expect("exporter ids are NUL-free literals");

    // The optimisation steps need a configured property store (see `optimise_props`), which the
    // plain `aiImportFile` shorthand has nowhere to take — hence the two import calls rather than
    // one with a conditional flag word. The store must stay alive across the import.
    let flags = if optimize {
        IMPORT_FLAGS | OPTIMISE_FLAGS
    } else {
        IMPORT_FLAGS
    };
    let props = optimize.then(optimise_props).transpose()?;

    // SAFETY: `path_c` (and `props`) outlive the call; Assimp copies what it needs. A null return
    // means the import failed, and the reason is deliberately not fetched (see below). The null
    // `aiFileIO` asks for Assimp's own default filesystem, which is what `aiImportFile` uses too.
    let scene = match &props {
        Some(p) => unsafe {
            russimp_ng::sys::aiImportFileExWithProperties(
                path_c.as_ptr(),
                flags,
                std::ptr::null_mut(),
                p.0,
            )
        },
        None => unsafe { russimp_ng::sys::aiImportFile(path_c.as_ptr(), flags) },
    };
    if scene.is_null() {
        // Deliberately no Assimp error detail. `aiGetErrorString()` reads `gLastErrorString`, a
        // plain `static std::string` that the C API assigns with **no lock** — and this crate is
        // not the only caller (`dam-render` imports too, and convert runs on `spawn_blocking`), so
        // reading it while another thread reassigns it is a use-after-free, not merely a stale
        // message. Recovering the detail safely needs either a process-wide lock over every Assimp
        // entry point (including `dam-render`'s) or the C++ `Importer` API, which keeps its error
        // per-instance. Neither belongs in this slice; a wrong-but-safe message beats a race.
        return Err(HandlerError::Corrupt(
            "Assimp could not read this file as a 3D model".into(),
        ));
    }
    let scene = Scene(scene);

    // Assimp's importers are lenient: the OBJ reader skips lines it does not recognise, so a text
    // file of prose "imports" cleanly as a scene with a single named node and no geometry, and then
    // exports to a structurally valid GLB containing nothing. A convert that reports success and
    // writes an empty model is worse than one that refuses, so an empty scene is a failure here.
    // SAFETY: `scene.0` is non-null and live for the duration of this borrow.
    let mesh_count = unsafe { (*scene.0).mNumMeshes };
    if mesh_count == 0 {
        return Err(HandlerError::Corrupt(
            "the file contains no geometry to convert".into(),
        ));
    }

    // Export a *copy*, not the imported scene — this is worth 3.8x on output size and is entirely
    // non-obvious.
    //
    // `Exporter::Export` computes `pp = (enforced | requested) & ~already_applied_by_the_importer`,
    // and skips that subtraction when the scene is flagged as a copy. `glb2` enforces
    // `JoinIdenticalVertices`, which our import flags also request — so on the *imported* scene the
    // join is subtracted out as "already done". But the exporter then runs `MakeVerboseFormat`
    // unconditionally, which de-indexes the mesh to three vertices per triangle, and only re-joins
    // when the enforced set *lacks* `JoinIdenticalVertices`. The result is a fully de-indexed GLB:
    // a 65k-triangle sphere exported at 196,608 positions / 7.1 MB instead of 33,153 / 1.8 MB.
    //
    // `aiCopyScene` sets the copy flag, so nothing is subtracted, the join runs, and the output is
    // indexed. Fixing it from the other end — dropping `JoinIdenticalVertices` at import — works
    // too, but the import-side join is what mesh simplification will need, so the copy is the
    // version that leaves both doors open.
    let mut copy_ptr: *mut russimp_ng::sys::aiScene = std::ptr::null_mut();
    // SAFETY: `scene.0` is a live imported scene; `aiCopyScene` writes a new scene pointer out.
    unsafe { russimp_ng::sys::aiCopyScene(scene.0, &mut copy_ptr) };
    if copy_ptr.is_null() {
        return Err(HandlerError::Encode(
            "Assimp could not copy the scene for export".into(),
        ));
    }
    let copy = SceneCopy(copy_ptr);

    // SAFETY: `copy.0` is a live scene; `id_c` outlives the call. The `0` is the exporter's
    // preprocessing flags — deliberately empty, because the processing that matters (texture
    // embedding) has to happen at *import*, not here.
    let blob = unsafe { russimp_ng::sys::aiExportSceneToBlob(copy.0, id_c.as_ptr(), 0) };
    if blob.is_null() {
        // No detail here for a second reason on top of the race: `aiExportSceneToBlob` builds a
        // local `Exporter` and discards its error string, so `aiGetErrorString()` would return the
        // last *import* error — or nothing at all in a fresh process. A message that confidently
        // reports the wrong cause is worse than one that admits it has none.
        return Err(HandlerError::Encode(format!(
            "Assimp's '{id}' exporter rejected this scene"
        )));
    }
    let blob = Blob(blob);

    // SAFETY: a non-null blob has a valid `data`/`size`, and `next` chains further parts.
    let mut parts = Vec::new();
    // SAFETY: the blob guard keeps the entire linked chain live. Null data at zero size is handled
    // explicitly because `from_raw_parts(null, 0)` is undefined behaviour.
    unsafe {
        let mut current = blob.0;
        while !current.is_null() {
            let part = &*current;
            let bytes = if part.data.is_null() || part.size == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(part.data as *const u8, part.size).to_vec()
            };
            let name_len = (part.name.length as usize).min(part.name.data.len());
            let name = String::from_utf8_lossy(std::slice::from_raw_parts(
                part.name.data.as_ptr() as *const u8,
                name_len,
            ))
            .into_owned();
            parts.push((name, bytes));
            current = part.next;
        }
    }
    let Some((_, primary)) = parts.first().cloned() else {
        return Err(HandlerError::Encode(
            "Assimp produced no output blobs".into(),
        ));
    };
    if primary.is_empty() {
        return Err(HandlerError::Encode("Assimp produced an empty file".into()));
    }
    let mut companions = Vec::new();
    let mut renames = Vec::new();
    let mut planned_names = HashSet::new();
    for (index, (original, bytes)) in parts.into_iter().skip(1).enumerate() {
        if bytes.is_empty() {
            return Err(HandlerError::Encode(format!(
                "Assimp produced an empty companion blob {original:?}"
            )));
        }
        if !is_safe_file_component(&original) {
            return Err(HandlerError::Encode(format!(
                "Assimp produced an unsafe companion name {original:?}"
            )));
        }
        let original_path = Path::new(&original);
        let original_name = original_path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                HandlerError::Encode(format!(
                    "Assimp produced an unsafe companion name {original:?}"
                ))
            })?;
        // Assimp's in-memory exporter uses a bare `bin`/`mtl` blob name while writing
        // `$blobfile.bin`/`$blobfile.mtl` into the primary. Other exporters use the complete name.
        let bare_extension = matches!(original.as_str(), "bin" | "mtl");
        let extension = original_path
            .extension()
            .and_then(|value| value.to_str())
            .or_else(|| bare_extension.then_some(original.as_str()))
            .unwrap_or("part");
        let name = if index == 0 || matches!(extension, "bin" | "mtl") {
            format!("{output_stem}.{extension}")
        } else {
            format!("{output_stem}-{original_name}")
        };
        if !planned_names.insert(name.clone()) {
            return Err(HandlerError::Encode(format!(
                "Assimp companion names collide after rebasing at {name:?}"
            )));
        }
        let reference = if bare_extension {
            format!("$blobfile.{original}")
        } else {
            original
        };
        renames.push((reference, name.clone()));
        companions.push(ModelCompanion { name, bytes });
    }

    let mut output = ModelOutput {
        primary,
        companions,
    };
    for (from, to) in renames {
        replace_text_reference(&mut output.primary, &from, &to);
        for companion in &mut output.companions {
            if matches!(
                Path::new(&companion.name)
                    .extension()
                    .and_then(|value| value.to_str()),
                Some("gltf" | "obj" | "mtl")
            ) {
                replace_text_reference(&mut companion.bytes, &from, &to);
            }
        }
    }
    if optimize && matches!(target_format, "glb" | "gltf") {
        compress_draco(output, target_format, output_stem)
    } else {
        Ok(output)
    }
}

fn replace_text_reference(bytes: &mut Vec<u8>, from: &str, to: &str) {
    if from.is_empty() || from == to {
        return;
    }
    let mut replaced = Vec::with_capacity(bytes.len());
    let mut rest = bytes.as_slice();
    while let Some(offset) = rest
        .windows(from.len())
        .position(|window| window == from.as_bytes())
    {
        replaced.extend_from_slice(&rest[..offset]);
        replaced.extend_from_slice(to.as_bytes());
        rest = &rest[offset + from.len()..];
    }
    replaced.extend_from_slice(rest);
    *bytes = replaced;
}

/// Apply real geometry compression after Assimp has produced standards-compliant glTF. The
/// document-preserving encoder removes ordinary primitive geometry in `DracoOnly` mode, marks
/// `KHR_draco_mesh_compression` required, and validates the rewritten graph before serialization.
fn compress_draco(
    output: ModelOutput,
    target_format: &str,
    output_stem: &str,
) -> Result<ModelOutput, HandlerError> {
    let mut import = if target_format == "glb" {
        draco_gltf::import_slice(&output.primary, None).map_err(|error| {
            HandlerError::Encode(format!("read exported GLB for Draco: {error}"))
        })?
    } else {
        let mut json: serde_json::Value = serde_json::from_slice(&output.primary)
            .map_err(|error| HandlerError::Encode(format!("read exported glTF JSON: {error}")))?;
        let buffers = json
            .get_mut("buffers")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| HandlerError::Encode("exported glTF has no buffers".into()))?;
        for buffer in buffers {
            let uri = buffer
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| HandlerError::Encode("exported glTF buffer has no URI".into()))?;
            let bytes = output
                .companions
                .iter()
                .find(|part| part.name == uri)
                .map(|part| part.bytes.as_slice())
                .ok_or_else(|| {
                    HandlerError::Encode(format!(
                        "exported glTF references missing companion {uri:?}"
                    ))
                })?;
            let embedded = format!(
                "data:application/octet-stream;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
            buffer["uri"] = serde_json::Value::String(embedded);
        }
        let embedded = serde_json::to_vec(&json)
            .map_err(|error| HandlerError::Encode(format!("embed glTF buffers: {error}")))?;
        draco_gltf::import_slice(&embedded, None).map_err(|error| {
            HandlerError::Encode(format!("read exported glTF for Draco: {error}"))
        })?
    };

    let work: Vec<_> = import
        .document
        .meshes()
        .into_iter()
        .map(|mesh| {
            let count = mesh
                .value()
                .get("primitives")
                .and_then(draco_gltf::JsonValue::as_array)
                .map_or(0, |primitives| primitives.len());
            (mesh.index(), count)
        })
        .collect();
    let mut compressed = 0usize;
    for (mesh, primitive_count) in work {
        for primitive in 0..primitive_count {
            import
                .compress_primitive(mesh, primitive, draco_gltf::CompressionOptions::default())
                .map_err(|error| {
                    HandlerError::Encode(format!("Draco-compress primitive: {error}"))
                })?;
            compressed += 1;
        }
    }
    if compressed == 0 {
        return Err(HandlerError::Encode(
            "Draco compression found no mesh primitives".into(),
        ));
    }

    if target_format == "glb" {
        let primary = import
            .to_bytes(draco_gltf::OutputFormat::GlbV2)
            .map_err(|error| HandlerError::Encode(format!("write Draco GLB: {error}")))?;
        return Ok(ModelOutput {
            primary,
            companions: Vec::new(),
        });
    }

    let portable = import
        .to_gltf_output()
        .map_err(|error| HandlerError::Encode(format!("write Draco glTF: {error}")))?;
    let mut primary = portable.json;
    let mut companions = Vec::with_capacity(portable.resources.len());
    let mut names = HashSet::new();
    for (index, resource) in portable.resources.into_iter().enumerate() {
        let extension = Path::new(&resource.uri)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("bin");
        let name = if index == 0 {
            format!("{output_stem}.{extension}")
        } else {
            format!("{output_stem}-{index}.{extension}")
        };
        if !names.insert(name.clone()) {
            return Err(HandlerError::Encode(format!(
                "Draco glTF companion names collide at {name:?}"
            )));
        }
        replace_text_reference(&mut primary, &resource.uri, &name);
        companions.push(ModelCompanion {
            name,
            bytes: resource.bytes,
        });
    }
    Ok(ModelOutput {
        primary,
        companions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but genuinely valid Wavefront OBJ: one triangle. Text, so the fixture is readable
    /// and needs no committed binary — and Assimp imports it happily.
    const TRIANGLE_OBJ: &str = "v 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 0.0 1.0 0.0\nf 1 2 3\n";

    fn write(name: &str, body: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        (dir, p)
    }

    #[test]
    fn a_model_transcodes_to_a_valid_glb() {
        let (_d, p) = write("tri.obj", TRIANGLE_OBJ.as_bytes());
        let bytes = convert(&p, "glb", false).expect("OBJ → GLB must convert");

        // The GLB container header: magic `glTF`, version 2, then the total length. Checking all
        // three rather than just the magic means a truncated or misreported blob fails here.
        assert!(bytes.len() > 20, "suspiciously small output");
        assert_eq!(&bytes[0..4], b"glTF", "not a GLB container");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize,
            bytes.len(),
            "the header's declared length must match the blob we were handed"
        );
    }

    /// Round-trip: the output has to be readable *as a model*, not merely well-formed as a
    /// container. Re-importing through Assimp is the strongest check available here without
    /// pulling in a second glTF parser.
    #[test]
    fn the_exported_glb_can_be_read_back_as_a_model() {
        let (_d, p) = write("tri.obj", TRIANGLE_OBJ.as_bytes());
        let bytes = convert(&p, "glb", false).unwrap();

        let (_d2, out) = write("out.glb", &bytes);
        let out_c = CString::new(out.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: same import/release discipline as `convert`; released before the assertions so a
        // failure cannot leak the scene.
        let (meshes, ok) = unsafe {
            let s = russimp_ng::sys::aiImportFile(out_c.as_ptr(), AI_PROCESS_TRIANGULATE);
            if s.is_null() {
                (0, false)
            } else {
                let n = (*s).mNumMeshes;
                russimp_ng::sys::aiReleaseImport(s);
                (n, true)
            }
        };
        assert!(ok, "the exported GLB did not re-import");
        assert!(meshes > 0, "the exported GLB has no meshes");
    }

    /// A 4x4 vertex grid — 16 vertices, 18 triangles, every interior vertex shared by several.
    /// Chosen so indexed (16 positions) and de-indexed (54) are unmistakably different.
    fn grid_obj() -> String {
        let mut o = String::new();
        for y in 0..4 {
            for x in 0..4 {
                o.push_str(&format!("v {x}.0 {y}.0 0.0\n"));
            }
        }
        for y in 0..3 {
            for x in 0..3 {
                let (a, b, c, d) = (
                    y * 4 + x + 1,
                    y * 4 + x + 2,
                    (y + 1) * 4 + x + 2,
                    (y + 1) * 4 + x + 1,
                );
                o.push_str(&format!("f {a} {b} {c}\n"));
                o.push_str(&format!("f {a} {c} {d}\n"));
            }
        }
        o
    }

    fn large_grid_obj(side: usize) -> String {
        let mut obj = String::new();
        for y in 0..=side {
            for x in 0..=side {
                obj.push_str(&format!("v {x} {y} 0\n"));
            }
        }
        let row = side + 1;
        for y in 0..side {
            for x in 0..side {
                let a = y * row + x + 1;
                let b = a + 1;
                let c = a + row;
                let d = c + 1;
                obj.push_str(&format!("f {a} {b} {d}\nf {a} {d} {c}\n"));
            }
        }
        obj
    }

    /// Parse a GLB's JSON chunk — the only part of the container these assertions read.
    fn glb_json(glb: &[u8]) -> serde_json::Value {
        let mut off = 12usize;
        let mut doc = None;
        while off + 8 <= glb.len() {
            let clen = u32::from_le_bytes(glb[off..off + 4].try_into().unwrap()) as usize;
            let ctype = &glb[off + 4..off + 8];
            if ctype == b"JSON" {
                doc =
                    serde_json::from_slice::<serde_json::Value>(&glb[off + 8..off + 8 + clen]).ok();
                break;
            }
            off += 8 + clen;
        }
        doc.expect("GLB has a JSON chunk")
    }

    /// Pull `(position_count, triangle_count)` out of a GLB's first mesh primitive.
    fn glb_counts(glb: &[u8]) -> (u64, u64) {
        let doc = glb_json(glb);
        let prim = &doc["meshes"][0]["primitives"][0];
        let acc = |i: &serde_json::Value| {
            doc["accessors"][i.as_u64().unwrap() as usize]["count"]
                .as_u64()
                .unwrap()
        };
        let pos = acc(&prim["attributes"]["POSITION"]);
        let tris = acc(&prim["indices"]) / 3;
        (pos, tris)
    }

    /// `(total positions, total triangles, primitive count)` across every mesh in the GLB.
    ///
    /// The primitive count is the interesting one: in glTF a primitive is one draw call, so it is
    /// the direct measure of what `OptimizeMeshes` is for.
    fn glb_totals(glb: &[u8]) -> (u64, u64, usize) {
        let doc = glb_json(glb);
        let acc = |i: &serde_json::Value| {
            doc["accessors"][i.as_u64().unwrap() as usize]["count"]
                .as_u64()
                .unwrap()
        };
        let (mut pos, mut tris, mut prims) = (0, 0, 0);
        for mesh in doc["meshes"].as_array().expect("GLB has meshes") {
            for prim in mesh["primitives"].as_array().unwrap() {
                prims += 1;
                pos += acc(&prim["attributes"]["POSITION"]);
                tris += acc(&prim["indices"]) / 3;
            }
        }
        (pos, tris, prims)
    }

    /// The output must be **indexed**, and this is not a nicety — it is a 3.5x difference in file
    /// size, measured.
    ///
    /// Assimp's exporter computes its post-processing as
    /// `(enforced | requested) & ~already_applied_by_the_importer`, and skips that subtraction only
    /// when the scene is a *copy*. Because `glb2` enforces `JoinIdenticalVertices` and our import
    /// flags also request it, exporting the imported scene directly subtracts the join away — while
    /// `MakeVerboseFormat` still de-indexes to three vertices per triangle, with nothing re-joining
    /// them. A 16k-triangle sphere came out at 1,377,636 bytes with 49,152 positions instead of
    /// 391,564 bytes with 8,066.
    ///
    /// This assertion catches both an accidental revert of the `aiCopyScene` hop and an Assimp
    /// upgrade that changes the `mIsCopy` behaviour it relies on — which is undocumented internals,
    /// so it is exactly the kind of thing that needs a guard rather than a comment.
    #[test]
    fn the_exported_glb_is_indexed_not_expanded_to_three_verts_per_triangle() {
        let (_d, p) = write("grid.obj", grid_obj().as_bytes());
        let bytes = convert(&p, "glb", false).expect("grid must convert");
        let (pos, tris) = glb_counts(&bytes);
        assert_eq!(tris, 18, "the fixture is 18 triangles");
        assert!(
            pos < tris * 3,
            "the GLB is de-indexed: {pos} positions for {tris} triangles (3x means every triangle \
             got its own copy of every vertex — see this test's docs)"
        );
        // Tighter: the grid's 16 shared vertices should survive as roughly that.
        assert!(pos <= 20, "expected ~16 shared vertices, got {pos}");
    }

    /// Two quads that meet along an edge, split into two OBJ groups so Assimp imports them as two
    /// meshes with the same (default) material.
    ///
    /// Shaped for exactly what optimisation should do to it: the groups can merge because their
    /// material is identical, and vertices 2 and 3 exist twice — once per mesh — until the merge
    /// puts them in the same buffer for `JoinIdenticalVertices` to collapse. So a plain transcode
    /// gives 2 primitives / 8 positions and an optimised one gives 1 / 6, from the same 4 triangles.
    fn two_group_obj() -> &'static str {
        "v 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 1.0 1.0 0.0\nv 0.0 1.0 0.0\n\
         v 2.0 0.0 0.0\nv 2.0 1.0 0.0\n\
         g left\nf 1 2 3\nf 1 3 4\n\
         g right\nf 2 5 6\nf 2 6 3\n"
    }

    /// Mesh optimisation as an encode option (issue #49): opting in must cost draw calls and
    /// vertices, not geometry.
    ///
    /// Both halves matter. Fewer primitives is `OptimizeMeshes` doing its job — a primitive is a
    /// draw call, and merging them is the headline win for a web/preview handoff. Fewer positions
    /// is the subtler one: `JoinIdenticalVertices` already runs on *both* paths, so a smaller
    /// vertex count can only come from the merge having happened first, which is only true because
    /// these are import-time steps ordered by Assimp's post-process chain. Wire them to the
    /// exporter instead and the primitive count still drops while this assertion goes red.
    ///
    /// The triangle count is asserted unchanged on purpose: "optimised" here means topology, not
    /// simplification. Anything that starts deleting faces is a different feature and should have
    /// to change this test to land.
    #[test]
    fn optimising_merges_draw_calls_and_the_vertices_they_shared() {
        let (_d, p) = write("two-groups.obj", two_group_obj().as_bytes());

        let plain = convert(&p, "glb", false).expect("plain transcode");
        let optimised = convert(&p, "glb", true).expect("optimised transcode");

        // Still a GLB, and still one whose header agrees with its own length — an optimisation that
        // produces a subtly malformed container would otherwise pass the count assertions below.
        assert_eq!(&optimised[0..4], b"glTF");
        assert_eq!(u32::from_le_bytes(optimised[4..8].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(optimised[8..12].try_into().unwrap()) as usize,
            optimised.len()
        );

        let (plain_pos, plain_tris, plain_prims) = glb_totals(&plain);
        let (opt_pos, opt_tris, opt_prims) = glb_totals(&optimised);

        assert_eq!(
            (plain_tris, opt_tris),
            (4, 4),
            "the fixture is 4 triangles and optimisation must not remove geometry"
        );
        assert!(
            opt_prims < plain_prims,
            "optimising did not merge draw calls: {plain_prims} → {opt_prims} primitives"
        );
        assert_eq!(
            opt_prims, 1,
            "the two groups share a material, so they merge"
        );
        assert!(
            opt_pos < plain_pos,
            "optimising did not re-join the vertices the two meshes shared: {plain_pos} → \
             {opt_pos} positions"
        );
        assert_eq!(
            opt_pos, 6,
            "6 distinct positions in the fixture; {plain_pos} unoptimised because the shared edge \
             is duplicated per mesh"
        );
    }

    /// The optimised output has to be readable *as a model*, not merely well-formed — the same bar
    /// the plain path is held to, because a merged scene is where a bad node/mesh index would show.
    #[test]
    fn an_optimised_glb_can_be_read_back_as_a_model() {
        let (_d, p) = write("two-groups.obj", two_group_obj().as_bytes());
        let bytes = convert(&p, "glb", true).unwrap();

        assert!(bytes
            .windows("KHR_draco_mesh_compression".len())
            .any(|window| window == b"KHR_draco_mesh_compression"));
        let draco = draco_gltf::import_slice(&bytes, None).unwrap();
        let decoded: Vec<_> = draco
            .draco_primitives()
            .map(|primitive| draco.decode_draco_primitive(primitive).unwrap())
            .collect();
        assert_eq!(decoded.len(), 1, "the merged primitive is Draco-compressed");
        assert_eq!(decoded[0].num_faces(), 4, "all faces survive compression");

        let (_d2, out) = write("out.glb", &bytes);
        let out_c = CString::new(out.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: same import/release discipline as `convert`; released before the assertions so a
        // failure cannot leak the scene.
        let (meshes, ok) = unsafe {
            let s = russimp_ng::sys::aiImportFile(out_c.as_ptr(), AI_PROCESS_TRIANGULATE);
            if s.is_null() {
                (0, false)
            } else {
                let n = (*s).mNumMeshes;
                russimp_ng::sys::aiReleaseImport(s);
                (n, true)
            }
        };
        assert!(ok, "the optimised GLB did not re-import");
        assert_eq!(meshes, 1, "one merged mesh survives the round trip");
    }

    #[test]
    fn draco_compression_reduces_a_nontrivial_glb_and_preserves_faces() {
        let side = 32;
        let (_dir, path) = write("grid.obj", large_grid_obj(side).as_bytes());
        let plain = convert(&path, "glb", false).unwrap();
        let compressed = convert(&path, "glb", true).unwrap();
        assert!(
            compressed.len() < plain.len(),
            "Draco did not reduce the grid GLB: {} -> {} bytes",
            plain.len(),
            compressed.len()
        );

        let decoded = draco_gltf::import_slice(&compressed, None).unwrap();
        let meshes: Vec<_> = decoded
            .draco_primitives()
            .map(|primitive| decoded.decode_draco_primitive(primitive).unwrap())
            .collect();
        assert_eq!(meshes.len(), 1);
        assert_eq!(meshes[0].num_faces(), side * side * 2);
    }

    /// Optimisation must not turn a refusal into a success (or a panic) — the property-store import
    /// path is a second FFI entry point, so every guard in `convert` has to hold on it too.
    #[test]
    fn the_optimised_path_refuses_the_same_inputs_as_the_plain_one() {
        let (_d, prose) = write("notamodel.obj", b"this is not geometry, it is prose");
        let err = convert(&prose, "glb", true).expect_err("must refuse");
        assert!(matches!(err, HandlerError::Corrupt(_)), "{err:?}");

        let (_d2, broken) = write("broken.fbx", b"\x00\x01\x02 not an fbx");
        assert!(convert(&broken, "glb", true).is_err());

        let (_d3, tri) = write("tri.obj", TRIANGLE_OBJ.as_bytes());
        assert!(matches!(
            convert(&tri, "gltf", true),
            Err(HandlerError::Unsupported(_))
        ));
    }

    #[test]
    fn an_unsupported_target_is_refused_by_name() {
        let (_d, p) = write("tri.obj", TRIANGLE_OBJ.as_bytes());
        for target in ["fbx", "usd", "png"] {
            let err = match convert(&p, target, false) {
                Err(e) => e,
                Ok(_) => panic!("{target} must be refused, but it converted"),
            };
            assert!(
                matches!(err, HandlerError::Unsupported(_)),
                "{target}: expected Unsupported, got {err:?}"
            );
        }
        assert!(supports_target("glb"));
        assert!(supports_target("gltf"));
        assert!(supports_target("obj"));
        assert!(!supports_target("fbx"));
        assert!(!supports_target("usd"));
    }

    #[test]
    fn bundle_names_are_platform_independently_safe() {
        for safe in ["triangle", "model 01", "mødel"] {
            assert!(is_safe_file_component(safe));
        }
        for unsafe_name in ["", ".", "..", "../evil", "..\\evil", "a/b", "a\\b", "a\0b"] {
            assert!(
                !is_safe_file_component(unsafe_name),
                "accepted {unsafe_name:?}"
            );
        }

        let (_d, path) = write("tri.obj", TRIANGLE_OBJ.as_bytes());
        for stem in ["..", "../evil", "..\\evil", "nested/name", "nested\\name"] {
            let error = convert_bundle(&path, "gltf", false, stem).unwrap_err();
            assert!(error.to_string().contains("safe path component"));
        }
    }

    #[test]
    fn gltf_and_obj_bundles_include_rebased_companions_and_reimport() {
        let (_d, p) = write("tri.obj", TRIANGLE_OBJ.as_bytes());
        for target in ["gltf", "obj"] {
            let bundle = convert_bundle(&p, target, false, "triangle")
                .unwrap_or_else(|error| panic!("{target} bundle failed: {error}"));
            assert!(!bundle.primary.is_empty());
            assert!(!bundle.companions.is_empty(), "{target} needs a sidecar");
            assert!(bundle
                .companions
                .iter()
                .all(|part| part.name.starts_with("triangle.")));
            for part in &bundle.companions {
                assert!(
                    bundle
                        .primary
                        .windows(part.name.len())
                        .any(|window| window == part.name.as_bytes())
                        || target == "obj",
                    "{target} primary does not reference {}",
                    part.name
                );
            }

            let dir = tempfile::tempdir().unwrap();
            let primary = dir.path().join(format!("triangle.{target}"));
            std::fs::write(&primary, &bundle.primary).unwrap();
            for part in bundle.companions {
                std::fs::write(dir.path().join(part.name), part.bytes).unwrap();
            }
            let primary_c = CString::new(primary.as_os_str().as_encoded_bytes()).unwrap();
            let imported = unsafe {
                let scene =
                    russimp_ng::sys::aiImportFile(primary_c.as_ptr(), AI_PROCESS_TRIANGULATE);
                if scene.is_null() {
                    false
                } else {
                    let has_mesh = (*scene).mNumMeshes > 0;
                    russimp_ng::sys::aiReleaseImport(scene);
                    has_mesh
                }
            };
            assert!(imported, "{target} bundle did not re-import");
        }
    }

    #[test]
    fn optimised_text_gltf_keeps_a_rebased_draco_companion_and_decodes() {
        let (_source_dir, path) = write("tri.obj", TRIANGLE_OBJ.as_bytes());
        let bundle = convert_bundle(&path, "gltf", true, "compressed-triangle").unwrap();
        assert!(bundle
            .primary
            .windows("KHR_draco_mesh_compression".len())
            .any(|window| window == b"KHR_draco_mesh_compression"));
        assert_eq!(bundle.companions.len(), 1);
        assert_eq!(bundle.companions[0].name, "compressed-triangle.bin");
        assert!(bundle
            .primary
            .windows(bundle.companions[0].name.len())
            .any(|window| window == bundle.companions[0].name.as_bytes()));

        let output_dir = tempfile::tempdir().unwrap();
        let primary = output_dir.path().join("compressed-triangle.gltf");
        std::fs::write(&primary, bundle.primary).unwrap();
        for companion in bundle.companions {
            std::fs::write(output_dir.path().join(companion.name), companion.bytes).unwrap();
        }
        let import = draco_gltf::import(&primary).unwrap();
        let decoded: Vec<_> = import
            .draco_primitives()
            .map(|primitive| import.decode_draco_primitive(primitive).unwrap())
            .collect();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].num_faces(), 1);
    }

    /// Fail-soft, per the handler contract: a file that is not a model is a per-item error, never
    /// a panic — this path is raw FFI, so that matters more than usual.
    ///
    /// The case that motivated the geometry check: Assimp's OBJ reader *skips* lines it does not
    /// recognise, so prose "imports" as a scene with one node and no meshes and exports to a
    /// perfectly well-formed GLB containing nothing. Reporting that as a successful conversion
    /// would hand someone an empty model and call it done.
    #[test]
    fn a_file_with_no_geometry_is_refused_rather_than_exported_empty() {
        let (_d, p) = write("notamodel.obj", b"this is not geometry, it is prose");
        let err = convert(&p, "glb", false).expect_err("must refuse");
        assert!(matches!(err, HandlerError::Corrupt(_)), "{err:?}");
        assert!(
            err.to_string().contains("no geometry"),
            "the refusal should say why: {err}"
        );
    }

    /// A file Assimp cannot parse at all — distinct from the empty-scene case above.
    #[test]
    fn an_unreadable_file_is_refused_not_fatal() {
        let (_d, p) = write("broken.fbx", b"\x00\x01\x02 not an fbx");
        assert!(convert(&p, "glb", false).is_err());
    }
}
