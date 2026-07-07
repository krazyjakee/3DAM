//! 3D model cheap-tier metadata (tech-spec 04 §4.1/§5, `../3d-handler-notes.md`).
//!
//! Everything here is the **CHEAP tier**: it walks container headers / the glTF JSON chunk / a
//! bounded text or binary header scan and never decodes geometry buffers or touches the GPU. glTF
//! and GLB read exact accessor `count`s from JSON (no BIN decode); OBJ/PLY/STL read counts from a
//! header or a bounded scan. Counts are marked approximate where a format cannot give them cheaply
//! (ADR 0009 §8 — "geometry counts approximate when accessor counts are absent").

use dam_api::dto::ModelAttributes;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const GLB_MAGIC: u32 = 0x4654_6C67; // "glTF" little-endian
const GLB_CHUNK_JSON: u32 = 0x4E4F_534A; // "JSON"

/// Extract cheap model attributes; best-effort, fail-soft (a parse fault yields whatever was read).
pub fn metadata(path: &Path, format: &str) -> ModelAttributes {
    let attrs = match format {
        "glb" => glb(path),
        "gltf" => gltf(path),
        "obj" => obj(path),
        "stl" => stl(path),
        "ply" => ply(path),
        _ => None,
    };
    let mut attrs = attrs.unwrap_or_default();
    // The reported size of a model should reflect the whole asset — its external textures and
    // buffers, not just the mesh container — so a 130 KB `.fbx` with 14 MB of maps reads honestly.
    attrs.dependency_bytes = dependency_bytes(path, format);
    attrs
}

// ── glTF family ────────────────────────────────────────────────────────────

fn glb(path: &Path) -> Option<ModelAttributes> {
    let mut f = File::open(path).ok()?;
    let mut header = [0u8; 12];
    f.read_exact(&mut header).ok()?;
    let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    if magic != GLB_MAGIC {
        return None;
    }
    // header[8..12] = total length; then chunks: [u32 len][u32 type][bytes]. First chunk is JSON.
    let mut chunk_head = [0u8; 8];
    f.read_exact(&mut chunk_head).ok()?;
    let chunk_len =
        u32::from_le_bytes([chunk_head[0], chunk_head[1], chunk_head[2], chunk_head[3]]) as usize;
    let chunk_type =
        u32::from_le_bytes([chunk_head[4], chunk_head[5], chunk_head[6], chunk_head[7]]);
    if chunk_type != GLB_CHUNK_JSON {
        return None;
    }
    // Bound the JSON read defensively; the BIN chunk after it is deliberately never touched.
    let mut json = vec![0u8; chunk_len];
    f.read_exact(&mut json).ok()?;
    let root: Value = serde_json::from_slice(&json).ok()?;
    Some(from_gltf_json(&root))
}

fn gltf(path: &Path) -> Option<ModelAttributes> {
    let bytes = std::fs::read(path).ok()?;
    let root: Value = serde_json::from_slice(&bytes).ok()?;
    Some(from_gltf_json(&root))
}

/// Derive counts from a parsed glTF document. Vertex/triangle totals sum per primitive from the
/// accessor `count` fields — exact, no BIN decode (`../3d-handler-notes.md` §1).
fn from_gltf_json(root: &Value) -> ModelAttributes {
    let accessors = root.get("accessors").and_then(Value::as_array);
    let meshes = root.get("meshes").and_then(Value::as_array);

    let mut vertex_count: i64 = 0;
    let mut triangle_count: i64 = 0;
    let mut has_uvs = false;

    if let Some(meshes) = meshes {
        for mesh in meshes {
            let Some(prims) = mesh.get("primitives").and_then(Value::as_array) else {
                continue;
            };
            for prim in prims {
                let attributes = prim.get("attributes");
                let pos_idx = attributes
                    .and_then(|a| a.get("POSITION"))
                    .and_then(Value::as_u64);
                let pos_count = pos_idx
                    .and_then(|i| accessor_count(accessors, i))
                    .unwrap_or(0);
                vertex_count += pos_count;
                if attributes
                    .and_then(|a| a.get("TEXCOORD_0"))
                    .and_then(Value::as_u64)
                    .is_some()
                {
                    has_uvs = true;
                }
                let mode = prim.get("mode").and_then(Value::as_u64).unwrap_or(4);
                let index_count = prim
                    .get("indices")
                    .and_then(Value::as_u64)
                    .and_then(|i| accessor_count(accessors, i));
                let elems = index_count.unwrap_or(pos_count);
                triangle_count += triangles_for(mode, elems);
            }
        }
    }

    let count_of = |k: &str| {
        root.get(k)
            .and_then(Value::as_array)
            .map(|a| a.len() as i64)
    };
    let has_rig = root
        .get("skins")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty());
    let has_animation = root
        .get("animations")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty());

    ModelAttributes {
        vertex_count: nonzero(vertex_count),
        triangle_count: nonzero(triangle_count),
        mesh_count: count_of("meshes"),
        material_count: count_of("materials"),
        texture_count: count_of("textures").or_else(|| count_of("images")),
        dependency_bytes: None,
        has_rig,
        has_animation,
        has_uvs: Some(has_uvs),
        class: None,
    }
}

