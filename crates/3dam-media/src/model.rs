//! 3D model cheap-tier metadata (tech-spec 04 §4.1/§5, `../3d-handler-notes.md`).
//!
//! Everything here is the **CHEAP tier**: it walks container headers / the glTF JSON chunk / a
//! bounded text or binary header scan and never decodes geometry buffers or touches the GPU. glTF
//! and GLB read exact accessor `count`s from JSON (no BIN decode); OBJ/PLY/STL read counts from a
//! header or a bounded scan. Counts are marked approximate where a format cannot give them cheaply
//! (ADR 0009 §8 — "geometry counts approximate when accessor counts are absent").

use dam_api::dto::ModelAttributes;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

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
    attrs.unwrap_or_default()
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
        has_rig: Some(false),
        has_animation: Some(false),
        has_uvs: None,
        class: None,
    })
}

fn nonzero(n: i64) -> Option<i64> {
    (n > 0).then_some(n)
}
