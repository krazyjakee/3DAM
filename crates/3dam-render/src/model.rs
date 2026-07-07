//! Model import for turntable thumbnails (tech-spec 06 §3.1), backed by **Assimp** (via
//! `russimp-ng`, statically linked — ADR 0011). One code path decodes the full professional format
//! range — FBX, OBJ/MTL, DAE, 3DS, glTF/GLB, PLY, STL, X, LWO, … — into world-space submeshes plus
//! PBR materials (base-color / metallic-roughness / normal / emissive, factors + textures, embedded
//! or resolved from sibling files). Node transforms are baked (`PreTransformVertices`) so the
//! framing camera sees the asset as authored, including FBX axis/unit conversions.

use glam::Vec3;
use russimp_ng::material::{DataContent, Material as AiMaterial, PropertyTypeInfo, TextureType};
use russimp_ng::scene::{PostProcess, Scene};
use std::path::{Path, PathBuf};

/// Axis-aligned bounding box in world space; drives camera framing.
#[derive(Clone, Copy, Debug)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    fn empty() -> Self {
        Aabb {
            min: Vec3::splat(f32::INFINITY),
            max: Vec3::splat(f32::NEG_INFINITY),
        }
    }

    fn expand(&mut self, p: Vec3) {
        self.min = self.min.min(p);
        self.max = self.max.max(p);
    }

    fn is_valid(&self) -> bool {
        self.min.cmple(self.max).all() && self.min.is_finite() && self.max.is_finite()
    }

    pub fn center(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    /// Radius of the bounding sphere (half the diagonal). Never zero so framing math stays finite.
    pub fn radius(&self) -> f32 {
        ((self.max - self.min) * 0.5).length().max(1e-4)
    }
}

/// One interleaved GPU vertex. `tangent.w` carries the bitangent handedness for normal mapping.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub tangent: [f32; 4],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

/// A decoded RGBA8 texture ready for GPU upload.
pub struct TexImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A metallic-roughness PBR material. Textures are optional; factors apply when a map is absent.
pub struct Material {
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
    pub base_color_tex: Option<TexImage>,
    /// Packed metallic-roughness (glTF convention: G = roughness, B = metallic).
    pub mr_tex: Option<TexImage>,
    pub normal_tex: Option<TexImage>,
    pub emissive_tex: Option<TexImage>,
}

/// A run of geometry sharing one material.
pub struct SubMesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub material: usize,
}

/// A fully decoded model: world-space submeshes + their materials + overall bounds.
pub struct Model {
    pub submeshes: Vec<SubMesh>,
    pub materials: Vec<Material>,
    pub bounds: Aabb,
}

/// Load any Assimp-supported model into a [`Model`]. `Ok(None)` when the file has no renderable
/// triangle geometry (caller degrades to the typed tile).
///
/// A `.blend` is tried through Assimp first (it decodes *legacy* ≤2.7x files in-process), and only
/// on failure — a modern 2.8+ file its importer can't parse — routed through the headless Blender
/// bridge ([`crate::blend`]): export to a temporary GLB, decode that. When Blender isn't provisioned
/// the original Assimp result stands and the caller fails soft to the typed tile.
pub fn load(path: &Path) -> Result<Option<Model>, String> {
    let is_blend = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("blend"));

    match load_assimp(path) {
        Ok(Some(model)) => Ok(Some(model)),
        // Modern `.blend` defeats Assimp's legacy importer (Err) or yields no geometry (None) —
        // retry through Blender if it's available, else preserve the original fail-soft outcome.
        original if is_blend => match crate::blend::convert_to_glb(path) {
            Some(glb) => load_assimp(glb.path()),
            None => original,
        },
        original => original,
    }
}

