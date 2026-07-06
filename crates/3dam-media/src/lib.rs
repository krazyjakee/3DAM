//! `dam-media` — format detection and the media-handler layer (tech-spec 04).
//!
//! Phase 2 (Media depth) fills in the cost-tiered contract (04 §4): **detection** stays cheap
//! (extension-ordered, with a couple of content sniffs), the **cheap tier** ([`extract_metadata`])
//! reads container/headers only, and the **expensive tier** ([`render_thumbnail`], the `convert`
//! encoders) fully decodes and is only ever called on preview/convert — never at ingest scale.
//! The audio/3D interactive previews are WASM islands (`dam-viewer`); only images produce a
//! server-rendered thumbnail here.

mod audio;
mod features;
mod image;
mod model;

pub use features::{extract_image_features, l2_normalise, ImageFeatures};

use dam_api::dto::{MediaAttributes, MediaType};
use std::path::Path;

/// What detection yields for a file: its media class and a concrete format tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detected {
    pub media: MediaType,
    /// Concrete format, lowercase (`wav`, `png`, `gltf`, …).
    pub format: String,
}

/// A per-asset handler fault. Always fail-soft at the call site (tech-spec 04 §6): the orchestrator
/// records it against the one asset and moves on; it never aborts a scan or a convert batch.
#[derive(Debug)]
pub enum HandlerError {
    /// Recognised format, but a sub-feature/codec this build does not support.
    Unsupported(String),
    /// Structurally invalid container/chunk.
    Corrupt(String),
    /// Failed to encode a derivative/output.
    Encode(String),
    Io(std::io::Error),
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandlerError::Unsupported(s) => write!(f, "unsupported: {s}"),
            HandlerError::Corrupt(s) => write!(f, "corrupt: {s}"),
            HandlerError::Encode(s) => write!(f, "encode failed: {s}"),
            HandlerError::Io(e) => write!(f, "io: {e}"),
        }
    }
}
impl std::error::Error for HandlerError {}

