//! OBJ line scanning for cheap-tier geometry and UV counts.

use super::nonzero;
use dam_api::dto::ModelAttributes;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// OBJ has no reliable magic (§3.2): a bounded line scan counts `v`/`f` tokens. Each `f` is a
/// polygon fan → (verts-2) triangles. Vertex count is the `v` line count.
pub(super) fn metadata(path: &Path) -> Option<ModelAttributes> {
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