/// Decode a model file straight through Assimp (no `.blend` bridging). See [`load`].
fn load_assimp(path: &Path) -> Result<Option<Model>, String> {
    let path_str = path.to_str().ok_or("non-UTF-8 model path")?;

    let scene = Scene::from_file(
        path_str,
        vec![
            PostProcess::Triangulate,
            // Bake node transforms → world-space meshes (handles FBX axis/unit conversion).
            PostProcess::PreTransformVertices,
            PostProcess::GenerateSmoothNormals,
            PostProcess::CalculateTangentSpace,
            PostProcess::GenerateUVCoords,
            PostProcess::FlipUVs,
            PostProcess::JoinIdenticalVertices,
            PostProcess::SortByPrimitiveType,
            PostProcess::ImproveCacheLocality,
            PostProcess::FindDegenerates,
            PostProcess::RemoveRedundantMaterials,
            PostProcess::OptimizeMeshes,
        ],
    )
    .map_err(|e| e.to_string())?;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let materials: Vec<Material> = scene
        .materials
        .iter()
        .map(|m| load_material(m, dir))
        .collect();

    let mut submeshes = Vec::new();
    let mut bounds = Aabb::empty();

    for mesh in &scene.meshes {
        if mesh.vertices.is_empty() {
            continue;
        }
        let uv0 = mesh.texture_coords.first().and_then(|o| o.as_ref());
        let col0 = mesh.colors.first().and_then(|o| o.as_ref());
        // Vertex colours multiply into albedo — but game exports routinely bake a *constant*
        // non-white colour (Unreal/Unity often emit a flat `(1,0,0)`) that carries no shading
        // information and simply tints the whole textured mesh. That's export junk, not vertex
        // paint: ignore a uniform, non-white channel so the albedo texture shows through. Genuine
        // per-vertex paint (which varies across the mesh) and a plain white channel are kept.
        let use_vcol = col0.is_some_and(|c| !is_uniform_nonwhite(c));
        let has_tangents = mesh.tangents.len() == mesh.vertices.len()
            && mesh.bitangents.len() == mesh.vertices.len();

        let mut vertices = Vec::with_capacity(mesh.vertices.len());
        for i in 0..mesh.vertices.len() {
            let p = &mesh.vertices[i];
            let pos = Vec3::new(p.x, p.y, p.z);
            bounds.expand(pos);

            let normal = mesh
                .normals
                .get(i)
                .map(|n| [n.x, n.y, n.z])
                .unwrap_or([0.0, 1.0, 0.0]);
            let uv = uv0
                .and_then(|u| u.get(i))
                .map(|u| [u.x, u.y])
                .unwrap_or([0.0, 0.0]);
            let color = col0
                .filter(|_| use_vcol)
                .and_then(|c| c.get(i))
                .map(|c| [c.r, c.g, c.b, c.a])
                .unwrap_or([1.0, 1.0, 1.0, 1.0]);
            let tangent = if has_tangents {
                let t = &mesh.tangents[i];
                let b = &mesh.bitangents[i];
                let n = Vec3::from_array(normal);
                let tv = Vec3::new(t.x, t.y, t.z);
                let bv = Vec3::new(b.x, b.y, b.z);
                let w = if n.cross(tv).dot(bv) < 0.0 { -1.0 } else { 1.0 };
                [t.x, t.y, t.z, w]
            } else {
                [1.0, 0.0, 0.0, 1.0]
            };

            vertices.push(Vertex {
                pos: pos.to_array(),
                normal,
                tangent,
                uv,
                color,
            });
        }

        let mut indices = Vec::with_capacity(mesh.faces.len() * 3);
        for f in &mesh.faces {
            // After Triangulate every face is a triangle; guard defensively anyway.
            if f.0.len() == 3 {
                indices.extend_from_slice(&f.0);
            }
        }
        if indices.is_empty() {
            continue;
        }

        submeshes.push(SubMesh {
            vertices,
            indices,
            material: mesh.material_index as usize,
        });
    }

    if submeshes.is_empty() || !bounds.is_valid() {
        return Ok(None);
    }
    Ok(Some(Model {
        submeshes,
        materials,
        bounds,
    }))
}

// ── materials ────────────────────────────────────────────────────────────────