fn accessor_count(accessors: Option<&Vec<Value>>, idx: u64) -> Option<i64> {
    accessors?
        .get(idx as usize)?
        .get("count")
        .and_then(Value::as_i64)
}

/// Triangle count for a glTF primitive `mode` given the element (index/vertex) count.
fn triangles_for(mode: u64, elems: i64) -> i64 {
    match mode {
        4 => elems / 3,              // TRIANGLES
        5 | 6 => (elems - 2).max(0), // TRIANGLE_STRIP | TRIANGLE_FAN
        _ => 0,                      // POINTS/LINES/… contribute no triangles
    }
}

// ── OBJ ──────────────────────────────────────────────────────────────────────

/// OBJ has no reliable magic (§3.2): a bounded line scan counts `v`/`f` tokens. Each `f` is a
/// polygon fan → (verts-2) triangles. Vertex count is the `v` line count.
fn obj(path: &Path) -> Option<ModelAttributes> {
    let f = File::open(path).ok()?;
    let reader = BufReader::new(f);
    let mut vertex_count: i64 = 0;
    let mut triangle_count: i64 = 0;
    let mut has_uvs = false;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("v ") {
            let _ = rest;
            vertex_count += 1;
        } else if line.starts_with("vt ") {
            has_uvs = true;
        } else if let Some(rest) = line.strip_prefix("f ") {
            let verts = rest.split_whitespace().count() as i64;
            triangle_count += (verts - 2).max(0);
        }
    }
    Some(ModelAttributes {
        vertex_count: nonzero(vertex_count),
        triangle_count: nonzero(triangle_count),
        mesh_count: Some(1),
        material_count: None,
        texture_count: None,
        dependency_bytes: None,
        has_rig: Some(false),
        has_animation: Some(false),
        has_uvs: Some(has_uvs),
        class: None,
    })
}

// ── STL ──────────────────────────────────────────────────────────────────────

/// STL: binary carries the triangle count in a 4-byte header field after the 80-byte comment;
/// ASCII needs a bounded `facet` scan. Vertices are not deduplicated in STL → 3 per triangle.
fn stl(path: &Path) -> Option<ModelAttributes> {
    let mut f = File::open(path).ok()?;
    let mut head = [0u8; 84];
    let n = f.read(&mut head).ok()?;
    if n < 84 {
        // Too short for a binary header; treat as ASCII.
        return stl_ascii(path);
    }
    // ASCII STL starts with "solid"; but some binary files also do, so validate against size.
    let tri_count = u32::from_le_bytes([head[80], head[81], head[82], head[83]]) as u64;
    let expected_binary_len = 84 + tri_count * 50;
    let file_len = f.seek(SeekFrom::End(0)).ok()?;
    if file_len == expected_binary_len {
        return Some(model_from_tris(tri_count as i64));
    }
    if head.starts_with(b"solid") {
        return stl_ascii(path);
    }
    // Fall back to the header's declared count when the size heuristic is inconclusive.
    Some(model_from_tris(tri_count as i64))
}

fn stl_ascii(path: &Path) -> Option<ModelAttributes> {
    let f = File::open(path).ok()?;
    let reader = BufReader::new(f);
    let mut tris: i64 = 0;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim_start().starts_with("facet normal") {
            tris += 1;
        }
    }
    Some(model_from_tris(tris))
}

fn model_from_tris(tris: i64) -> ModelAttributes {
    ModelAttributes {
        vertex_count: nonzero(tris * 3),
        triangle_count: nonzero(tris),
        mesh_count: Some(1),
        material_count: None,
        texture_count: None,
        dependency_bytes: None,
        has_rig: Some(false),
        has_animation: Some(false),
        has_uvs: Some(false),
        class: None,
    }
}