/// A generated raster preview derivative (tech-spec 04 §6.4). PNG-encoded so the DOM shows it via
/// a plain `<img>`; the cache layer (02) stores `bytes` keyed by content hash.
#[derive(Clone, Debug)]
pub struct ThumbPng {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Lowercased final extension of a path, if any.
fn ext(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// The extension → (media, format) table for detection. Kept in one place so the format-coverage
/// matrix (tech-spec 04 §7) has a single source of truth.
pub fn detect(path: &Path) -> Option<Detected> {
    let e = ext(path)?;
    let media = match e.as_str() {
        // audio
        "wav" | "flac" | "mp3" | "ogg" | "oga" | "opus" | "aiff" | "aif" | "m4a" | "aac"
        | "wma" | "it" | "xm" | "mod" | "s3m" => MediaType::Audio,
        // image
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tga" | "tiff" | "tif" | "webp" | "psd"
        | "exr" | "hdr" | "dds" | "ktx" | "ktx2" | "svg" => MediaType::Image,
        // 3d models
        "gltf" | "glb" | "fbx" | "obj" | "stl" | "ply" | "dae" | "3ds" | "blend" | "usd"
        | "usdz" | "usda" | "usdc" => MediaType::Model,
        _ => return None,
    };
    // Normalise a few aliases to a canonical format tag.
    let format = match e.as_str() {
        "jpeg" => "jpg",
        "tif" => "tiff",
        "aif" => "aiff",
        "oga" => "ogg",
        other => other,
    }
    .to_string();
    Some(Detected { media, format })
}

/// CHEAP tier (tech-spec 04 §4): read the media-specific attribute struct from container headers /
/// metadata only — no geometry decode, no PCM decode, no full pixel decode, no GPU. Runs on every
/// asset at ingest. Best-effort and fail-soft: a partial or empty struct is returned on fault,
/// never an error that would sink the scan.
pub fn extract_metadata(path: &Path, det: &Detected) -> MediaAttributes {
    match det.media {
        MediaType::Audio => MediaAttributes::Audio(audio::metadata(path, &det.format)),
        MediaType::Image => MediaAttributes::Image(image::metadata(path, &det.format)),
        MediaType::Model => MediaAttributes::Model(model::metadata(path, &det.format)),
    }
}

/// EXPENSIVE tier: render a downscaled PNG thumbnail (tech-spec 04 §6.4). Only images produce one
/// in v1 — audio waveforms and 3D turntables are interactive WASM islands, so this returns
/// `Unsupported` for them and the UI falls back to the honest typed tile.
pub fn render_thumbnail(
    path: &Path,
    det: &Detected,
    max_edge: u32,
) -> Result<ThumbPng, HandlerError> {
    match det.media {
        MediaType::Image => {
            let (bytes, width, height) = image::thumbnail(path, max_edge)?;
            Ok(ThumbPng {
                bytes,
                width,
                height,
            })
        }
        MediaType::Audio | MediaType::Model => Err(HandlerError::Unsupported(format!(
            "{} previews render client-side (WASM island), not as a server thumbnail",
            det.media.as_str()
        ))),
    }
}

/// EXPENSIVE tier: decode + re-encode an image to a raster target (convert pipeline, tech-spec 08
/// §3.2). Returns the encoded bytes.
pub fn convert_image(
    path: &Path,
    target_format: &str,
    max_edge: Option<u32>,
    quality: Option<u8>,
) -> Result<Vec<u8>, HandlerError> {
    image::convert(path, target_format, max_edge, quality)
}

/// EXPENSIVE tier: decode audio and encode a canonical 16-bit WAV (convert pipeline, tech-spec 08
/// §3.1). Returns the encoded bytes.
pub fn convert_audio(
    path: &Path,
    source_format: &str,
    target_format: &str,
) -> Result<Vec<u8>, HandlerError> {
    if target_format != "wav" {
        return Err(HandlerError::Unsupported(format!(
            "audio target {target_format:?} is not supported in this build (v1 encodes WAV)"
        )));
    }
    let mut cursor = std::io::Cursor::new(Vec::new());
    audio::convert_to_wav(path, source_format, &mut cursor)?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::dto::MediaAttributes;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn tmp(name: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("3dam-media-test-{}-{n}-{name}", std::process::id()))
    }

    fn det(media: MediaType, format: &str) -> Detected {
        Detected {
            media,
            format: format.to_string(),
        }
    }

    fn write_png(path: &Path, w: u32, h: u32) {
        // `::image` — the crate, not this crate's `mod image` which shadows the bare name here.
        let img = ::image::RgbaImage::from_pixel(w, h, ::image::Rgba([10, 20, 30, 255]));
        img.save_with_format(path, ::image::ImageFormat::Png)
            .unwrap();
    }

    #[test]
    fn image_metadata_reads_dimensions_and_alpha() {
        let p = tmp("dims.png");
        write_png(&p, 64, 48);
        let attrs = extract_metadata(&p, &det(MediaType::Image, "png"));
        let MediaAttributes::Image(i) = attrs else {
            panic!("expected image attrs")
        };
        assert_eq!(i.width, Some(64));
        assert_eq!(i.height, Some(48));
        assert_eq!(i.has_alpha, Some(true)); // RGBA PNG → colour type 6
        assert_eq!(i.color_space.as_deref(), Some("srgb"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn image_thumbnail_and_convert() {
        let p = tmp("src.png");
        write_png(&p, 200, 100);
        let thumb = render_thumbnail(&p, &det(MediaType::Image, "png"), 64).unwrap();
        assert!(thumb.width <= 64 && thumb.height <= 64);
        assert!(thumb.bytes.starts_with(b"\x89PNG"), "PNG-encoded");
        // Aspect preserved: 200x100 → long edge 64 → 64x32.
        assert_eq!((thumb.width, thumb.height), (64, 32));

        let jpg = convert_image(&p, "jpg", Some(50), Some(80)).unwrap();
        assert!(jpg.starts_with(&[0xFF, 0xD8]), "JPEG SOI marker");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn gltf_json_counts_and_flags() {
        // One mesh, one primitive: POSITION accessor count 3, indices accessor count 3 (mode
        // default TRIANGLES) → 3 verts, 1 triangle. TEXCOORD_0 present → has_uvs.
        let doc = r#"{
          "meshes":[{"primitives":[{"attributes":{"POSITION":0,"TEXCOORD_0":1},"indices":2}]}],
          "accessors":[{"count":3},{"count":3},{"count":3}],
          "materials":[{}],
          "textures":[{}],
          "skins":[{}],
          "animations":[{}]
        }"#;
        let p = tmp("m.gltf");
        std::fs::write(&p, doc).unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&p, &det(MediaType::Model, "gltf")) else {
            panic!("expected model attrs")
        };
        assert_eq!(m.vertex_count, Some(3));
        assert_eq!(m.triangle_count, Some(1));
        assert_eq!(m.mesh_count, Some(1));
        assert_eq!(m.material_count, Some(1));
        assert_eq!(m.texture_count, Some(1));
        assert_eq!(m.has_rig, Some(true));
        assert_eq!(m.has_animation, Some(true));
        assert_eq!(m.has_uvs, Some(true));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn glb_container_json_chunk() {
        let mut json =
            br#"{"meshes":[{"primitives":[{"attributes":{"POSITION":0},"indices":1}]}],"accessors":[{"count":24},{"count":36}]}"#
                .to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' '); // chunks are 4-byte aligned
        }
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes()); // version
        glb.extend_from_slice(&((12 + 8 + json.len()) as u32).to_le_bytes()); // total length
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes()); // chunk length
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        let p = tmp("m.glb");
        std::fs::write(&p, &glb).unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&p, &det(MediaType::Model, "glb")) else {
            panic!("expected model attrs")
        };
        assert_eq!(m.vertex_count, Some(24));
        assert_eq!(m.triangle_count, Some(12)); // 36 indices / 3
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn obj_stl_ply_counts() {
        let obj = tmp("c.obj");
        std::fs::write(&obj, "v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nf 1 2 3\n").unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&obj, &det(MediaType::Model, "obj"))
        else {
            panic!()
        };
        assert_eq!(m.vertex_count, Some(3));
        assert_eq!(m.triangle_count, Some(1));
        assert_eq!(m.has_uvs, Some(true));

