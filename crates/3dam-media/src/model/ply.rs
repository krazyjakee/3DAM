//! PLY header parsing for cheap-tier vertex and face counts.

use dam_api::dto::ModelAttributes;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// PLY carries element counts in its ASCII header (`element vertex N` / `element face N`) — cheap
/// to read without touching the body (tech-spec 04 §7.3).
pub(super) fn metadata(path: &Path) -> Option<ModelAttributes> {
    let f = File::open(path).ok()?;
    from_reader(f)
}

pub(super) fn from_reader(f: impl Read) -> Option<ModelAttributes> {
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
