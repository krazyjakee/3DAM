//! Serialise a decoded [`Model`](crate::model::Model) into the compact self-contained `DMSH` blob
//! the browser 3D island uploads directly (tech-spec 09 §B.3).
//!
//! One server-side Assimp decode feeds **both** the turntable thumbnail and the interactive viewer,
//! so every professional format renders identically and the browser never re-parses buffers or
//! textures — which is what retired the fragile loose-glTF "missing or unreadable buffers" path.
//!
//! ## Wire format (`DMSH` v1, little-endian)
//! ```text
//! magic  "DMSH" (4 B) · version u32 (=1) · bounds 6×f32 (min xyz, max xyz)
//! n_tex  u32 · per tex: len u32, png bytes[len]
//! n_mat  u32 · per mat: base_color 4×f32, metallic f32, roughness f32, emissive 3×f32,
//!             tex_base u32, tex_mr u32, tex_normal u32, tex_emissive u32   (u32::MAX = none)
//! n_sub  u32 · per sub: material u32, n_vert u32, verts[n_vert], n_idx u32, idx[n_idx] u32
//! ```
//! Vertices are [`model::Vertex`](crate::model::Vertex) verbatim (`bytemuck`-castable on both ends).
//! Textures are PNG-encoded and downscaled to [`MAX_TEX`] px on the long edge so the blob stays
//! small over the wire — a browser preview never needs an 8k material map.

use crate::model::{Model, TexImage};

/// The magic prefix the WASM parser checks before trusting the rest of the blob.
pub const MAGIC: &[u8; 4] = b"DMSH";

/// Long-edge cap for preview textures. Keeps the blob (and the browser's decode) bounded while
/// preserving enough detail for a close-up inspect — 1k was visibly soft on hero props, 2k reads
/// crisp and still compresses small over the wire.
const MAX_TEX: u32 = 2048;

/// Sentinel for "this material slot has no texture".
const NONE: u32 = u32::MAX;

/// Serialise `model` into a `DMSH` blob (see module docs). Never fails — a texture that won't
/// PNG-encode is simply dropped (its material slot becomes `NONE`), matching the render path's
/// fail-soft handling of a broken map.
pub fn serialize(model: &Model) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&1u32.to_le_bytes());

    for v in [model.bounds.min, model.bounds.max] {
        out.extend_from_slice(&v.x.to_le_bytes());
        out.extend_from_slice(&v.y.to_le_bytes());
        out.extend_from_slice(&v.z.to_le_bytes());
    }

    // Encode each material's maps into a flat texture table, remembering the per-slot indices.
    let mut textures: Vec<Vec<u8>> = Vec::new();
    let mut mat_slots: Vec<[u32; 4]> = Vec::with_capacity(model.materials.len());
    for m in &model.materials {
        mat_slots.push([
            push_tex(&mut textures, &m.base_color_tex),
            push_tex(&mut textures, &m.mr_tex),
            push_tex(&mut textures, &m.normal_tex),
            push_tex(&mut textures, &m.emissive_tex),
        ]);
    }

    out.extend_from_slice(&(textures.len() as u32).to_le_bytes());
    for png in &textures {
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(png);
    }

    out.extend_from_slice(&(model.materials.len() as u32).to_le_bytes());
    for (m, slots) in model.materials.iter().zip(&mat_slots) {
        for c in m.base_color {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out.extend_from_slice(&m.metallic.to_le_bytes());
        out.extend_from_slice(&m.roughness.to_le_bytes());
        for c in m.emissive {
            out.extend_from_slice(&c.to_le_bytes());
        }
        for s in slots {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }

    out.extend_from_slice(&(model.submeshes.len() as u32).to_le_bytes());
    for s in &model.submeshes {
        out.extend_from_slice(&(s.material as u32).to_le_bytes());
        out.extend_from_slice(&(s.vertices.len() as u32).to_le_bytes());
        out.extend_from_slice(bytemuck::cast_slice(&s.vertices));
        out.extend_from_slice(&(s.indices.len() as u32).to_le_bytes());
        out.extend_from_slice(bytemuck::cast_slice(&s.indices));
    }

    out
}

/// PNG-encode `tex` (downscaled to fit [`MAX_TEX`]), append it to `table`, and return its index —
/// or [`NONE`] when the slot is empty or the encode fails.
fn push_tex(table: &mut Vec<Vec<u8>>, tex: &Option<TexImage>) -> u32 {
    let Some(img) = tex else { return NONE };
    match encode_png(img) {
        Some(png) => {
            let idx = table.len() as u32;
            table.push(png);
            idx
        }
        None => NONE,
    }
}

/// Downscale to the preview cap then PNG-encode as RGBA8.
fn encode_png(img: &TexImage) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let clamped = clamp(img);
    let img = clamped.as_ref().unwrap_or(img);
    let (w, h) = (img.width.max(1), img.height.max(1));
    if img.rgba.len() < (w * h * 4) as usize {
        return None;
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&img.rgba, w, h, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(out)
}

/// Downscale a texture to fit within [`MAX_TEX`] on both axes (aspect-preserving); `None` when it
/// already fits so the caller encodes the original without a copy.
fn clamp(img: &TexImage) -> Option<TexImage> {
    let (w, h) = (img.width.max(1), img.height.max(1));
    if w <= MAX_TEX && h <= MAX_TEX {
        return None;
    }
    let scale = MAX_TEX as f32 / w.max(h) as f32;
    let nw = ((w as f32 * scale) as u32).clamp(1, MAX_TEX);
    let nh = ((h as f32 * scale) as u32).clamp(1, MAX_TEX);
    let src = image::RgbaImage::from_raw(w, h, img.rgba.clone())?;
    let resized = image::imageops::resize(&src, nw, nh, image::imageops::FilterType::Triangle);
    Some(TexImage {
        rgba: resized.into_raw(),
        width: nw,
        height: nh,
    })
}
