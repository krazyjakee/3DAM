//! GLB and glTF JSON cheap-tier geometry metadata parsing.

use super::nonzero;
use dam_api::dto::ModelAttributes;
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const GLB_MAGIC: u32 = 0x4654_6C67; // "glTF" little-endian
const GLB_CHUNK_JSON: u32 = 0x4E4F_534A; // "JSON"

pub(super) fn glb(path: &Path) -> Option<ModelAttributes> {
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

pub(super) fn gltf(path: &Path) -> Option<ModelAttributes> {
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
