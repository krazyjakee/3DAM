//! 3D model cheap-tier metadata (tech-spec 04 §4.1/§5, `../3d-handler-notes.md`).
//!
//! Everything here is the **CHEAP tier**: it walks container headers / the glTF JSON chunk / a
//! bounded text or binary header scan and never decodes geometry buffers or touches the GPU. glTF
//! and GLB read exact accessor `count`s from JSON (no BIN decode); OBJ/PLY/STL read counts from a
//! header or a bounded scan. Counts are marked approximate where a format cannot give them cheaply
//! (ADR 0009 §8 — "geometry counts approximate when accessor counts are absent").
//!
//! Issue #49 closed the gap that left FBX, Collada and 3D Studio with no geometry facts at all —
//! only a texture-dependency byte total. The received wisdom is that those need a full importer,
//! and for geometry they do; for *counts* they do not. All three are self-describing enough to walk
//! structurally: a binary FBX record carries its own end offset and states array lengths in the
//! clear ([`fbx`]), a `.3ds` chunk states its own byte length and puts element counts first
//! ([`tds`]), and Collada declares every count as an XML attribute ([`dae`]). Each lives in its own
//! submodule because each is a real container walk rather than a few lines of header poke.
//!
//! [`probe`] is the deliberate exception and is **not** part of this tier: a full Assimp import,
//! feature-gated, for the analyse pass — see its module docs for what it buys and why it stays out
//! of scan.

mod dae;
mod deps;
mod fbx;
mod gltf;
mod obj;
mod ply;
mod stl;
mod tds;

// Exact counts via a full Assimp import — the analyse/deep tier, behind the same feature as the 3D
// convert path because both build Assimp from source.
#[cfg(feature = "model-convert")]
mod probe;

use dam_api::dto::{MediaAttributes, MediaType, ModelAttributes};
#[cfg(test)]
use deps::asset_base;
use deps::dependency_bytes;
use std::path::Path;

pub(crate) struct Handler;
pub(crate) static HANDLER: Handler = Handler;

impl crate::MediaHandler for Handler {
    fn media_type(&self) -> MediaType {
        MediaType::Model
    }

    fn detect(&self, path: &Path) -> Option<crate::FormatId> {
        let ext = crate::ext(path)?;
        let format = match ext.as_str() {
            "gltf" => "gltf",
            "glb" => "glb",
            "fbx" => "fbx",
            "obj" => "obj",
            "stl" => "stl",
            "ply" => "ply",
            "dae" => "dae",
            "3ds" => "3ds",
            "blend" => "blend",
            "usd" => "usd",
            "usdz" => "usdz",
            "usda" => "usda",
            "usdc" => "usdc",
            _ => return None,
        };
        Some(crate::FormatId {
            media: MediaType::Model,
            format,
            confidence: crate::Confidence::ExtensionOnly,
        })
    }

    fn extract_metadata(&self, path: &Path, format: &str) -> MediaAttributes {
        MediaAttributes::Model(metadata(path, format))
    }
}

/// Extract cheap model attributes; best-effort, fail-soft (a parse fault yields whatever was read).
pub fn metadata(path: &Path, format: &str) -> ModelAttributes {
    let attrs = match format {
        "glb" => gltf::glb(path),
        "gltf" => gltf::gltf(path),
        "obj" => obj::metadata(path),
        "stl" => stl::metadata(path),
        "ply" => ply::metadata(path),
        "fbx" => fbx::metadata(path),
        "dae" => dae::metadata(path),
        "3ds" => tds::metadata(path),
        // `blend`/`usd*` have no structure this tier can walk — a `.blend` is a memory image of
        // Blender's own DNA and USD is a whole layered scene description. They stay catalogued and
        // browsable with no geometry facts; the feature-gated deep probe answers for `.blend`.
        _ => None,
    };
    let mut attrs = attrs.unwrap_or_default();
    // The reported size of a model should reflect the whole asset — its external textures and
    // buffers, not just the mesh container — so a 130 KB `.fbx` with 14 MB of maps reads honestly.
    attrs.dependency_bytes = dependency_bytes(path, format);
    attrs
}

