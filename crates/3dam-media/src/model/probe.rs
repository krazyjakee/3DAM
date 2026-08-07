//! Exact 3D geometry metadata via a full Assimp import (issue #49) — the **DEEP tier**.
//!
//! ## Why this exists when the cheap tier already answers
//!
//! The header/structure readers next door (`fbx.rs`, `dae.rs`, `tds.rs`, and the glTF/OBJ/PLY/STL
//! paths in `model.rs`) get real counts out of every format without decoding geometry, and that is
//! what a scan uses. They cannot be exact everywhere, though: an FBX whose `PolygonVertexIndex` is
//! deflate-compressed only publishes how many *indices* it has, not how they group into polygons, so
//! the cheap tier assumes triangles; a Collada `<polygons>` block with no `<vcount>` has the same
//! problem; and `.blend` has no cheap structure to walk at all. Running the geometry through a real
//! importer settles all of that — triangulated, de-duplicated, counted from the actual mesh.
//!
//! ## Why it is not in the scan path
//!
//! This decodes. It reads every vertex, builds the node graph, and allocates the whole scene, which
//! is precisely what the cost-tiered handler contract (tech-spec 04 §4) keeps out of ingest. So it
//! belongs to the **analyse** pass, which is already the tier that is allowed to be expensive and is
//! already the one that fans out over rayon. It is also behind the `model-convert` feature, because
//! turning it on builds Assimp from source — a default build keeps the cheap answers and loses
//! nothing else.
//!
//! ## Fail-soft over FFI
//!
//! Assimp is C++. Every failure mode here is a `null` return that becomes a `HandlerError`, and the
//! imported scene is owned by a guard so an early return still releases it. A malformed file is one
//! asset's error, never a panic and never an aborted job (golden rule 6).

use dam_api::dto::ModelAttributes;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::path::Path;

use crate::HandlerError;

/// Post-processing applied at import.
///
/// Deliberately the *minimum* that makes counting meaningful, not the render/convert set:
/// `Triangulate` so `mNumFaces` is a triangle count for every format, and `JoinIdenticalVertices`
/// so the vertex count is the shared-vertex one a budget is actually measured in. Normal
/// generation, texture embedding and vertex pre-transforms all cost real work and change nothing
/// that is counted here, so none of them are requested.
const IMPORT_FLAGS: u32 = AI_PROCESS_TRIANGULATE | AI_PROCESS_JOIN_IDENTICAL_VERTICES;

const AI_PROCESS_JOIN_IDENTICAL_VERTICES: u32 = 0x2;
const AI_PROCESS_TRIANGULATE: u32 = 0x8;

/// The material texture slots worth asking about — the classic set plus the PBR one. Assimp's
/// enumeration runs past this into per-DCC vendor slots that no importer in this tree populates.
const TEXTURE_TYPES: std::ops::RangeInclusive<u32> = 1..=17;

/// Owns the imported scene so every exit path releases it, including the error ones.
struct Scene(*const russimp_ng::sys::aiScene);

impl Drop for Scene {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `aiImportFile` and is released exactly once, here.
        unsafe { russimp_ng::sys::aiReleaseImport(self.0) }
    }
}

/// Import `path` and read exact geometry counts off the resulting scene.
///
/// EXPENSIVE tier — the analyse pass only, never at ingest.
pub(crate) fn metadata(path: &Path) -> Result<ModelAttributes, HandlerError> {
    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| HandlerError::Unsupported("path contains a NUL byte".into()))?;

    // SAFETY: `path_c` outlives the call; Assimp copies what it needs. A null return means the
    // import failed.
    let scene = unsafe { russimp_ng::sys::aiImportFile(path_c.as_ptr(), IMPORT_FLAGS) };
    if scene.is_null() {
        // No Assimp error detail, for the same reason `model_convert` gives none: the C API's error
        // string is an unsynchronised process-global that other callers reassign, so reading it
        // races. A safe generic message beats a detailed use-after-free.
        return Err(HandlerError::Corrupt(
            "Assimp could not read this file as a 3D model".into(),
        ));
    }
    let scene = Scene(scene);

    // SAFETY: `scene.0` is non-null and stays live for this whole borrow; every array read below is
    // bounded by the count field that precedes it in the struct.
    let attrs = unsafe {
        let s = &*scene.0;
        let meshes = counted_slice(s.mMeshes, s.mNumMeshes);
        let mut vertices: i64 = 0;
        let mut triangles: i64 = 0;
        let mut has_rig = false;
        let mut has_uvs = false;
        for &m in meshes {
            if m.is_null() {
                continue;
            }
            let m = &*m;
            vertices += i64::from(m.mNumVertices);
            // `Triangulate` guarantees every face is a triangle, so faces *are* triangles here —
            // no per-face index inspection needed.
            triangles += i64::from(m.mNumFaces);
            has_rig |= m.mNumBones > 0;
            has_uvs |= !m.mTextureCoords[0].is_null();
        }
        let materials = counted_slice(s.mMaterials, s.mNumMaterials);
        ModelAttributes {
            vertex_count: super::nonzero(vertices),
            triangle_count: super::nonzero(triangles),
            mesh_count: super::nonzero(i64::from(s.mNumMeshes)),
            material_count: super::nonzero(i64::from(s.mNumMaterials)),
            texture_count: super::nonzero(distinct_textures(materials)),
            // Left to the cheap tier: it stats the model's companion files on disk, which is a
            // question about the folder, not about the imported scene.
            dependency_bytes: None,
            has_rig: Some(has_rig),
            has_animation: Some(s.mNumAnimations > 0),
            has_uvs: Some(has_uvs),
            class: None,
        }
    };

    // An import that yields no geometry is not a model this can describe — the same judgement
    // `model_convert` makes, and for the same reason: Assimp's text importers are lenient enough
    // that prose "imports" as an empty scene, and reporting that as a zero-triangle model would
    // overwrite the cheap tier's honest answer with a confident wrong one.
    if attrs.mesh_count.is_none() {
        return Err(HandlerError::Corrupt(
            "the file contains no geometry".into(),
        ));
    }
    Ok(attrs)
}