// ── PLY ──────────────────────────────────────────────────────────────────────

/// PLY carries element counts in its ASCII header (`element vertex N` / `element face N`) — cheap
/// to read without touching the body (tech-spec 04 §7.3).
fn ply(path: &Path) -> Option<ModelAttributes> {
    let f = File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    if first.trim() != "ply" {
        return None;
    }
    let mut vertex_count: Option<i64> = None;
    let mut face_count: Option<i64> = None;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line == "end_header" {
            break;
        }
        if let Some(rest) = line.strip_prefix("element vertex ") {
            vertex_count = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("element face ") {
            face_count = rest.trim().parse().ok();
        }
    }
    Some(ModelAttributes {
        vertex_count,
        triangle_count: face_count, // faces ≈ triangles (quads counted as one; approximate)
        mesh_count: Some(1),
        material_count: None,
        texture_count: None,
        dependency_bytes: None,
        has_rig: Some(false),
        has_animation: Some(false),
        has_uvs: None,
        class: None,
    })
}

fn nonzero(n: i64) -> Option<i64> {
    (n > 0).then_some(n)
}

// ── dependency size (external textures / buffers) ──────────────────────────────
//
// The cheap tier statting a model's *referenced* companion files so the catalog can report the
// whole asset's footprint, not just the mesh container. It reads text headers / a bounded byte
// scan and stats siblings — it never decodes a texture or a geometry buffer.

/// Recognised texture extensions for the sibling scan (lower-case, no dot).
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "tga", "tif", "tiff", "bmp", "webp", "dds", "exr", "psd", "gif",
];

/// Cap on how much of a (potentially huge) FBX to scan for texture-path strings.
const FBX_SCAN_CAP: u64 = 64 * 1024 * 1024;

/// Total on-disk bytes of the files a model references beyond its own container — external textures,
/// glTF `.bin` buffers, an OBJ's `.mtl` and its maps, plus the sibling maps that belong to the same
/// texture set by naming convention. `None` when the format is self-contained (GLB/STL/PLY embed or
/// carry no external data) or nothing external resolves.
pub fn dependency_bytes(path: &Path, format: &str) -> Option<i64> {
    let files = match format {
        "gltf" => gltf_deps(path),
        "obj" => obj_deps(path),
        "fbx" => fbx_deps(path),
        _ => Vec::new(), // glb (embedded), stl, ply carry no external references
    };
    let self_canon = std::fs::canonicalize(path).ok();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut total: i64 = 0;
    for f in files {
        let canon = std::fs::canonicalize(&f).unwrap_or(f);
        if Some(&canon) == self_canon.as_ref() || !seen.insert(canon.clone()) {
            continue; // never count the model file itself, and dedup shared maps
        }
        if let Ok(md) = std::fs::metadata(&canon) {
            if md.is_file() {
                total += md.len() as i64;
            }
        }
    }
    (total > 0).then_some(total)
}

/// External `buffers[].uri` / `images[].uri` a loose `.gltf` points at (data-URIs are inline).
fn gltf_deps(path: &Path) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in ["buffers", "images"] {
        let Some(arr) = root.get(key).and_then(Value::as_array) else {
            continue;
        };
        for item in arr {
            if let Some(uri) = item.get("uri").and_then(Value::as_str) {
                if uri.starts_with("data:") {
                    continue;
                }
                out.push(dir.join(uri.replace("%20", " ")));
            }
        }
    }
    out
}

/// The `.mtl` an OBJ names in `mtllib`, plus every texture map it references (`map_Kd`, `bump`, …).
fn obj_deps(path: &Path) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(f) = File::open(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if let Some(rest) = line.trim_start().strip_prefix("mtllib ") {
            for name in rest.split_whitespace() {
                let mtl = dir.join(name);
                out.extend(mtl_maps(&mtl));
                out.push(mtl);
            }
        }
    }
    out
}

/// Texture maps referenced by a `.mtl` file. Map statements may carry options
/// (`map_Kd -bm 0.2 tex.png`), so the filename is the last whitespace token.
fn mtl_maps(mtl: &Path) -> Vec<PathBuf> {
    let dir = mtl.parent().unwrap_or_else(|| Path::new("."));
    let Ok(f) = File::open(mtl) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let line = line.trim_start();
        let is_map = line.starts_with("map_")
            || line.starts_with("bump ")
            || line.starts_with("disp ")
            || line.starts_with("decal ")
            || line.starts_with("norm ")
            || line.starts_with("refl ");
        if is_map {
            if let Some(file) = line.split_whitespace().next_back() {
                out.push(dir.join(file.replace('\\', "/")));
            }
        }
    }
    out
}

