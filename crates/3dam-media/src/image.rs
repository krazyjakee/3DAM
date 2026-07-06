//! Image cheap-tier metadata + server-rendered thumbnail (tech-spec 04 §5, §6.4, §7.2).
//!
//! `metadata` reads dimensions from the header only (the `image` crate decodes just enough for
//! `into_dimensions`) plus a tiny PNG IHDR sniff for alpha/bit-depth — no full pixel decode
//! (the CHEAP contract, §4). `thumbnail` is the one place a full decode happens, and only on
//! demand (preview open), producing a downscaled PNG the DOM shows via `<img>`.

use dam_api::dto::ImageAttributes;
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader};
use std::io::Cursor;
use std::path::Path;

use crate::HandlerError;

/// Cheap header read: dimensions always; alpha/bit-depth/colour-space where the container gives
/// them without a pixel decode.
pub fn metadata(path: &Path, format: &str) -> ImageAttributes {
    let mut attrs = ImageAttributes::default();

    if let Ok(reader) = ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
        if let Ok((w, h)) = reader.into_dimensions() {
            attrs.width = Some(w as i64);
            attrs.height = Some(h as i64);
        }
    }

    // PNG's IHDR (bytes 16..26) carries bit depth + colour type cheaply; sniff it directly so we
    // can report alpha honestly (capability ≠ presence for other formats, so we leave those None).
    if format == "png" {
        if let Some((depth, alpha)) = png_ihdr(path) {
            attrs.color_depth = Some(depth as i64);
            attrs.has_alpha = Some(alpha);
        }
    }

    // The v1 raster matrix (PNG/JPEG/WebP/TGA/BMP/GIF/TIFF) is sRGB-encoded by default; the linear
    // analysis that matters for normal maps is phase-3 (05), not a cheap header field.
    attrs.color_space = Some("srgb".to_string());
    attrs
}

/// Read PNG bit-depth and alpha from the IHDR chunk without decoding pixels. Colour types 4/6
/// (grey+alpha / RGBA) and any `tRNS` chunk imply alpha; we detect the former cheaply here.
fn png_ihdr(path: &Path) -> Option<(u8, bool)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 26];
    f.read_exact(&mut buf).ok()?;
    // Signature (8) + IHDR length+type (8) then: width(4) height(4) bit_depth(1) color_type(1).
    if &buf[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let bit_depth = buf[24];
    let color_type = buf[25];
    let alpha = matches!(color_type, 4 | 6);
    Some((bit_depth, alpha))
}

/// Fully decode and downscale to a PNG no larger than `max_edge` on its long side (aspect
/// preserved). EXPENSIVE tier — only called on preview/thumbnail request, never at ingest.
pub fn thumbnail(path: &Path, max_edge: u32) -> Result<(Vec<u8>, u32, u32), HandlerError> {
    let reader = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(HandlerError::Io)?;
    let img = reader
        .decode()
        .map_err(|e| HandlerError::Corrupt(e.to_string()))?;
    // `thumbnail` uses a fast box filter for big reductions; `resize` with Lanczos when close.
    let thumb = img.thumbnail(max_edge, max_edge);
    let (w, h) = (thumb.width(), thumb.height());
    let mut bytes = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|e| HandlerError::Encode(e.to_string()))?;
    Ok((bytes, w, h))
}

/// Decode + optionally resize + re-encode to a target raster format (convert pipeline, tech-spec
/// 08 §3.2). `max_edge` fits within a box (aspect preserved); `quality` applies to lossy encoders.
pub fn convert(
    path: &Path,
    target_format: &str,
    max_edge: Option<u32>,
    quality: Option<u8>,
) -> Result<Vec<u8>, HandlerError> {
    let reader = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(HandlerError::Io)?;
    let mut img = reader
        .decode()
        .map_err(|e| HandlerError::Corrupt(e.to_string()))?;
    if let Some(edge) = max_edge {
        if img.width() > edge || img.height() > edge {
            img = img.resize(edge, edge, FilterType::Lanczos3);
        }
    }
    let fmt = match target_format {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::WebP,
        "bmp" => ImageFormat::Bmp,
        "tga" => ImageFormat::Tga,
        "tiff" | "tif" => ImageFormat::Tiff,
        "gif" => ImageFormat::Gif,
        other => {
            return Err(HandlerError::Unsupported(format!(
                "image target format {other:?} is not supported in this build"
            )))
        }
    };
    let mut bytes = Vec::new();
    if fmt == ImageFormat::Jpeg {
        // JPEG honours a quality knob; default to a high-quality 85 when unspecified.
        let q = quality.unwrap_or(85).clamp(1, 100);
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, q);
        enc.encode_image(&img)
            .map_err(|e| HandlerError::Encode(e.to_string()))?;
    } else {
        img.write_to(&mut Cursor::new(&mut bytes), fmt)
            .map_err(|e| HandlerError::Encode(e.to_string()))?;
    }
    Ok(bytes)
}