fn load_material(m: &AiMaterial, dir: &Path) -> Material {
    // glTF/PBR uses `$clr.base`; classic formats (OBJ/FBX) fall back to the diffuse colour.
    let is_pbr = prop_vec4(m, "$clr.base").is_some();
    let base_factor = prop_vec4(m, "$clr.base")
        .or_else(|| prop_vec4(m, "$clr.diffuse"))
        .unwrap_or([1.0, 1.0, 1.0, 1.0]);
    // Non-PBR formats carry no metalness — default to dielectric. Roughness comes from the PBR
    // factor, else a legacy Phong `shininess` converted to a perceptual roughness, else moderate.
    let metallic = prop_f32(m, "$mat.metallicFactor").unwrap_or(0.0);
    let roughness = prop_f32(m, "$mat.roughnessFactor")
        .or_else(|| prop_f32(m, "$mat.shininess").map(shininess_to_roughness))
        .unwrap_or(0.6);
    let emissive = prop_vec3(m, "$clr.emissive").unwrap_or([0.0, 0.0, 0.0]);

    // Textures as the material *declares* them.
    let mut base = load_tex(m, dir, &[TextureType::BaseColor, TextureType::Diffuse]);
    let mut mr = load_tex(
        m,
        dir,
        &[
            TextureType::GltfMetallicRoughness,
            TextureType::Metalness,
            TextureType::Roughness,
        ],
    );
    let mut normal = load_tex(m, dir, &[TextureType::Normals, TextureType::Height]);
    let mut emissive_tex = load_tex(m, dir, &[TextureType::Emissive, TextureType::EmissionColor]);

    // Game-asset companion-map discovery. Many FBX/OBJ exports wire up only a diffuse/normal slot —
    // or, as with Unreal→Unity FBX, mis-wire the diffuse slot to a *mask* — while the real PBR set
    // sits alongside by the standard `<base>_<channel>` convention (`_B`/`_N`/`_ORM`/…). Anchor on
    // the base name of a texture the material already references and fill in / upgrade the slots
    // from siblings on disk, so the thumbnail reflects the authored asset, not the export's gaps.
    let mut discovered_base = false;
    if let Some((tex_dir, name)) = companion_anchor(&[&base, &normal, &mr, &emissive_tex]) {
        // Upgrade base colour when the slot is empty or holds a non-albedo map (e.g. a mask).
        if base
            .as_ref()
            .is_none_or(|r| !is_role_stem(r, BASE_SUFFIXES))
        {
            if let Some(t) = find_companion(&tex_dir, &name, BASE_SUFFIXES) {
                base = Some(t);
                discovered_base = true;
            }
        }
        // Packed occlusion-roughness-metallic (Unreal `_ORM`: G=roughness, B=metallic) maps straight
        // onto the glTF metallic-roughness slot the shader samples.
        if mr.is_none() {
            mr = find_companion(&tex_dir, &name, ORM_SUFFIXES);
        }
        if normal.is_none() {
            normal = find_companion(&tex_dir, &name, NORMAL_SUFFIXES);
        }
        if emissive_tex.is_none() {
            emissive_tex = find_companion(&tex_dir, &name, EMISSIVE_SUFFIXES);
        }
    }

    // When an albedo map drives a legacy (non-PBR) surface, a grey diffuse factor (Unreal exports
    // 0.8) would needlessly darken it — let the map speak at full value, keeping only its alpha.
    let base_color = if base.is_some() && !is_pbr && (discovered_base || base_factor[3] < 1.0) {
        [1.0, 1.0, 1.0, base_factor[3]]
    } else {
        base_factor
    };

    Material {
        base_color,
        metallic,
        roughness,
        emissive,
        base_color_tex: base.map(|r| r.img),
        mr_tex: mr.map(|r| r.img),
        normal_tex: normal.map(|r| r.img),
        emissive_tex: emissive_tex.map(|r| r.img),
    }
}

/// Convert a legacy Phong specular `shininess` exponent to a perceptual roughness in `[0,1]`
/// (the usual inverse of the Blinn-Phong→GGX mapping `shininess = 2/α² − 2`).
fn shininess_to_roughness(shininess: f32) -> f32 {
    (2.0 / (shininess.max(0.0) + 2.0)).sqrt().clamp(0.0, 1.0)
}

