//! OBJ line scanning for cheap-tier geometry and UV counts.

use super::nonzero;
use dam_api::dto::ModelAttributes;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// OBJ has no reliable magic (§3.2): a bounded line scan counts `v`/`f` tokens. Each `f` is a
/// polygon fan → (verts-2) triangles. Vertex count is the `v` line count.
pub(super) fn metadata(path: &Path) -> Option<ModelAttributes> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > 64 * 1024 * 1024 {
        return None;
    }
    let mut bytes = Vec::new();
    std::io::Read::take(file, 64 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(from_bytes(&bytes))
}

pub(super) fn from_bytes(bytes: &[u8]) -> ModelAttributes {
    let reader = BufReader::new(bytes);
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
    ModelAttributes {
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
    }
}
