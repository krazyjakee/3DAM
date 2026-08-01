//! 3D container transcode — any Assimp-readable model to a self-contained **GLB** (issue #49,
//! tech-spec 08 §3.3).
//!
//! ## Why this is nearly free
//!
//! Assimp ships exporters as well as importers, and the static library `dam-render` already builds
//! for thumbnails has them compiled in (`ASSIMP_NO_EXPORT` defaults off, and nothing in this tree
//! turns it on). `russimp-ng` re-exports the generated bindings, so the whole path is raw FFI over
//! a library that is already linked — no new native dependency, no second cmake build.
//!
//! ## Why GLB only, for now
//!
//! Assimp returns a *blob chain*, and the convert pipeline's encode seam is a single `Vec<u8>`
//! (`dam-core::convert::encode` → `atomic_write`). `glb2` produces exactly one part, so it drops in
//! untouched. `gltf2` is two (`.gltf` + `.bin`) and `obj` is two (`.obj` + `.mtl`), so both need a
//! multi-file output seam — a change to the pipeline's non-destructive write discipline, which is
//! not something to bolt on as a side effect of adding a format. Deferred deliberately; tech-spec
//! 08 §3.3 lists them as v1 targets and they remain open.
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
//! It is "give me a self-contained GLB of this model" — for preview, web delivery, or handoff. The
//! output is **indexed**, which is not automatic here: see the `aiCopyScene` comment in `convert`
//! for the flag interaction that otherwise triples the vertex count.
//! It is **not** lossless interchange. Assimp's exporters re-interpret: a one-material textured
//! cube comes back with two materials, because a default is appended. Custom properties and
//! non-PBR material extensions are lost. That is acceptable only because the pipeline is
//! non-destructive by construction — the original is never touched (tech-spec 08 §5.1) — so a
//! lossy convert is always an addition, never a replacement.

use std::ffi::CString;
use std::path::Path;

use crate::HandlerError;

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

const AI_PROCESS_JOIN_IDENTICAL_VERTICES: u32 = 0x2;
const AI_PROCESS_TRIANGULATE: u32 = 0x8;
const AI_PROCESS_GEN_SMOOTH_NORMALS: u32 = 0x40;
const AI_PROCESS_EMBED_TEXTURES: u32 = 0x1000_0000;

/// Target formats this module accepts, mapped to Assimp's exporter ids.
///
/// Only single-part exporters: see the module docs on why `gltf`/`obj` are not here yet.
fn exporter_id(target: &str) -> Option<&'static str> {
    match target {
        "glb" => Some("glb2"),
        _ => None,
    }
}

/// Is `target` a 3D container this build can write?
pub fn supports_target(target: &str) -> bool {
    exporter_id(target).is_some()
}

/// Owns the imported `aiScene` so every exit path releases it, including the error ones.
struct Scene(*const russimp_ng::sys::aiScene);

impl Drop for Scene {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `aiImportFile` and is released exactly once, here.
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
/// EXPENSIVE tier — the convert pipeline only, never at ingest.
pub fn convert(path: &Path, target_format: &str) -> Result<Vec<u8>, HandlerError> {
    let id = exporter_id(target_format).ok_or_else(|| {
        HandlerError::Unsupported(format!(
            "3D target '{target_format}' is not supported (this build writes: glb)"
        ))
    })?;

    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| HandlerError::Unsupported("path contains a NUL byte".into()))?;
    // `id` is one of this module's own string literals, so this cannot fail.
    let id_c = CString::new(id).expect("exporter ids are NUL-free literals");

    // SAFETY: `path_c` outlives the call; Assimp copies what it needs. A null return means the
    // import failed, and the reason is fetched immediately (see `last_error`).
    let scene = unsafe { russimp_ng::sys::aiImportFile(path_c.as_ptr(), IMPORT_FLAGS) };
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
    let (bytes, extra_parts) = unsafe {
        let head = &*blob.0;
        let mut parts = 0usize;
        let mut next = head.next;
        while !next.is_null() {
            parts += 1;
            next = (*next).next;
        }
        // A blob that was created but never written has `data == null, size == 0`, and
        // `from_raw_parts(null, 0)` is UB even at zero length — so the null case is handled
        // rather than relying on the emptiness check further down.
        let data = if head.data.is_null() || head.size == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(head.data as *const u8, head.size).to_vec()
        };
        (data, parts)
    };

    // A chained blob means the exporter wanted to write sidecars (`.bin`, `.mtl`). Writing only
    // the first part would produce a file that looks fine and references data that does not exist,
    // which is worse than refusing. `glb2` is single-part, so this should be unreachable — it
    // exists so that adding a format to `exporter_id` without extending the seam fails loudly.
    if extra_parts > 0 {
        return Err(HandlerError::Unsupported(format!(
            "'{target_format}' produces {} files, which this pipeline cannot yet write as one \
             output (issue #49)",
            extra_parts + 1
        )));
    }
    if bytes.is_empty() {
        return Err(HandlerError::Encode("Assimp produced an empty file".into()));
    }
    Ok(bytes)
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
        let bytes = convert(&p, "glb").expect("OBJ → GLB must convert");

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
        let bytes = convert(&p, "glb").unwrap();

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

    /// Pull `(position_count, triangle_count)` out of a GLB's JSON chunk.
    fn glb_counts(glb: &[u8]) -> (u64, u64) {
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
        let doc = doc.expect("GLB has a JSON chunk");
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
        let bytes = convert(&p, "glb").expect("grid must convert");
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

    #[test]
    fn an_unsupported_target_is_refused_by_name() {
        let (_d, p) = write("tri.obj", TRIANGLE_OBJ.as_bytes());
        // `gltf` and `obj` are real Assimp exporters, but both emit sidecars the single-`Vec<u8>`
        // convert seam cannot write — so they must be refused *here*, clearly, rather than
        // producing a first part that silently references a file nobody wrote.
        for target in ["gltf", "obj", "fbx", "png"] {
            let err = match convert(&p, target) {
                Err(e) => e,
                Ok(_) => panic!("{target} must be refused, but it converted"),
            };
            assert!(
                matches!(err, HandlerError::Unsupported(_)),
                "{target}: expected Unsupported, got {err:?}"
            );
        }
        assert!(supports_target("glb"));
        assert!(!supports_target("gltf"));
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
        let err = convert(&p, "glb").expect_err("must refuse");
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
        assert!(convert(&p, "glb").is_err());
    }
}