/// Whether a vertex-colour channel is a single, non-white colour repeated across every vertex — the
/// signature of a baked export constant rather than genuine vertex paint. Multiplying such a flat
/// tint into albedo only corrupts a textured mesh, so the caller drops it.
fn is_uniform_nonwhite(colors: &[russimp_ng::Color4D]) -> bool {
    let Some(first) = colors.first() else {
        return false;
    };
    let near_white = first.r > 0.96 && first.g > 0.96 && first.b > 0.96;
    if near_white {
        return false;
    }
    colors.iter().all(|c| {
        (c.r - first.r).abs() < 1e-3 && (c.g - first.g).abs() < 1e-3 && (c.b - first.b).abs() < 1e-3
    })
}

fn prop_floats(m: &AiMaterial, key: &str) -> Option<Vec<f32>> {
    m.properties.iter().find(|p| p.key == key).and_then(|p| {
        if let PropertyTypeInfo::FloatArray(v) = &p.data {
            Some(v.clone())
        } else {
            None
        }
    })
}

fn prop_f32(m: &AiMaterial, key: &str) -> Option<f32> {
    prop_floats(m, key).and_then(|v| v.first().copied())
}

fn prop_vec3(m: &AiMaterial, key: &str) -> Option<[f32; 3]> {
    let v = prop_floats(m, key)?;
    (v.len() >= 3).then(|| [v[0], v[1], v[2]])
}

fn prop_vec4(m: &AiMaterial, key: &str) -> Option<[f32; 4]> {
    let v = prop_floats(m, key)?;
    match v.len() {
        n if n >= 4 => Some([v[0], v[1], v[2], v[3]]),
        3 => Some([v[0], v[1], v[2], 1.0]),
        _ => None,
    }
}

/// A resolved texture plus, for on-disk files, the directory and file stem it came from — the
/// anchor companion-map discovery uses to find sibling PBR maps by naming convention. Embedded
/// textures (GLB/FBX inline) carry no anchor: a self-contained pack needs no sibling lookup.
struct ResolvedTex {
    img: TexImage,
    anchor: Option<(PathBuf, String)>,
}

/// Resolve the first available texture among `types`, trying embedded data first (FBX/GLB pack
/// textures inline) then a sibling file referenced by a `$tex.file` property.
fn load_tex(m: &AiMaterial, dir: &Path, types: &[TextureType]) -> Option<ResolvedTex> {
    for &t in types {
        if let Some(tex) = m.textures.get(&t) {
            if let Some(img) = decode_embedded(&tex.borrow()) {
                return Some(ResolvedTex { img, anchor: None });
            }
        }
        let external = m.properties.iter().find_map(|p| {
            if p.key == "$tex.file" && p.semantic == t {
                if let PropertyTypeInfo::String(s) = &p.data {
                    return Some(s.clone());
                }
            }
            None
        });
        if let Some(rel) = external {
            if let Some((img, path)) = load_external_tex(dir, &rel) {
                return Some(ResolvedTex {
                    img,
                    anchor: anchor_of(&path),
                });
            }
        }
    }
    None
}

/// Directory + file stem of a resolved texture path, for companion discovery.
fn anchor_of(path: &Path) -> Option<(PathBuf, String)> {
    let dir = path.parent()?.to_path_buf();
    let stem = path.file_stem()?.to_str()?.to_string();
    Some((dir, stem))
}

fn decode_embedded(tex: &russimp_ng::material::Texture) -> Option<TexImage> {
    match &tex.data {
        // Compressed (png/jpg/…) embedded blob — decode with the `image` crate.
        DataContent::Bytes(bytes) => decode_image_bytes(bytes),
        // Raw texels in Assimp's BGRA order → RGBA.
        DataContent::Texel(texels) => {
            let (w, h) = (tex.width, tex.height);
            // `checked_mul` on usize: a hostile texture declaring a huge w×h must fail soft, not
            // wrap in release and pass the length guard with a mismatched (w, h) vs texel count.
            let needed = (w as usize).checked_mul(h as usize);
            if w == 0 || h == 0 || needed.is_none_or(|n| texels.len() < n) {
                return None;
            }
            let mut rgba = Vec::with_capacity(texels.len() * 4);
            for t in texels {
                rgba.extend_from_slice(&[t.r, t.g, t.b, t.a]);
            }
            Some(TexImage {
                rgba,
                width: w,
                height: h,
            })
        }
    }
}