        let stl = tmp("c.stl");
        std::fs::write(
            &stl,
            "solid t\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nendloop\nendfacet\nendsolid t\n",
        )
        .unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&stl, &det(MediaType::Model, "stl"))
        else {
            panic!()
        };
        assert_eq!(m.triangle_count, Some(1));

        let ply = tmp("c.ply");
        std::fs::write(
            &ply,
            "ply\nformat ascii 1.0\nelement vertex 8\nproperty float x\nelement face 12\nproperty list uchar int vertex_index\nend_header\n",
        )
        .unwrap();
        let MediaAttributes::Model(m) = extract_metadata(&ply, &det(MediaType::Model, "ply"))
        else {
            panic!()
        };
        assert_eq!(m.vertex_count, Some(8));
        assert_eq!(m.triangle_count, Some(12));
        std::fs::remove_file(&obj).ok();
        std::fs::remove_file(&stl).ok();
        std::fs::remove_file(&ply).ok();
    }

    #[test]
    fn audio_metadata_and_wav_convert() {
        // Synthesize a 1-second mono 8 kHz WAV with hound, then probe it back.
        let p = tmp("tone.wav");
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 8000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut w = hound::WavWriter::create(&p, spec).unwrap();
            for _ in 0..8000 {
                w.write_sample(0i16).unwrap();
            }
            w.finalize().unwrap();
        }
        let MediaAttributes::Audio(a) = extract_metadata(&p, &det(MediaType::Audio, "wav")) else {
            panic!("expected audio attrs")
        };
        assert_eq!(a.sample_rate, Some(8000));
        assert_eq!(a.channels, Some(1));
        assert_eq!(a.duration_ms, Some(1000));

        let wav = convert_audio(&p, "wav", "wav").unwrap();
        assert!(wav.starts_with(b"RIFF"), "WAV RIFF header");
        assert!(
            convert_audio(&p, "wav", "mp3").is_err(),
            "non-WAV target rejected"
        );
        std::fs::remove_file(&p).ok();
    }
}