/// DEEP tier: exact geometry counts from a full Assimp import (issue #49), for the analyse pass.
///
/// The cheap tier above is honest but approximates in two places it cannot avoid — a compressed FBX
/// index array and a `<vcount>`-less Collada polygon list both publish element counts without
/// publishing how those elements group into polygons — and it has nothing at all to say about
/// `.blend`. This settles both, at the cost of actually decoding the geometry, which is why it is
/// not what a scan calls.
///
/// `Err(Unsupported)` when the build has no Assimp (`model-convert` off), so a caller can tell
/// "this build can't" from "this file won't" and fall back to the cheap answer either way. The
/// external-dependency byte total is carried over from the cheap tier: it is a fact about the
/// folder the model sits in, not about the imported scene.
#[cfg(feature = "model-convert")]
pub fn deep_metadata(path: &Path, format: &str) -> Result<ModelAttributes, crate::HandlerError> {
    let mut attrs = probe::metadata(path)?;
    attrs.dependency_bytes = dependency_bytes(path, format);
    Ok(attrs)
}

#[cfg(not(feature = "model-convert"))]
pub fn deep_metadata(_path: &Path, _format: &str) -> Result<ModelAttributes, crate::HandlerError> {
    Err(crate::HandlerError::Unsupported(
        "exact 3D geometry counts are not compiled into this build (feature `model-convert`)"
            .into(),
    ))
}