/// Sibling folders game/DCC exports conventionally keep textures in, tried by bare filename when the
/// reference path itself doesn't resolve (e.g. a baked absolute `C:\…` path from an Unreal export).
const TEXTURE_DIRS: &[&str] = &[
    "Textures",
    "textures",
    "Texture",
    "texture",
    "../Textures",
    "../textures",
    "Maps",
    "maps",
    "tex",
];

fn load_external_tex(dir: &Path, rel: &str) -> Option<(TexImage, PathBuf)> {
    // Embedded refs ("*0") never reach here; normalise Windows separators from FBX/DCC exports.
    let cleaned = rel.replace('\\', "/");
    let cleaned = cleaned.trim_start_matches("./");
    // The reference as-given (relative to the model dir) wins; then the bare filename in the model
    // dir and in the conventional sibling texture folders, salvaging absolute or stale export paths.
    let mut cands = vec![dir.join(cleaned)];
    if let Some(f) = Path::new(cleaned).file_name() {
        cands.push(dir.join(f));
        for sib in TEXTURE_DIRS {
            cands.push(dir.join(sib).join(f));
        }
    }
    for cand in cands {
        if let Ok(bytes) = std::fs::read(&cand) {
            if let Some(img) = decode_image_bytes(&bytes) {
                return Some((img, cand));
            }
        }
    }
    None
}

fn decode_image_bytes(bytes: &[u8]) -> Option<TexImage> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(TexImage {
        rgba: img.into_raw(),
        width,
        height,
    })
}

// ── companion PBR-map discovery ────────────────────────────────────────────────
//
// Trailing `<base>_<channel>` filename tokens for the standard game-asset texture set. Ordered
// most- to least-specific within each role so the best-named sibling wins. Single letters (`_b`,
// `_n`, …) come last: they are the loosest match and the likeliest false positive.

/// Albedo / base-colour maps.
const BASE_SUFFIXES: &[&str] = &[
    "basecolor",
    "albedo",
    "diffuse",
    "color",
    "colour",
    "base",
    "col",
    "bc",
    "d",
    "c",
    "b",
];
/// Tangent-space normal maps.
const NORMAL_SUFFIXES: &[&str] = &["normal", "normalmap", "nrm", "norm", "n"];
/// Packed metallic-roughness maps. `_ORM`/`_RMA` (occlusion-roughness-metallic) share the glTF
/// green=roughness / blue=metallic channel layout the shader samples, so they map straight across.
const ORM_SUFFIXES: &[&str] = &[
    "orm",
    "rma",
    "arm",
    "mroa",
    "occlusionroughnessmetallic",
    "roughnessmetallic",
    "mr",
    "rm",
];
/// Emissive maps.
const EMISSIVE_SUFFIXES: &[&str] = &["emissive", "emission", "emit", "glow", "e"];
/// Auxiliary channels we recognise only to detect the `<base>_<channel>` pattern (never applied as
/// a slot on their own): masks, ambient occlusion, split metallic/roughness, height, etc.
const AUX_SUFFIXES: &[&str] = &[
    "mask",
    "ao",
    "o",
    "occlusion",
    "metallic",
    "metalness",
    "m",
    "roughness",
    "rough",
    "r",
    "height",
    "h",
    "disp",
    "displacement",
    "opacity",
    "spec",
    "specular",
    "s",
    "gloss",
    "glossiness",
    "g",
    "bump",
];

/// Whether `s` (case-insensitive) is a recognised trailing channel token.
fn is_channel_suffix(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    [
        BASE_SUFFIXES,
        NORMAL_SUFFIXES,
        ORM_SUFFIXES,
        EMISSIVE_SUFFIXES,
        AUX_SUFFIXES,
    ]
    .iter()
    .any(|list| list.contains(&s.as_str()))
}