/// FBX texture paths (baked as `RelativeFilename`/`Filename` strings), resolved against the model
/// dir + sibling texture folders, plus the rest of each texture's naming-convention set — many FBX
/// exports reference only a subset (e.g. mask + normal) of maps that ship alongside.
fn fbx_deps(path: &Path) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let cap = f
        .metadata()
        .map(|m| m.len().min(FBX_SCAN_CAP))
        .unwrap_or(FBX_SCAN_CAP);
    let mut buf = vec![0u8; cap as usize];
    let Ok(n) = f.read(&mut buf) else {
        return Vec::new();
    };
    buf.truncate(n);

    let mut out = Vec::new();
    for reference in extract_texture_paths(&buf) {
        if let Some(resolved) = resolve_sibling(dir, &reference) {
            out.extend(companion_siblings(&resolved));
            out.push(resolved);
        }
    }
    out
}

/// Pull filename-like ASCII runs ending in an image extension out of a byte blob (FBX stores texture
/// paths as plain strings, binary or ASCII).
fn extract_texture_paths(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut flush = |run: &mut String| {
        if is_image_name(run) {
            out.push(std::mem::take(run));
        } else {
            run.clear();
        }
    };
    for &b in bytes {
        // Printable ASCII plus the separators that appear inside paths.
        if b.is_ascii_graphic() || b == b' ' {
            run.push(b as char);
        } else {
            flush(&mut run);
        }
    }
    flush(&mut run);
    out
}

/// Whether a string looks like a path ending in a known image extension.
fn is_image_name(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    IMAGE_EXTS.iter().any(|e| lower.ends_with(&format!(".{e}")))
}

/// Sibling texture folders game/DCC exports conventionally use, mirroring the render crate's
/// resolver so the counted set matches what the thumbnail actually draws.
const TEXTURE_DIRS: &[&str] = &[
    "Textures",
    "textures",
    "Texture",
    "texture",
    "../Textures",
    "../textures",
    "Maps",
    "maps",
    "tex",
];

/// Resolve an FBX/DCC texture reference to an existing file: the path as-given (relative to the
/// model dir), then the bare filename in the model dir and the conventional sibling folders.
fn resolve_sibling(dir: &Path, reference: &str) -> Option<PathBuf> {
    let cleaned = reference.replace('\\', "/");
    let cleaned = cleaned.trim_start_matches("./");
    let mut cands = vec![dir.join(cleaned)];
    if let Some(name) = Path::new(cleaned).file_name() {
        cands.push(dir.join(name));
        for sib in TEXTURE_DIRS {
            cands.push(dir.join(sib).join(name));
        }
    }
    cands.into_iter().find(|c| c.is_file())
}

/// Every image sibling that shares a resolved texture's asset base name (`T_Table_Mask.png` →
/// `T_Table_*`), so a whole PBR set is counted even when the FBX names only part of it.
fn companion_siblings(file: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (file.parent(), file.file_stem().and_then(|s| s.to_str())) else {
        return Vec::new();
    };
    let Some(base) = asset_base(stem) else {
        return vec![file.to_path_buf()];
    };
    let base = base.to_ascii_lowercase();
    let prefix = format!("{base}_");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![file.to_path_buf()];
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| has_image_ext(p))
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
                .is_some_and(|s| s == base || s.starts_with(&prefix))
        })
        .collect()
}

/// Strip a trailing `_<channel>` token to recover the shared asset base name. A token counts if it's
/// a known channel word or a short alphabetic code (`_B`, `_ORM`, `_Mask`); `None` otherwise.
fn asset_base(stem: &str) -> Option<String> {
    let (base, tok) = stem.rsplit_once('_')?;
    if base.is_empty() {
        return None;
    }
    let t = tok.to_ascii_lowercase();
    let known = matches!(
        t.as_str(),
        "basecolor"
            | "albedo"
            | "diffuse"
            | "color"
            | "colour"
            | "normal"
            | "normalmap"
            | "roughness"
            | "metallic"
            | "metalness"
            | "emissive"
            | "emission"
            | "occlusion"
            | "orm"
            | "rma"
            | "arm"
            | "mask"
            | "height"
            | "ao"
            | "specular"
            | "gloss"
            | "opacity"
    );
    let short_alpha = (1..=4).contains(&t.len()) && t.chars().all(|c| c.is_ascii_alphabetic());
    (known || short_alpha).then(|| base.to_string())
}

