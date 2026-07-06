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

/// Decode glTF or GLB bytes. Walks the default scene (or scene 0), applies each node's world
/// transform, and concatenates every primitive into one indexed mesh.
pub fn load(bytes: &[u8]) -> Result<CpuMesh, String> {
    let (doc, buffers, _images) =
        gltf::import_slice(bytes).map_err(|e| format!("glTF decode failed: {e}"))?;

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
            &buffers,
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