/// Strip a recognised `_<channel>` token off a texture stem to recover the shared asset base name
/// (`T_TelephoneTable_Mask` → `T_TelephoneTable`). `None` when the stem isn't `<base>_<channel>`.
fn strip_channel_suffix(stem: &str) -> Option<String> {
    let (base, suf) = stem.rsplit_once('_')?;
    (!base.is_empty() && is_channel_suffix(suf)).then(|| base.to_string())
}

/// Whether a resolved texture's stem ends in one of `suffixes` — i.e. it already *is* a map of that
/// role (used to tell a genuine albedo from a mis-assigned mask before overriding it).
fn is_role_stem(t: &ResolvedTex, suffixes: &[&str]) -> bool {
    let Some((_, stem)) = &t.anchor else {
        return false;
    };
    stem.rsplit_once('_')
        .is_some_and(|(_, s)| suffixes.contains(&s.to_ascii_lowercase().as_str()))
}

/// Pick a directory + asset base name to anchor companion discovery on: the first slot backed by an
/// on-disk file whose stem carries a recognised channel token.
fn companion_anchor(slots: &[&Option<ResolvedTex>]) -> Option<(PathBuf, String)> {
    slots.iter().filter_map(|s| s.as_ref()).find_map(|r| {
        let (dir, stem) = r.anchor.as_ref()?;
        Some((dir.clone(), strip_channel_suffix(stem)?))
    })
}

/// Find a sibling `<base>_<suffix>.<img-ext>` in `dir` for the highest-priority `suffixes` entry
/// that exists on disk, and decode it. One directory listing, matched case-insensitively so
/// `_B.png` resolves from a lowercase `b` token.
fn find_companion(dir: &Path, base: &str, suffixes: &[&str]) -> Option<ResolvedTex> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| has_image_ext(p))
        .collect();
    for suf in suffixes {
        let want = format!("{}_{}", base.to_ascii_lowercase(), suf);
        for p in &entries {
            let matches = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case(&want));
            if !matches {
                continue;
            }
            if let Ok(bytes) = std::fs::read(p) {
                if let Some(img) = decode_image_bytes(&bytes) {
                    return Some(ResolvedTex {
                        img,
                        anchor: anchor_of(p),
                    });
                }
            }
        }
    }
    None
}

/// Whether a path has a decodable image extension (cheap pre-filter for the directory scan).
fn has_image_ext(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "tga" | "tif" | "tiff" | "bmp" | "webp" | "gif"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UV orientation regression. Assimp normalises every format to OpenGL's lower-left UV origin,
    /// but the render/viewer pipeline uploads textures row-0-at-top and samples `v = 0` as the top
    /// texel (a top-left/glTF convention). Without [`PostProcess::FlipUVs`] every textured model
    /// renders vertically flipped — most visibly on glTF assets with asymmetric maps. The flip is
    /// verified here by pinning the glTF cube's front face back to its *authored* UVs: the fixture
    /// gives `pos(-1,-1,1) → uv(0,0)` and `pos(-1,1,1) → uv(0,1)`, and a correct decode reproduces
    /// exactly that (the flip cancels Assimp's normalisation for glTF's native top-left origin).
    #[test]
    fn gltf_uvs_match_authored_top_left_origin() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/textured_cube.gltf");
        let model = load(&path).unwrap().expect("cube has geometry");

        let uv_at = |target: [f32; 3]| -> [f32; 2] {
            model
                .submeshes
                .iter()
                .flat_map(|s| &s.vertices)
                .find(|v| {
                    Vec3::from(v.pos).distance(Vec3::from(target)) < 1e-3
                        // Front face (+Z): the seam vertices are shared by adjacent faces, so match
                        // the one whose normal faces the viewer to get that face's UV.
                        && v.normal[2] > 0.5
                })
                .map(|v| v.uv)
                .unwrap_or_else(|| panic!("no +Z vertex at {target:?}"))
        };

        let bl = uv_at([-1.0, -1.0, 1.0]);
        let tl = uv_at([-1.0, 1.0, 1.0]);
        assert!(
            bl[1] < 0.5 && tl[1] > 0.5,
            "glTF UVs are vertically flipped — expected bottom vertex v≈0, top vertex v≈1, got \
             bottom={bl:?} top={tl:?} (is PostProcess::FlipUVs still applied?)"
        );
    }
}
