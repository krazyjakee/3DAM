//! Parse the server's self-contained `DMSH` preview blob into CPU meshes + materials + textures,
//! ready for a GPU upload.
//!
//! The blob is produced by `dam-render`'s Assimp decode (`crates/3dam-render/src/preview.rs`), so the
//! browser inherits the **full professional format range with textures** without shipping an importer
//! into WASM — and the fragile loose-glTF external-buffer path is gone (the blob is self-contained).
//! See that module for the authoritative wire layout; this is the mirror reader.

use glam::Vec3;

use crate::camera::Aabb;
use crate::scene::Vertex;

/// A decoded RGBA8 texture ready for GPU upload.
pub struct CpuTexture {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A metallic-roughness material; each `Option<usize>` indexes [`CpuModel::textures`].
pub struct CpuMaterial {
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
    pub base: Option<usize>,
    pub mr: Option<usize>,
    pub normal: Option<usize>,
    pub emissive_tex: Option<usize>,
    /// Alpha interpretation (0 = opaque, 1 = mask, 2 = blend); drives draw order + blend state.
    pub alpha_mode: u32,
    /// Mask coverage threshold (glTF `alphaCutoff`); only meaningful when `alpha_mode == 1`.
    pub alpha_cutoff: f32,
}

/// A run of geometry sharing one material (index into [`CpuModel::materials`]).
pub struct CpuSubMesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub material: usize,
}

/// A fully decoded preview: submeshes + their materials + textures + overall bounds.
pub struct CpuModel {
    pub submeshes: Vec<CpuSubMesh>,
    pub materials: Vec<CpuMaterial>,
    pub textures: Vec<CpuTexture>,
    pub bounds: Aabb,
}

/// Bounded little-endian cursor over the blob — every read is length-checked so a truncated or
/// malformed blob is a readable `Err`, never a panic.
struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.p.checked_add(n).ok_or("DMSH length overflow")?;
        let s = self
            .b
            .get(self.p..end)
            .ok_or("unexpected end of DMSH blob")?;
        self.p = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, String> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_bits(self.u32()?))
    }
}

/// A `u32::MAX` slot means "no texture for this material channel".
fn slot(v: u32) -> Option<usize> {
    (v != u32::MAX).then_some(v as usize)
}

/// Decode a `DMSH` v2 blob. `Err` (bad magic/version, truncation, undecodable texture) surfaces to
/// the wrapper as a readable "3D preview unavailable" state.
pub fn parse(bytes: &[u8]) -> Result<CpuModel, String> {
    let mut c = Cursor { b: bytes, p: 0 };
    if c.take(4)? != crate::DMSH_MAGIC {
        return Err("not a DMSH preview blob".to_string());
    }
    let version = c.u32()?;
    if version != 2 {
        return Err(format!("unsupported DMSH version {version}"));
    }

    let mut bounds = Aabb::empty();
    bounds.expand(Vec3::new(c.f32()?, c.f32()?, c.f32()?));
    bounds.expand(Vec3::new(c.f32()?, c.f32()?, c.f32()?));

    let n_tex = c.u32()?;
    let mut textures = Vec::with_capacity(n_tex as usize);
    for _ in 0..n_tex {
        let len = c.u32()? as usize;
        let png = c.take(len)?;
        let img = image::load_from_memory(png)
            .map_err(|e| format!("preview texture decode failed: {e}"))?
            .to_rgba8();
        let (width, height) = img.dimensions();
        textures.push(CpuTexture {
            rgba: img.into_raw(),
            width,
            height,
        });
    }

    let n_mat = c.u32()?;
    let mut materials = Vec::with_capacity(n_mat as usize);
    for _ in 0..n_mat {
        materials.push(CpuMaterial {
            base_color: [c.f32()?, c.f32()?, c.f32()?, c.f32()?],
            metallic: c.f32()?,
            roughness: c.f32()?,
            emissive: [c.f32()?, c.f32()?, c.f32()?],
            base: slot(c.u32()?),
            mr: slot(c.u32()?),
            normal: slot(c.u32()?),
            emissive_tex: slot(c.u32()?),
            alpha_mode: c.u32()?,
            alpha_cutoff: c.f32()?,
        });
    }

    let n_sub = c.u32()?;
    let mut submeshes = Vec::with_capacity(n_sub as usize);
    for _ in 0..n_sub {
        let material = c.u32()? as usize;
        let n_vert = c.u32()? as usize;
        let vbytes = c.take(n_vert * std::mem::size_of::<Vertex>())?;
        // `pod_collect_to_vec` copies, so it tolerates the blob's arbitrary byte alignment (a
        // PNG-length-preceded vertex run is rarely 4-aligned) — `cast_slice` would panic there.
        let vertices = bytemuck::pod_collect_to_vec::<u8, Vertex>(vbytes);
        let n_idx = c.u32()? as usize;
        let ibytes = c.take(n_idx * 4)?;
        let indices = bytemuck::pod_collect_to_vec::<u8, u32>(ibytes);
        submeshes.push(CpuSubMesh {
            vertices,
            indices,
            material,
        });
    }

    if submeshes.is_empty() {
        return Err("preview blob has no geometry".to_string());
    }

    Ok(CpuModel {
        submeshes,
        materials,
        textures,
        bounds: bounds.or_unit(),
    })
}