fn nonzero(n: i64) -> Option<i64> {
    (n > 0).then_some(n)
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

    // ── geometry counts, per format (issue #49) ──────────────────────────────
    //
    // Every fixture below is synthesised inline, byte for byte, rather than committed as a binary:
    // a hand-built container is the only kind whose expected counts are *derivable from the test
    // source*, so a failure says which field is wrong instead of "the blob disagrees".

    fn attrs(dir: &tempfile::TempDir, name: &str, format: &str, bytes: &[u8]) -> ModelAttributes {
        let p = dir.path().join(name);
        write(&p, bytes);
        metadata(&p, format)
    }

    /// One binary-FBX node record. FBX records state their own *absolute* end offset, so encoding
    /// is bottom-up from a known base — which is also why this mirrors the parser's assumptions
    /// closely enough to be a real test of them.
    struct Rec {
        name: &'static str,
        props: Vec<u8>,
        num_props: u32,
        kids: Vec<Rec>,
    }

    impl Rec {
        fn new(name: &'static str) -> Self {
            Rec {
                name,
                props: Vec::new(),
                num_props: 0,
                kids: Vec::new(),
            }
        }

        /// Append an array property: type code, element count, encoding (0 = raw), payload length.
        fn array(mut self, kind: u8, len: u32, encoding: u32, payload: &[u8]) -> Self {
            self.props.push(kind);
            self.props.extend_from_slice(&len.to_le_bytes());
            self.props.extend_from_slice(&encoding.to_le_bytes());
            self.props
                .extend_from_slice(&(payload.len() as u32).to_le_bytes());
            self.props.extend_from_slice(payload);
            self.num_props += 1;
            self
        }

        fn kid(mut self, k: Rec) -> Self {
            self.kids.push(k);
            self
        }

        fn encode(&self, base: u64) -> Vec<u8> {
            let header = 13 + self.name.len() as u64; // 3 offset words + the name-length byte
            let mut body = self.props.clone();
            let mut end = base + header + self.props.len() as u64;
            for k in &self.kids {
                let b = k.encode(end);
                end += b.len() as u64;
                body.extend_from_slice(&b);
            }
            if !self.kids.is_empty() {
                body.extend_from_slice(&[0u8; 13]); // the sentinel that closes a nested list
                end += 13;
            }
            let mut out = (end as u32).to_le_bytes().to_vec();
            out.extend_from_slice(&self.num_props.to_le_bytes());
            out.extend_from_slice(&(self.props.len() as u32).to_le_bytes());
            out.push(self.name.len() as u8);
            out.extend_from_slice(self.name.as_bytes());
            out.extend_from_slice(&body);
            out
        }
    }

    fn binary_fbx(roots: Vec<Rec>) -> Vec<u8> {
        let mut out = b"Kaydara FBX Binary  \x00".to_vec();
        out.extend_from_slice(&[0x1A, 0x00]);
        out.extend_from_slice(&7400u32.to_le_bytes()); // < 7500 → 32-bit record offsets
        let mut off = out.len() as u64;
        for r in &roots {
            let b = r.encode(off);
            off += b.len() as u64;
            out.extend_from_slice(&b);
        }
        out.extend_from_slice(&[0u8; 13]);
        out
    }

    fn i32_array(values: &[i32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// A binary FBX of one quad: 4 shared vertices, one 4-gon that fans into 2 triangles. The index
    /// array is uncompressed, which is the case where the polygon boundaries (`~i`, hence negative)
    /// are readable and the triangle count is therefore **exact**.
    #[test]
    fn a_binary_fbx_yields_geometry_counts() {
        let geometry = Rec::new("Geometry")
            .kid(Rec::new("Vertices").array(b'd', 12, 0, &[0u8; 96]))
            .kid(Rec::new("PolygonVertexIndex").array(b'i', 4, 0, &i32_array(&[0, 1, 2, -4])))
            .kid(Rec::new("LayerElementUV"));
        let objects = Rec::new("Objects")
            .kid(geometry)
            .kid(Rec::new("Material"))
            .kid(Rec::new("Texture"))
            .kid(Rec::new("Deformer"))
            .kid(Rec::new("AnimationCurve"));
        // A sibling of `Objects` that must be skipped whole rather than descended into — this is
        // the property that keeps the walk O(nodes visited) on a real file, where `Connections`
        // dwarfs everything else.
        let doc = binary_fbx(vec![Rec::new("Connections"), objects]);

        let dir = tempfile::tempdir().unwrap();
        let m = attrs(&dir, "quad.fbx", "fbx", &doc);
        assert_eq!(m.vertex_count, Some(4), "12 float components / 3");
        assert_eq!(
            m.triangle_count,
            Some(2),
            "one quad fans into two triangles"
        );
        assert_eq!(m.mesh_count, Some(1));
        assert_eq!(m.material_count, Some(1));
        assert_eq!(m.texture_count, Some(1));
        assert_eq!(m.has_rig, Some(true), "a Deformer means it is skinned");
        assert_eq!(m.has_animation, Some(true));
        assert_eq!(m.has_uvs, Some(true));
    }

    /// The compressed case, which is what a real exporter writes: the array header still states its
    /// element count, but the polygon boundaries are inside the deflate payload. The count then
    /// falls back to assuming triangles — right for game-ready meshes, low for quads, and
    /// deliberately not worth inflating megabytes at scan time to sharpen.
    #[test]
    fn a_compressed_fbx_index_array_falls_back_to_the_triangulated_estimate() {
        let geometry = Rec::new("Geometry")
            .kid(Rec::new("Vertices").array(b'd', 27, 0, &[0u8; 216]))
            // Encoding 1 = deflate; the payload is opaque here and must never be read.
            .kid(Rec::new("PolygonVertexIndex").array(b'i', 9, 1, &[0xEE; 20]));
        let doc = binary_fbx(vec![Rec::new("Objects").kid(geometry)]);

        let dir = tempfile::tempdir().unwrap();
        let m = attrs(&dir, "packed.fbx", "fbx", &doc);
        assert_eq!(m.vertex_count, Some(9));
        assert_eq!(m.triangle_count, Some(3), "9 indices / 3");
    }

    /// The text form declares the same element counts inline, so a bounded line scan answers it.
    #[test]
    fn an_ascii_fbx_yields_geometry_counts() {
        let doc = concat!(
            "; FBX 7.4.0 project file\n",
            "Objects:  {\n",
            "\tGeometry: 12345, \"Geometry::mesh\", \"Mesh\" {\n",
            "\t\tVertices: *12 {\n",
            "\t\t\ta: 0,0,0,1,0,0,1,1,0,0,1,0\n",
            "\t\t}\n",
            "\t\tPolygonVertexIndex: *6 {\n",
            "\t\t\ta: 0,1,2,0,2,-4\n",
            "\t\t}\n",
            "\t\tLayerElementUV: 0 {\n",
            "\t\t}\n",
            "\t}\n",
            "\tMaterial: 999, \"Material::mat\", \"\" {\n",
            "\t}\n",
            "}\n",
        );
        let dir = tempfile::tempdir().unwrap();
        let m = attrs(&dir, "text.fbx", "fbx", doc.as_bytes());
        assert_eq!(m.vertex_count, Some(4));
        assert_eq!(m.triangle_count, Some(2));
        assert_eq!(m.mesh_count, Some(1));
        assert_eq!(m.material_count, Some(1));
        assert_eq!(m.has_uvs, Some(true));
        assert_eq!(m.has_rig, Some(false));
    }

    /// A Collada mesh: positions are reached through the `<vertices>` → `<source>` reference (not
    /// by taking the first array in the mesh, which would pick up whichever source came first), and
    /// the polygon list's `<vcount>` makes the triangle count exact.
    #[test]
    fn a_collada_document_yields_geometry_counts() {
        let doc = r##"<?xml version="1.0" encoding="utf-8"?>
<COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1">
  <library_images><image id="tex"><init_from>albedo.png</init_from></image></library_images>
  <library_materials><material id="mat" name="mat"><instance_effect url="#fx"/></material></library_materials>
  <library_geometries>
    <geometry id="g">
      <mesh>
        <source id="g-uv"><float_array id="g-uv-a" count="8">0 0 1 0 1 1 0 1</float_array></source>
        <source id="g-pos"><float_array id="g-pos-a" count="12">0 0 0 1 0 0 1 1 0 0 1 0</float_array></source>
        <vertices id="g-v"><input semantic="POSITION" source="#g-pos"/></vertices>
        <polylist material="mat" count="1">
          <input semantic="VERTEX" source="#g-v" offset="0"/>
          <input semantic="TEXCOORD" source="#g-uv" offset="1"/>
          <vcount>4</vcount>
          <p>0 0 1 1 2 2 3 3</p>
        </polylist>
      </mesh>
    </geometry>
  </library_geometries>
  <library_animations><animation id="anim"/></library_animations>
</COLLADA>"##;
        let dir = tempfile::tempdir().unwrap();
        let m = attrs(&dir, "scene.dae", "dae", doc.as_bytes());
        assert_eq!(
            m.vertex_count,
            Some(4),
            "the POSITION source, not the UV source that precedes it"
        );
        assert_eq!(m.triangle_count, Some(2), "one 4-gon via <vcount>");
        assert_eq!(m.mesh_count, Some(1));
        assert_eq!(m.material_count, Some(1), "<instance_material> is not one");
        assert_eq!(m.texture_count, Some(1));
        assert_eq!(m.has_uvs, Some(true));
        assert_eq!(m.has_animation, Some(true));
        assert_eq!(m.has_rig, Some(false));
    }

    fn tds_chunk(id: u16, body: &[u8]) -> Vec<u8> {
        let mut v = id.to_le_bytes().to_vec();
        v.extend_from_slice(&((body.len() + 6) as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// A 3D Studio chunk tree. Counts are exact here: `.3ds` stores triangles, and both lists put
    /// their element count in the first two bytes, ahead of data the walk then skips by length.
    #[test]
    fn a_3ds_chunk_tree_yields_geometry_counts() {
        let mut vertices = 4u16.to_le_bytes().to_vec();
        vertices.extend(std::iter::repeat_n(0u8, 4 * 12));
        let mut faces = 2u16.to_le_bytes().to_vec();
        faces.extend(std::iter::repeat_n(0u8, 2 * 8));
        let mut uvs = 4u16.to_le_bytes().to_vec();
        uvs.extend(std::iter::repeat_n(0u8, 4 * 8));

        let mut trimesh = tds_chunk(0x4110, &vertices);
        trimesh.extend(tds_chunk(0x4120, &faces));
        trimesh.extend(tds_chunk(0x4140, &uvs));

        // A named object puts a NUL-terminated name before its sub-chunks — miss it and every
        // offset below is wrong, which is exactly what this fixture pins.
        let mut object = b"Box\0".to_vec();
        object.extend(tds_chunk(0x4100, &trimesh));

        let mut edit = tds_chunk(0xAFFF, b"mat\0");
        edit.extend(tds_chunk(0x4000, &object));

        let mut main = tds_chunk(0x3D3D, &edit);
        main.extend(tds_chunk(0xB000, &[])); // keyframer → animated
        let doc = tds_chunk(0x4D4D, &main);

        let dir = tempfile::tempdir().unwrap();
        let m = attrs(&dir, "box.3ds", "3ds", &doc);
        assert_eq!(m.vertex_count, Some(4));
        assert_eq!(m.triangle_count, Some(2));
        assert_eq!(m.mesh_count, Some(1));
        assert_eq!(m.material_count, Some(1));
        assert_eq!(m.has_uvs, Some(true));
        assert_eq!(m.has_animation, Some(true));
        assert_eq!(m.has_rig, Some(false), ".3ds has no skinning concept");
    }

    /// Binary STL, including the classic trap: the 80-byte comment is free-form and a binary file
    /// is perfectly entitled to open with the word `solid`, which is the ASCII marker. The declared
    /// triangle count reconciling with the file length is what settles it.
    #[test]
    fn a_binary_stl_reads_its_triangle_count_header() {
        let binary_stl = |comment: &[u8], tris: u32| {
            let mut v = vec![0u8; 80];
            v[..comment.len()].copy_from_slice(comment);
            v.extend_from_slice(&tris.to_le_bytes());
            v.extend(std::iter::repeat_n(0u8, 50 * tris as usize));
            v
        };
        let dir = tempfile::tempdir().unwrap();

        let m = attrs(
            &dir,
            "part.stl",
            "stl",
            &binary_stl(b"exported by 3DAM", 12),
        );
        assert_eq!(m.triangle_count, Some(12));
        assert_eq!(m.vertex_count, Some(36), "STL never shares a vertex");
        assert_eq!(m.mesh_count, Some(1));

        let m = attrs(
            &dir,
            "liar.stl",
            "stl",
            &binary_stl(b"solid exported by 3DAM", 7),
        );
        assert_eq!(
            m.triangle_count,
            Some(7),
            "the size check must beat the `solid` prefix"
        );
    }

    /// PLY declares its element counts in an ASCII header even when the body is binary, so the
    /// reader must stop at `end_header` and never touch what follows.
    #[test]
    fn a_ply_reads_its_element_counts() {
        let dir = tempfile::tempdir().unwrap();
        let ascii = "ply\nformat ascii 1.0\ncomment made by 3DAM\nelement vertex 8\n\
             property float x\nproperty float y\nproperty float z\n\
             element face 12\nproperty list uchar int vertex_indices\nend_header\n\
             0 0 0\n0 0 1\n";
        let m = attrs(&dir, "cube.ply", "ply", ascii.as_bytes());
        assert_eq!(m.vertex_count, Some(8));
        assert_eq!(m.triangle_count, Some(12));

        let mut binary = b"ply\nformat binary_little_endian 1.0\nelement vertex 3\n\
             property float x\nelement face 1\n\
             property list uchar int vertex_indices\nend_header\n"
            .to_vec();
        binary.extend_from_slice(&[0xFF; 64]); // opaque body the header scan must not reach into
        let m = attrs(&dir, "tri.ply", "ply", &binary);
        assert_eq!(m.vertex_count, Some(3));
        assert_eq!(m.triangle_count, Some(1));
    }

    /// Fail-soft, per golden rule 6: a corrupt or mistyped file of any of these formats is a
    /// per-item miss — empty attributes, no panic, no hang — never something that could sink a
    /// scan. Every case here is a shape that reaches a different guard: a truncated container, a
    /// header whose offsets point past the end, a length that swallows the file, and a file that is
    /// simply something else wearing the extension.
    #[test]
    fn malformed_models_degrade_to_empty_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let empty = ModelAttributes::default();

        let mut broken_fbx = b"Kaydara FBX Binary  \x00\x1A\x00".to_vec();
        broken_fbx.extend_from_slice(&7400u32.to_le_bytes());
        broken_fbx.extend_from_slice(&[0xFF; 64]); // every record offset now points past the file
        let cases: Vec<(&str, &str, Vec<u8>)> = vec![
            ("truncated.fbx", "fbx", b"Kaydara FBX".to_vec()),
            ("offsets.fbx", "fbx", broken_fbx),
            ("prose.fbx", "fbx", b"this is a note, not a model".to_vec()),
            (
                "notxml.dae",
                "dae",
                b"<html><body>hello</body></html>".to_vec(),
            ),
            ("truncated.dae", "dae", b"<COLL".to_vec()),
            ("wrongroot.3ds", "3ds", tds_chunk(0x1234, b"nope")),
            // A chunk that claims to be longer than the file it sits in.
            ("toolong.3ds", "3ds", {
                let mut v = 0x4D4Du16.to_le_bytes().to_vec();
                v.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                v
            }),
            ("stub.stl", "stl", b"\x00\x01\x02".to_vec()),
            ("notply.ply", "ply", b"PLY? no.\n".to_vec()),
            ("bad.gltf", "gltf", b"{ not json".to_vec()),
            ("bad.glb", "glb", b"glTFxxxxxxxx".to_vec()),
        ];
        for (name, format, bytes) in cases {
            let m = attrs(&dir, name, format, &bytes);
            assert_eq!(
                (m.vertex_count, m.triangle_count, m.mesh_count),
                (empty.vertex_count, empty.triangle_count, empty.mesh_count),
                "{name} should degrade to empty attributes, got {m:?}"
            );
        }
    }
}
