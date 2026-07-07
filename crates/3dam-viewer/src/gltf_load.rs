//! glTF / GLB decode → one flattened CPU mesh, ready for a single GPU upload.
//!
//! The DOM hands the island raw model bytes (`load_model(&[u8])`, tech-spec 09 §B.3); this turns
//! them into positions/normals/base-colour + indices with node transforms baked in. It is a
//! *viewer-grade* decode (self-contained GLB / data-URI buffers — the common preview case), not the
//! full ingest decoder that file 04 owns server-side. External-URI glTF (loose `.bin`/textures)
//! isn't resolvable from bytes alone and returns `Err`, which the wrapper shows as "preview
//! unavailable".
//!
//! One flattened buffer (rather than per-primitive draws) keeps the draw loop trivial and is plenty
//! for an inspector-sized preview; per-material batching is a later optimisation shared with
//! `dam-render`.

use glam::{Mat3, Mat4, Vec3};

use crate::camera::Aabb;
use crate::scene::Vertex;

pub struct CpuMesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub bounds: Aabb,
}

/// Decode self-contained glTF or GLB bytes (embedded / data-URI buffers). Walks the default scene
/// (or scene 0), applies each node's world transform, and concatenates every primitive into one
/// indexed mesh. Loose glTF with *external* buffers goes through [`load_external`] instead.
pub fn load(bytes: &[u8]) -> Result<CpuMesh, String> {
    let (doc, buffers, _images) =
        gltf::import_slice(bytes).map_err(|e| format!("glTF decode failed: {e}"))?;
    build_mesh(&doc, &buffers)
}

/// Decode a loose glTF whose buffers live in *external* files (issue #56). The DOM does the
/// networking: it parses the glTF JSON, resolves each buffer's URI against the asset's directory
/// (data-URIs decoded client-side, external files fetched via `/assets/{id}/related`), and passes the
/// resolved bytes here in **buffer-index order**. This crate never touches the network.
pub fn load_external(json: &[u8], mut external: Vec<Vec<u8>>) -> Result<CpuMesh, String> {
    let gltf = gltf::Gltf::from_slice(json).map_err(|e| format!("glTF parse failed: {e}"))?;
    let blob = gltf.blob.clone();
    let doc = gltf.document;

    let mut buffers: Vec<gltf::buffer::Data> = Vec::with_capacity(doc.buffers().count());
    for buffer in doc.buffers() {
        let data = match buffer.source() {
            // A GLB binary chunk (unusual for loose glTF, but handle it): use the parsed blob.
            gltf::buffer::Source::Bin => blob.clone().ok_or_else(|| {
                "glTF references a binary chunk but none was supplied".to_string()
            })?,
            // Every URI buffer (external file *or* data-URI) was resolved to bytes by the DOM.
            gltf::buffer::Source::Uri(uri) => external
                .get_mut(buffer.index())
                .map(std::mem::take)
                .filter(|b| !b.is_empty())
                .ok_or_else(|| format!("missing external buffer #{} ({uri})", buffer.index()))?,
        };
        buffers.push(gltf::buffer::Data(data));
    }

    build_mesh(&doc, &buffers)
}

/// Shared mesh assembly: walk the default scene and flatten every primitive into one indexed mesh.
fn build_mesh(doc: &gltf::Document, buffers: &[gltf::buffer::Data]) -> Result<CpuMesh, String> {
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut bounds = Aabb::empty();

    let scene = doc
        .default_scene()
        .or_else(|| doc.scenes().next())
        .ok_or_else(|| "glTF has no scenes".to_string())?;

    for node in scene.nodes() {
        walk(
            &node,
            Mat4::IDENTITY,
            buffers,
            &mut vertices,
            &mut indices,
            &mut bounds,
        );
    }

    if vertices.is_empty() {
        return Err("glTF contained no drawable geometry".to_string());
    }

    Ok(CpuMesh {
        vertices,
        indices,
        bounds: bounds.or_unit(),
    })
}

fn walk(
    node: &gltf::Node,
    parent: Mat4,
    buffers: &[gltf::buffer::Data],
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    bounds: &mut Aabb,
) {
    let local = Mat4::from_cols_array_2d(&node.transform().matrix());
    let world = parent * local;
    // Normal matrix = inverse-transpose of the upper-left 3×3 (handles non-uniform scale).
    let normal_mat = Mat3::from_mat4(world).inverse().transpose();

    if let Some(mesh) = node.mesh() {
        for prim in mesh.primitives() {
            add_primitive(&prim, world, normal_mat, buffers, vertices, indices, bounds);
        }
    }

    for child in node.children() {
        walk(&child, world, buffers, vertices, indices, bounds);
    }
}

fn add_primitive(
    prim: &gltf::Primitive,
    world: Mat4,
    normal_mat: Mat3,
    buffers: &[gltf::buffer::Data],
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    bounds: &mut Aabb,
) {
    // We only draw triangles; skip lines/points (rare in asset previews).
    if prim.mode() != gltf::mesh::Mode::Triangles {
        return;
    }

    let reader = prim.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));

    let positions: Vec<[f32; 3]> = match reader.read_positions() {
        Some(p) => p.collect(),
        None => return,
    };

    let base_start = vertices.len() as u32;

    // Base colour (rgb) from the PBR material; alpha ignored for the opaque preview.
    let bc = prim.material().pbr_metallic_roughness().base_color_factor();
    let color = [bc[0], bc[1], bc[2]];

    // Normals: use provided; else compute flat per-triangle below (left zero for now).
    let normals: Option<Vec<[f32; 3]>> = reader.read_normals().map(|n| n.collect());

    for (i, p) in positions.iter().enumerate() {
        let wp = world.transform_point3(Vec3::from_array(*p));
        bounds.expand(wp);
        let n = match &normals {
            Some(ns) => (normal_mat * Vec3::from_array(ns[i])).normalize_or_zero(),
            None => Vec3::ZERO,
        };
        vertices.push(Vertex {
            pos: wp.to_array(),
            normal: n.to_array(),
            color,
        });
    }

    // Indices: provided or sequential.
    let local_indices: Vec<u32> = match reader.read_indices() {
        Some(idx) => idx.into_u32().collect(),
        None => (0..positions.len() as u32).collect(),
    };

    // Flat normals when the mesh shipped none — one normal per face, applied to its three verts.
    if normals.is_none() {
        for tri in local_indices.chunks_exact(3) {
            let a = Vec3::from_array(vertices[(base_start + tri[0]) as usize].pos);
            let b = Vec3::from_array(vertices[(base_start + tri[1]) as usize].pos);
            let c = Vec3::from_array(vertices[(base_start + tri[2]) as usize].pos);
            let fn_ = (b - a).cross(c - a).normalize_or_zero();
            for &vi in tri {
                vertices[(base_start + vi) as usize].normal = fn_.to_array();
            }
        }
    }

    indices.extend(local_indices.iter().map(|i| base_start + i));
}