fn has_image_ext(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| IMAGE_EXTS.contains(&e.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::File::create(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    /// An FBX that names only a subset of its texture set (mask + normal, via a relative
    /// `..\Textures\` path, plus a stale absolute Windows path) still counts the *whole* sibling
    /// set — including the base-colour and ORM maps the export never referenced.
    #[test]
    fn fbx_counts_referenced_plus_companion_textures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Baked texture strings as they appear inside a real Unreal→Unity FBX — separated by the
        // binary (length-prefix / null) bytes that delimit strings in the container.
        let mut fbx: Vec<u8> = Vec::new();
        for field in [
            &b"RelativeFilename"[..],
            b"..\\Textures\\T_Foo_Mask.png",
            b"C:\\UnrealToUnity\\Assets\\Foo\\Textures\\T_Foo_N.png",
        ] {
            fbx.push(0);
            fbx.extend_from_slice(field);
            fbx.push(0);
        }
        write(&root.join("Meshes/SM_Foo.fbx"), &fbx);
        write(&root.join("Textures/T_Foo_B.png"), &[0u8; 4000]);
        write(&root.join("Textures/T_Foo_N.png"), &[0u8; 2000]);
        write(&root.join("Textures/T_Foo_ORM.png"), &[0u8; 3000]);
        write(&root.join("Textures/T_Foo_Mask.png"), &[0u8; 1000]);
        // An unrelated texture in the same folder must NOT be counted.
        write(&root.join("Textures/T_Other_B.png"), &[0u8; 9999]);

        let deps = dependency_bytes(&root.join("Meshes/SM_Foo.fbx"), "fbx");
        assert_eq!(deps, Some(4000 + 2000 + 3000 + 1000));
    }

    /// A loose glTF counts its external `.bin` buffer and image URIs, but not inline data-URIs.
    #[test]
    fn gltf_counts_external_buffers_and_images() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let gltf = br#"{"buffers":[{"uri":"scene.bin"},{"uri":"data:application/octet-stream;base64,AAAA"}],
                        "images":[{"uri":"albedo.png"},{"uri":"tex%20with%20space.png"}]}"#;
        write(&root.join("scene.gltf"), gltf);
        write(&root.join("scene.bin"), &[0u8; 5000]);
        write(&root.join("albedo.png"), &[0u8; 800]);
        write(&root.join("tex with space.png"), &[0u8; 200]);

        let deps = dependency_bytes(&root.join("scene.gltf"), "gltf");
        assert_eq!(deps, Some(5000 + 800 + 200));
    }

    /// An OBJ counts its `.mtl` and every map the material references (last token wins past options).
    #[test]
    fn obj_counts_mtl_and_maps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("m.obj"), b"mtllib m.mtl\nv 0 0 0\n");
        let mtl = b"newmtl x\nmap_Kd -bm 0.2 albedo.png\nbump normal.png\n";
        write(&root.join("m.mtl"), mtl);
        write(&root.join("albedo.png"), &[0u8; 700]);
        write(&root.join("normal.png"), &[0u8; 300]);

        let mtl_len = mtl.len() as i64;
        let deps = dependency_bytes(&root.join("m.obj"), "obj");
        assert_eq!(deps, Some(700 + 300 + mtl_len));
    }

    /// Self-contained formats report no external dependency footprint.
    #[test]
    fn embedded_formats_have_no_deps() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.glb");
        write(&p, &[0u8; 128]);
        assert_eq!(dependency_bytes(&p, "glb"), None);
    }

    #[test]
    fn asset_base_strips_known_and_short_tokens() {
        assert_eq!(asset_base("T_Foo_Mask").as_deref(), Some("T_Foo"));
        assert_eq!(asset_base("T_Foo_ORM").as_deref(), Some("T_Foo"));
        assert_eq!(asset_base("Wood_BaseColor").as_deref(), Some("Wood"));
        // A trailing numeric token is not a channel suffix → no base, whole name kept.
        assert_eq!(asset_base("Table_01"), None);
    }
}
