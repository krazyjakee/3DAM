//! Binary and ASCII STL cheap-tier triangle metadata parsing.

use super::nonzero;
use dam_api::dto::ModelAttributes;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// STL: binary carries the triangle count in a 4-byte header field after the 80-byte comment;
/// ASCII needs a bounded `facet` scan. Vertices are not deduplicated in STL → 3 per triangle.
pub(super) fn metadata(path: &Path) -> Option<ModelAttributes> {
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
    let mut declares_solid = false;
    for (n, line) in reader.lines().map_while(Result::ok).enumerate() {
        let line = line.trim_start();
        if n == 0 {
            declares_solid = line.starts_with("solid");
        }
        if line.starts_with("facet normal") {
            tris += 1;
        }
    }
    // Neither the ASCII marker nor a single facet means this is not an STL, just a file wearing the
    // extension — and "one mesh with no triangles" is a confident wrong answer, not a fail-soft one.
    (declares_solid || tris > 0).then(|| model_from_tris(tris))
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
