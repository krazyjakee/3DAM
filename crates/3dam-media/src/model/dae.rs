//! Collada (`.dae`) cheap-tier geometry metadata (issue #49).
//!
//! Collada is XML, and it *declares* everything the catalog wants in attributes: a `<float_array>`
//! states its component `count`, a `<triangles>` / `<polylist>` states its primitive `count`. So a
//! single streaming pass with `quick-xml` — already in this crate for DOCX/ODT — answers the whole
//! cheap tier without materialising a DOM or parsing a single coordinate. The cost is one linear
//! read of the file, which is the same class as the OBJ line scan next door.
//!
//! Vertex count needs one hop: `<vertices>` names its POSITION `<source>` by id, and that source
//! holds the `<float_array count>` of x/y/z components. Following the reference rather than taking
//! the first array in the mesh matters because normals and UVs are sources too, and they are often
//! larger than the positions.
//!
//! Triangles are exact for `<triangles>` and for a `<polylist>` whose `<vcount>` is present (each
//! polygon of `k` vertices fans into `k - 2`); a `<polygons>` block with no vcount falls back to its
//! declared primitive count, which is right only if those polygons are triangles.

use dam_api::dto::ModelAttributes;
use quick_xml::events::Event;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// Cap on the streaming read. A Collada export of a whole level is genuinely large, and the cheap
/// tier is not the place to read an unbounded one — a truncated parse still yields honest partial
/// counts, which is the fail-soft answer.
const SCAN_CAP: u64 = 128 * 1024 * 1024;

pub(super) fn metadata(path: &Path) -> Option<ModelAttributes> {
    let f = File::open(path).ok()?;
    let mut reader = quick_xml::Reader::from_reader(BufReader::new(f.take(SCAN_CAP)));
    reader.config_mut().trim_text(true);

    // `<source id>` → the component count of the `<float_array>` inside it.
    let mut source_components: HashMap<String, i64> = HashMap::new();
    // The POSITION sources each `<vertices>` points at, in document order (meshes may share one).
    let mut position_sources: Vec<String> = Vec::new();
    let mut current_source: Option<String> = None;
    let (mut in_vertices, mut in_vcount) = (false, false);

    let mut geometries: i64 = 0;
    let mut materials: i64 = 0;
    let mut images: i64 = 0;
    let mut triangles: i64 = 0;
    let mut declared_polygons: i64 = 0;
    let mut vcount_triangles: i64 = 0;
    let mut saw_vcount = false;
    let (mut has_uvs, mut has_rig, mut has_animation) = (false, false, false);
    let mut is_collada = false;
    let mut seen_root = false;

    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf);
        match ev {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                // The document element settles whether this is Collada at all; an `.dae` that is
                // really something else must report nothing rather than a model of zero triangles.
                if !seen_root {
                    seen_root = true;
                    is_collada = name == b"COLLADA";
                    if !is_collada {
                        return None;
                    }
                }
                match name {
                    b"source" => current_source = attr(e, b"id"),
                    b"float_array" => {
                        if let (Some(id), Some(n)) = (current_source.clone(), int_attr(e, b"count"))
                        {
                            source_components.insert(id, n);
                        }
                    }
                    b"vertices" => in_vertices = true,
                    b"input" => match attr(e, b"semantic").as_deref() {
                        Some("POSITION") if in_vertices => {
                            if let Some(src) = attr(e, b"source") {
                                // Collada ids are case-sensitive, so the `#` is the only thing
                                // stripped before the lookup.
                                position_sources.push(src.trim_start_matches('#').to_string());
                            }
                        }
                        Some("TEXCOORD") => has_uvs = true,
                        _ => {}
                    },
                    b"triangles" => triangles += int_attr(e, b"count").unwrap_or(0),
                    b"polylist" | b"polygons" => {
                        declared_polygons += int_attr(e, b"count").unwrap_or(0)
                    }
                    b"vcount" => in_vcount = true,
                    b"geometry" => geometries += 1,
                    // `<material>` is the library entry; `<instance_material>` is a binding and has
                    // its own local name, so this does not double-count.
                    b"material" => materials += 1,
                    b"image" => images += 1,
                    b"skin" | b"controller" => has_rig = true,
                    b"animation" => has_animation = true,
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"source" => current_source = None,
                b"vertices" => in_vertices = false,
                b"vcount" => in_vcount = false,
                _ => {}
            },
            Ok(Event::Text(ref t)) if in_vcount => {
                if let Ok(s) = t.decode() {
                    for k in s
                        .split_ascii_whitespace()
                        .filter_map(|v| v.parse::<i64>().ok())
                    {
                        vcount_triangles += (k - 2).max(0);
                        saw_vcount = true;
                    }
                }
            }
            // A read error mid-document is fail-soft: keep whatever was counted up to here rather
            // than discarding a mostly-parsed file (the `SCAN_CAP` truncation lands here too).
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    if !is_collada {
        return None;
    }

    let vertices: i64 = position_sources
        .iter()
        .filter_map(|id| source_components.get(id))
        .map(|components| components / 3)
        .sum();
    let triangles = triangles
        + if saw_vcount {
            vcount_triangles
        } else {
            declared_polygons
        };

    Some(ModelAttributes {
        vertex_count: super::nonzero(vertices),
        triangle_count: super::nonzero(triangles),
        mesh_count: super::nonzero(geometries),
        material_count: super::nonzero(materials),
        texture_count: super::nonzero(images),
        dependency_bytes: None,
        has_rig: Some(has_rig),
        has_animation: Some(has_animation),
        has_uvs: Some(has_uvs),
        class: None,
    })
}

/// An attribute's unescaped value, if the element carries it.
fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    let a = e.try_get_attribute(key).ok()??;
    a.unescape_value().ok().map(|v| v.into_owned())
}

fn int_attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<i64> {
    attr(e, key)?.trim().parse().ok()
}