/// One of Assimp's `(count, pointer)` array pairs as a slice.
///
/// The null check is not defensive padding: an empty scene — which is exactly what an unreadable
/// text file imports as — carries a **null** array pointer with a zero count, and
/// `from_raw_parts(null, 0)` is undefined behaviour even at zero length. The same trap the export
/// blob has in `model_convert`, and it aborts the process rather than returning an error, so it has
/// to be handled here instead of relied on downstream.
///
/// # Safety
///
/// `ptr` must either be null or point to `len` initialised elements owned by a live scene.
unsafe fn counted_slice<'a, T>(ptr: *mut T, len: std::os::raw::c_uint) -> &'a [T] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(ptr, len as usize)
    }
}

/// Count the *distinct* texture files the materials reference, rather than slots: a PBR set shared
/// by two materials is one texture in the catalog's sense, which is how the glTF path counts too.
///
/// # Safety
///
/// `materials` must be a live slice of non-dangling `aiMaterial` pointers from an imported scene.
unsafe fn distinct_textures(materials: &[*mut russimp_ng::sys::aiMaterial]) -> i64 {
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    for &mat in materials {
        if mat.is_null() {
            continue;
        }
        for ty in TEXTURE_TYPES {
            // `as _`, not a named type: bindgen gives C enums a signed underlying type under
            // MSVC and an unsigned one elsewhere, so `aiTextureType` is i32 on Windows and u32
            // on the other targets.
            let n = russimp_ng::sys::aiGetMaterialTextureCount(mat, ty as _);
            for i in 0..n {
                let mut path = std::mem::zeroed::<russimp_ng::sys::aiString>();
                let ok = russimp_ng::sys::aiGetMaterialTexture(
                    mat,
                    ty as _,
                    i,
                    &mut path,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                // `length` is Assimp's own bound on the fixed 1024-byte buffer; clamping it means a
                // garbage value cannot turn into an out-of-range slice.
                let len = (path.length as usize).min(path.data.len());
                if ok == russimp_ng::sys::aiReturn_aiReturn_SUCCESS && len > 0 {
                    let bytes = std::slice::from_raw_parts(path.data.as_ptr() as *const u8, len);
                    seen.insert(bytes.to_vec());
                }
            }
        }
    }
    seen.len() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, body: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        (dir, p)
    }

    /// A 4x4 vertex grid: 16 vertices, 18 triangles, every interior vertex shared by several faces.
    /// The sharing is the point — a count taken without `JoinIdenticalVertices` would read 54.
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

    #[test]
    fn a_full_import_yields_exact_shared_vertex_counts() {
        let (_d, p) = write("grid.obj", grid_obj().as_bytes());
        let m = metadata(&p).expect("a valid OBJ must import");
        assert_eq!(m.triangle_count, Some(18));
        assert_eq!(
            m.vertex_count,
            Some(16),
            "the grid's shared vertices must survive the import, not fan out to 3 per triangle"
        );
        assert_eq!(m.mesh_count, Some(1));
        assert_eq!(m.has_rig, Some(false));
        assert_eq!(m.has_animation, Some(false));
    }

    /// Fail-soft over FFI (golden rule 6): both a file Assimp cannot parse and a file it parses
    /// into nothing are per-item errors, never a crash. The second case is the one that bites —
    /// Assimp's text importers skip lines they do not recognise, so prose "imports" as an empty
    /// scene, and letting that overwrite the cheap tier's answer with zeroes would be worse than
    /// not probing at all.
    #[test]
    fn unreadable_and_empty_files_are_errors_not_crashes() {
        let (_d, p) = write("broken.fbx", b"\x00\x01\x02 not an fbx");
        assert!(matches!(metadata(&p), Err(HandlerError::Corrupt(_))));

        let (_d2, q) = write("prose.obj", b"this is a note, not geometry");
        let err = metadata(&q).expect_err("an empty scene must not report as a model");
        assert!(err.to_string().contains("no geometry"), "{err}");
    }
}
