//! GPU texture containers — DDS and KTX2 (issue #49; ADR 0009 §8, tech-spec 04 §5).
//!
//! These two are why a *game*-asset manager needs more than the `image` crate. A source folder is
//! full of them, and until now they were detected as images and then handed to a decoder that
//! cannot read them: catalogued with no dimensions, no thumbnail, and nothing to tell one 4 MB
//! `.dds` from another.
//!
//! ## Why not `image`'s own `dds` feature
//!
//! It exists, and it decodes DXT1/DXT3/DXT5 only — every other FourCC and DXGI format returns
//! `Unsupported`. That excludes **BC5** (two-channel, what normal maps are stored as) and **BC7**
//! (the modern default for albedo), which between them are most of a modern texture set. Turning it
//! on would have looked like support while failing on the common cases, so this uses the `dds`
//! crate instead: same org, safe Rust, and it decodes all of BC1–BC7 plus ASTC and the uncompressed
//! formats.
//!
//! ## What is deliberately not decoded
//!
//! KTX2 is two problems stacked — a container, and a payload that may be **supercompressed**
//! (Basis/ETC1S/UASTC, Zstandard, or ZLIB). The container is read here always, because the `ktx2`
//! crate parses headers without touching payload bytes and so can never fail on an exotic one.
//!
//! The *payload* is decoded for: uncompressed `R8G8B8A8`/`B8G8R8A8` (UNORM and SRGB), and the
//! block formats **BC1–BC7**. Everything else — **ASTC**, **ETC2/EAC**, and any
//! **supercompressed** level (Basis/ETC1S/UASTC, Zstd, ZLIB) — gets full metadata and no
//! thumbnail. That is the "metadata-before-decoder" staging ADR 0009 §8 keeps open, and it is the
//! honest split: ASTC/ETC2 need block decoders this module does not yet wire up, and Basis needs a
//! transcoder whose only pure-Rust implementation was two weeks old and single-author when this
//! landed — not a dependency to add to an asset pipeline for the sake of a preview.
//!
//! KTX v1 is a different container with different magic; it is recognised and reported as
//! undecoded rather than misreported as a corrupt KTX2.

use dam_api::dto::ImageAttributes;
use image::RgbaImage;
use std::path::Path;

use crate::HandlerError;

/// Does this extension name a GPU texture container this module owns?
///
/// `ktx` (v1) is included on purpose: it is detected as an image, and answering "known container,
/// unsupported payload" is better than handing it to a raster decoder that reports it as corrupt.
pub fn is_texture(format: &str) -> bool {
    matches!(format, "dds" | "ktx" | "ktx2")
}

/// Cheap header read. Never decodes pixels, and never reads more than the header.
///
/// Returns `None` when the file is not a readable container of that kind — the caller keeps its
/// existing fallback, so a corrupt texture degrades to a plain catalog row rather than an error.
pub fn metadata(path: &Path, format: &str) -> Option<ImageAttributes> {
    match format {
        "dds" => dds_metadata(path),
        // KTX v1 is a different container with a different magic; `ktx2`'s parser rejects it. It
        // is answered as "known but not decoded" rather than pretending to try.
        "ktx" | "ktx2" => ktx2_metadata(path),
        _ => None,
    }
}

/// Full decode of mip 0 to RGBA. EXPENSIVE tier — thumbnail/preview only, never at ingest.
pub fn decode_rgba(path: &Path, format: &str) -> Result<RgbaImage, HandlerError> {
    match format {
        "dds" => dds_decode(path),
        "ktx" | "ktx2" => {
            // A KTX v1 file is a *recognised* container we do not decode, not a corrupt KTX2.
            // Saying so is the difference between "we don't support this yet" and "your file is
            // broken", and only one of those is true.
            if is_ktx1(path) {
                return Err(HandlerError::Unsupported(
                    "KTX v1 is not decoded (KTX2 only)".into(),
                ));
            }
            ktx2_decode(path)
        }
        _ => Err(HandlerError::Unsupported(format.to_string())),
    }
}

/// KTX v1 magic — `«KTX 11»\r\n\x1A\n`, distinct from KTX2's `«KTX 20»`.
fn is_ktx1(path: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 12];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
        && buf
            == [
                0xAB, 0x4B, 0x54, 0x58, 0x20, 0x31, 0x31, 0xBB, 0x0D, 0x0A, 0x1A, 0x0A,
            ]
}

// ── DDS ─────────────────────────────────────────────────────────────────────

fn dds_metadata(path: &Path) -> Option<ImageAttributes> {
    let file = std::fs::File::open(path).ok()?;
    // `Decoder::new` reads the header and stops; it does not touch surface data.
    let decoder = dds::Decoder::new(std::io::BufReader::new(file)).ok()?;
    let header = decoder.header();
    let size = decoder.main_size();

    let mut attrs = ImageAttributes {
        width: Some(size.width as i64),
        height: Some(size.height as i64),
        ..Default::default()
    };
    attrs.texture_format = Some(format!("{:?}", decoder.format()));
    attrs.mip_levels = Some(header.mipmap_count().get() as i64);
    // From the *format*, not `alpha_mode()`. `AlphaMode` is a DX10 hint that reads `Unknown` in
    // essentially every real file (the dds crate documents that), and DX9 headers only ever set it
    // for DXT2/DXT4 — so `!= Opaque` is a constant `true`, which reported every BC5 normal map as
    // carrying alpha. The channel layout is the thing that actually knows.
    attrs.has_alpha = Some(matches!(
        decoder.native_color().channels,
        dds::Channels::Rgba | dds::Channels::Alpha
    ));
    // Colour space is only *stated* by a DX10 header. A DX9 header has no field for it, so
    // `is_srgb()` answering false there means "not recorded", not "linear" — and calling a DX9
    // albedo linear is exactly the wrong answer. Report it only when the file actually says.
    attrs.color_space = header
        .dx10()
        .map(|_| if header.is_srgb() { "srgb" } else { "linear" }.to_string());
    Some(attrs)
}

fn dds_decode(path: &Path) -> Result<RgbaImage, HandlerError> {
    let file = std::fs::File::open(path).map_err(HandlerError::Io)?;
    let mut decoder = dds::Decoder::new(std::io::BufReader::new(file))
        .map_err(|e| HandlerError::Corrupt(e.to_string()))?;
    let size = decoder.main_size();
    let (w, h) = (size.width, size.height);
    if w == 0 || h == 0 {
        return Err(HandlerError::Corrupt("texture has a zero dimension".into()));
    }
    // Guard before allocating: `w * h * 4` on a header-declared 65535x65535 is 17 GB, and the
    // header is attacker-controlled data from a scanned folder.
    let pixels = (w as u64)
        .checked_mul(h as u64)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| HandlerError::Corrupt("texture dimensions overflow".into()))?;
    if pixels > MAX_DECODE_BYTES {
        return Err(HandlerError::Unsupported(format!(
            "texture is too large to decode ({w}x{h})"
        )));
    }

    let mut buf = vec![0u8; pixels as usize];
    let view = dds::ImageViewMut::new(&mut buf, size, dds::ColorFormat::RGBA_U8)
        .ok_or_else(|| HandlerError::Corrupt("could not map the decode buffer".into()))?;
    decoder
        .read_surface(view)
        .map_err(|e| HandlerError::Corrupt(e.to_string()))?;
    RgbaImage::from_raw(w, h, buf)
        .ok_or_else(|| HandlerError::Corrupt("decoded buffer did not match its size".into()))
}

// ── KTX2 ────────────────────────────────────────────────────────────────────

/// Read just the fixed 80-byte KTX2 header. The container's level index and DFD sit after it, and
/// none of them are needed to answer "what is this texture?".
fn ktx2_header(path: &Path) -> Option<ktx2::Header> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; ktx2::Header::LENGTH];
    f.read_exact(&mut buf).ok()?;
    ktx2::Header::from_bytes(&buf).ok()
}

fn ktx2_metadata(path: &Path) -> Option<ImageAttributes> {
    let h = ktx2_header(path)?;
    let mut attrs = ImageAttributes {
        width: Some(h.pixel_width as i64),
        height: Some(h.pixel_height.max(1) as i64),
        ..Default::default()
    };
    attrs.mip_levels = Some(h.level_count.max(1) as i64);
    // `None` here is `VK_FORMAT_UNDEFINED`, which is what a Basis-supercompressed file carries:
    // its real format is decided at transcode time, so name the supercompression instead of
    // inventing a format.
    attrs.texture_format = Some(match h.format {
        Some(f) => format!("{f:?}"),
        None => match h.supercompression_scheme {
            Some(s) => format!("UNDEFINED ({s:?} supercompressed)"),
            None => "UNDEFINED".to_string(),
        },
    });
    attrs.color_space = h
        .format
        .map(|f| if is_srgb_format(f) { "srgb" } else { "linear" }.to_string());
    attrs.has_alpha = h.format.and_then(has_alpha_format);
    Some(attrs)
}

/// Whether a `VkFormat` is sRGB-encoded. The `_SRGB` suffix is the whole signal — Vulkan spells the
/// colour space into the format, which is why KTX2 can answer this and a DX9 DDS cannot.
fn is_srgb_format(f: ktx2::Format) -> bool {
    format!("{f:?}").contains("SRGB")
}

/// Whether a `VkFormat` carries alpha. `None` where the name does not settle it.
fn has_alpha_format(f: ktx2::Format) -> Option<bool> {
    let name = format!("{f:?}");
    if name.contains("A8") || name.contains("A16") || name.contains("A32") {
        return Some(true);
    }
    // BC1 has a 1-bit-alpha variant spelled RGBA; BC4/BC5 are 1-/2-channel data formats.
    if name.starts_with("BC1_RGBA") || name.starts_with("BC3") || name.starts_with("BC7") {
        return Some(true);
    }
    if name.starts_with("BC1_RGB") || name.starts_with("BC4") || name.starts_with("BC5") {
        return Some(false);
    }
    None
}

fn ktx2_decode(path: &Path) -> Result<RgbaImage, HandlerError> {
    use std::io::{Read, Seek, SeekFrom};

    // Deliberately not `ktx2::Reader`: that validates the whole container including the Data Format
    // Descriptor, and the DFD describes *channel layout* — nothing the pixel decode below consults.
    // Requiring it to parse would make a file whose payload is perfectly readable fail on a
    // descriptor we ignore. The header and the level index are fixed-layout and bounded, so they
    // are read directly, the same way the cheap tier reads the header.
    let h = ktx2_header(path)
        .ok_or_else(|| HandlerError::Corrupt("not a readable KTX2 header".into()))?;

    if let Some(scheme) = h.supercompression_scheme {
        // Metadata already landed at the cheap tier; only the preview is missing.
        return Err(HandlerError::Unsupported(format!(
            "KTX2 {scheme:?} supercompression is not decoded"
        )));
    }
    let format = h
        .format
        .ok_or_else(|| HandlerError::Unsupported("KTX2 carries no pixel format".into()))?;
    let (w, h_px) = (h.pixel_width, h.pixel_height.max(1));
    if w == 0 || h_px == 0 {
        return Err(HandlerError::Corrupt("texture has a zero dimension".into()));
    }
    let out_bytes = (w as u64)
        .checked_mul(h_px as u64)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| HandlerError::Corrupt("texture dimensions overflow".into()))?;
    if out_bytes > MAX_DECODE_BYTES {
        return Err(HandlerError::Unsupported(format!(
            "texture is too large to decode ({w}x{h_px})"
        )));
    }

    // Level index entry 0 sits immediately after the header and is the *base* level: the index is
    // ordered largest-first (level 0 … level N-1), even though the level *data* is conventionally
    // laid out smallest-first in the file for streaming — so entry 0 usually has the largest
    // `byte_offset`. Index order is what matters here, and it gives the full-size image.
    let mut f = std::fs::File::open(path).map_err(HandlerError::Io)?;
    f.seek(SeekFrom::Start(ktx2::Header::LENGTH as u64))
        .map_err(HandlerError::Io)?;
    let mut idx = [0u8; ktx2::LevelIndex::LENGTH];
    f.read_exact(&mut idx).map_err(HandlerError::Io)?;
    let level = ktx2::LevelIndex::from_bytes(&idx);

    if level.byte_length > MAX_DECODE_BYTES {
        return Err(HandlerError::Unsupported(
            "KTX2 level is too large to decode".into(),
        ));
    }
    // Bound the allocation by the file that actually exists, not only by the ceiling: a 200-byte
    // file is free to declare a 256 MB level, and believing it would allocate a quarter of a
    // gigabyte before `read_exact` discovered the lie.
    let file_len = f.metadata().map_err(HandlerError::Io)?.len();
    if level
        .byte_offset
        .checked_add(level.byte_length)
        .is_none_or(|end| end > file_len)
    {
        return Err(HandlerError::Corrupt(
            "KTX2 level extends past the end of the file".into(),
        ));
    }
    f.seek(SeekFrom::Start(level.byte_offset))
        .map_err(HandlerError::Io)?;
    let mut data = vec![0u8; level.byte_length as usize];
    f.read_exact(&mut data).map_err(HandlerError::Io)?;

    let name = format!("{format:?}");
    let (wu, hu) = (w as usize, h_px as usize);

    // Uncompressed RGBA8 is a straight copy; everything else goes through the block decoder, whose
    // output is BGRA-packed and must be swizzled.
    // Exact match, not `starts_with`: `R8G8B8A8_SNORM`/`_UINT`/`_SINT` share the prefix but are not
    // UNORM bytes, and copying them through would silently reinterpret a signed normal map.
    if matches!(name.as_str(), "R8G8B8A8_UNORM" | "R8G8B8A8_SRGB") {
        let need = wu * hu * 4;
        if data.len() < need {
            return Err(HandlerError::Corrupt("KTX2 level is short".into()));
        }
        data.truncate(need);
        return RgbaImage::from_raw(w, h_px, data)
            .ok_or_else(|| HandlerError::Corrupt("level did not match its size".into()));
    }

    if matches!(name.as_str(), "B8G8R8A8_UNORM" | "B8G8R8A8_SRGB") {
        let need = wu * hu * 4;
        if data.len() < need {
            return Err(HandlerError::Corrupt("KTX2 level is short".into()));
        }
        for px in data[..need].chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        data.truncate(need);
        return RgbaImage::from_raw(w, h_px, data)
            .ok_or_else(|| HandlerError::Corrupt("level did not match its size".into()));
    }

    let mut bgra = vec![0u32; wu * hu];
    let decoded = match name.as_str() {
        n if n.starts_with("BC1") => texture2ddecoder::decode_bc1(&data, wu, hu, &mut bgra),
        n if n.starts_with("BC2") => texture2ddecoder::decode_bc2(&data, wu, hu, &mut bgra),
        n if n.starts_with("BC3") => texture2ddecoder::decode_bc3(&data, wu, hu, &mut bgra),
        n if n.starts_with("BC4") => texture2ddecoder::decode_bc4(&data, wu, hu, &mut bgra),
        n if n.starts_with("BC5") => texture2ddecoder::decode_bc5(&data, wu, hu, &mut bgra),
        // BC6H is the HDR format; the decoder tone-maps to 8-bit, which is all a thumbnail needs.
        // Vulkan spells the signedness into the name, and decoding an SFLOAT block as unsigned
        // produces garbage rather than a wrong-but-plausible image.
        n if n.starts_with("BC6H") => {
            texture2ddecoder::decode_bc6(&data, wu, hu, &mut bgra, n.contains("SFLOAT"))
        }
        n if n.starts_with("BC7") => texture2ddecoder::decode_bc7(&data, wu, hu, &mut bgra),
        _ => return Err(HandlerError::Unsupported(format!("KTX2 {name}"))),
    };
    decoded.map_err(|e| HandlerError::Corrupt(e.to_string()))?;

    // `texture2ddecoder` packs BGRA into a little-endian u32, so the bytes land as [B,G,R,A].
    let mut out = Vec::with_capacity(bgra.len() * 4);
    for px in bgra {
        let [b, g, r, a] = px.to_le_bytes();
        out.extend_from_slice(&[r, g, b, a]);
    }
    RgbaImage::from_raw(w, h_px, out)
        .ok_or_else(|| HandlerError::Corrupt("decoded buffer did not match its size".into()))
}

/// Ceiling on a single decoded surface (256 MB of RGBA ≈ 8192×8192). Dimensions come from a header
/// in a scanned folder, so the allocation they imply is untrusted input.
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures are built byte-by-byte rather than committed, for two reasons: a binary blob in git
    /// tells a reader nothing about *why* a byte is what it is, and hand-rolling means the fixture
    /// does not depend on the very crate under test to be correct.
    fn dds_bc1_4x4() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"DDS ");
        let mut h = [0u32; 31];
        h[0] = 124; // dwSize
        h[1] = 0x1 | 0x2 | 0x4 | 0x1000 | 0x80000; // CAPS|HEIGHT|WIDTH|PIXELFORMAT|LINEARSIZE
        h[2] = 4; // height
        h[3] = 4; // width
        h[4] = 8; // linear size: one BC1 block
        h[6] = 1; // mip count
        h[18] = 32; // pf.dwSize
        h[19] = 0x4; // pf.dwFlags = FOURCC
        h[20] = u32::from_le_bytes(*b"DXT1");
        h[26] = 0x1000; // dwCaps = TEXTURE
        for w in h {
            v.extend_from_slice(&w.to_le_bytes());
        }
        // One BC1 block: c0 = red (0xF800), c1 = blue (0x001F), all indices 0 → solid c0.
        v.extend_from_slice(&0xF800u16.to_le_bytes());
        v.extend_from_slice(&0x001Fu16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v
    }

    /// A 4x4 uncompressed RGBA KTX2. The DFD is the fiddly part, so it is generated by the `ktx2`
    /// crate's own serialiser from the `VkFormat` — the surrounding offsets are laid out here.
    fn ktx2_rgba_4x4(format: ktx2::Format) -> Vec<u8> {
        // Solid orange, so a channel swap would be visible rather than symmetric.
        let mut px = Vec::new();
        for _ in 0..16 {
            px.extend_from_slice(&[255, 128, 0, 255]);
        }
        ktx2_with_level(format, &px)
    }

    /// A 4x4 KTX2 carrying `level` as its only mip level.
    fn ktx2_with_level(format: ktx2::Format, level: &[u8]) -> Vec<u8> {
        let (dfd_block, type_size) = ktx2::dfd::Basic::from_format(format).expect("dfd");
        let dfd_bytes = dfd_block.to_vec();
        let dfd_len = 4 + dfd_bytes.len() as u32; // u32 total-size field + the block

        let level_bytes = level.len() as u64;
        let dfd_offset = (ktx2::Header::LENGTH + 24) as u32; // header + one level-index entry
        let level_offset = dfd_offset as u64 + dfd_len as u64;

        let header = ktx2::Header {
            format: Some(format),
            type_size,
            pixel_width: 4,
            pixel_height: 4,
            pixel_depth: 0,
            layer_count: 0,
            face_count: 1,
            level_count: 1,
            supercompression_scheme: None,
            index: ktx2::Index {
                dfd_byte_offset: dfd_offset,
                dfd_byte_length: dfd_len,
                kvd_byte_offset: 0,
                kvd_byte_length: 0,
                sgd_byte_offset: 0,
                sgd_byte_length: 0,
            },
        };

        let mut v = Vec::new();
        v.extend_from_slice(&header.as_bytes());
        v.extend_from_slice(
            &ktx2::LevelIndex {
                byte_offset: level_offset,
                byte_length: level_bytes,
                uncompressed_byte_length: level_bytes,
            }
            .as_bytes(),
        );
        v.extend_from_slice(&dfd_len.to_le_bytes());
        v.extend_from_slice(&dfd_bytes);
        v.extend_from_slice(level);
        v
    }

    fn write(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        (dir, p)
    }

    #[test]
    fn a_dds_reports_its_dimensions_and_compression() {
        let (_d, p) = write("brick.dds", &dds_bc1_4x4());
        let a = metadata(&p, "dds").expect("a DDS header must read");
        assert_eq!((a.width, a.height), (Some(4), Some(4)));
        // The whole point of the field: without it a BC1 albedo and a BC5 normal map are the same
        // row with the same dimensions.
        assert_eq!(a.texture_format.as_deref(), Some("BC1_UNORM"));
        assert_eq!(a.mip_levels, Some(1));
        // A DX9 header has no colour-space field, so the honest answer is "not stated" — calling
        // it linear would be a guess, and the wrong one for an albedo.
        assert_eq!(
            a.color_space, None,
            "a DX9 DDS does not state a colour space"
        );
        // Alpha must come from the channel layout, not `alpha_mode()` — that hint reads `Unknown`
        // in practically every real file, so deriving from it answered "yes" for everything,
        // including BC5 normal maps.
        assert_eq!(a.has_alpha, Some(true), "BC1 decodes as an RGBA layout");
    }

    /// The reason this module exists rather than `image`'s `dds` feature: that decoder handles
    /// DXT1/3/5 only. This asserts we actually get pixels back, not an `Unsupported`.
    #[test]
    fn a_dds_decodes_to_real_pixels() {
        let (_d, p) = write("brick.dds", &dds_bc1_4x4());
        let img = decode_rgba(&p, "dds").expect("BC1 must decode");
        assert_eq!(img.dimensions(), (4, 4));
        let px = img.get_pixel(0, 0).0;
        assert!(
            px[0] > 200 && px[1] < 60 && px[2] < 60,
            "expected the block's red, got {px:?} — a channel swap would show up here"
        );
        assert_eq!(px[3], 255);
    }

    /// A DX10-header BC7 DDS — the case that decided the dependency.
    ///
    /// `image`'s own `dds` feature decodes DXT1/3/5 and returns `Unsupported` for everything else,
    /// so this file (and every BC5 normal map beside it) would have been undecodable. If this test
    /// ever starts failing because someone swapped the decoder for `image`'s, that is the reason.
    fn dds_bc7_4x4() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"DDS ");
        let mut h = [0u32; 31];
        h[0] = 124;
        h[1] = 0x1 | 0x2 | 0x4 | 0x1000 | 0x80000;
        h[2] = 4;
        h[3] = 4;
        h[4] = 16; // one BC7 block
        h[6] = 1;
        h[18] = 32;
        h[19] = 0x4;
        h[20] = u32::from_le_bytes(*b"DX10");
        h[26] = 0x1000;
        for w in h {
            v.extend_from_slice(&w.to_le_bytes());
        }
        // DDS_HEADER_DXT10: dxgiFormat=98 (BC7_UNORM), dim=3 (TEXTURE2D), misc=0, array=1, misc2=0
        for w in [98u32, 3, 0, 1, 0] {
            v.extend_from_slice(&w.to_le_bytes());
        }
        // Mode-6 BC7 block: low bit set selects mode 6, remaining bits zero — a valid, decodable
        // block. The pixel values are not asserted; that it decodes at all is the point.
        let mut block = [0u8; 16];
        block[0] = 0x40;
        v.extend_from_slice(&block);
        v
    }

    #[test]
    fn a_bc7_dds_is_recognised_and_decodes() {
        let (_d, p) = write("albedo_bc7.dds", &dds_bc7_4x4());
        let a = metadata(&p, "dds").expect("a DX10 DDS header must read");
        assert_eq!(a.texture_format.as_deref(), Some("BC7_UNORM"));
        assert_eq!((a.width, a.height), (Some(4), Some(4)));
        // A DX10 header *does* state colour space, unlike DX9 — so here we answer.
        assert_eq!(a.color_space.as_deref(), Some("linear"));

        let img = decode_rgba(&p, "dds").expect("BC7 must decode");
        assert_eq!(img.dimensions(), (4, 4));
    }

    #[test]
    fn a_ktx2_reports_its_format_and_colour_space() {
        let (_d, p) = write("albedo.ktx2", &ktx2_rgba_4x4(ktx2::Format::R8G8B8A8_SRGB));
        let a = metadata(&p, "ktx2").expect("a KTX2 header must read");
        assert_eq!((a.width, a.height), (Some(4), Some(4)));
        assert_eq!(a.texture_format.as_deref(), Some("R8G8B8A8_SRGB"));
        assert_eq!(a.mip_levels, Some(1));
        assert_eq!(a.has_alpha, Some(true));
        // Vulkan spells the colour space into the format, which is why KTX2 can answer this
        // where a DX9 DDS cannot.
        assert_eq!(a.color_space.as_deref(), Some("srgb"));

        let (_d2, p2) = write("normal.ktx2", &ktx2_rgba_4x4(ktx2::Format::R8G8B8A8_UNORM));
        let b = metadata(&p2, "ktx2").unwrap();
        assert_eq!(b.color_space.as_deref(), Some("linear"));
    }

    #[test]
    fn an_uncompressed_ktx2_decodes_to_real_pixels() {
        let (_d, p) = write("albedo.ktx2", &ktx2_rgba_4x4(ktx2::Format::R8G8B8A8_UNORM));
        let img = decode_rgba(&p, "ktx2").expect("uncompressed RGBA must decode");
        assert_eq!(img.dimensions(), (4, 4));
        // This branch is a straight copy, so orange in means orange out.
        assert_eq!(img.get_pixel(0, 0).0, [255, 128, 0, 255]);
    }

    /// The block-compressed KTX2 branch, which is a *different* path from the uncompressed one
    /// above: it runs `texture2ddecoder`, whose output is BGRA-packed, and then swizzles.
    ///
    /// This exists because the obvious test does not cover it — an `R8G8B8A8` fixture returns at
    /// the straight-copy branch and never reaches the swizzle, so the reordering could be deleted
    /// with every other test still green. Red is deliberately asymmetric: a dropped swizzle shows
    /// up here as blue.
    #[test]
    fn a_block_compressed_ktx2_decodes_without_swapping_channels() {
        // One BC1 block: c0 = red (0xF800), c1 = blue (0x001F), indices all 0 → solid c0.
        let mut block = Vec::new();
        block.extend_from_slice(&0xF800u16.to_le_bytes());
        block.extend_from_slice(&0x001Fu16.to_le_bytes());
        block.extend_from_slice(&0u32.to_le_bytes());
        let (_d, p) = write(
            "brick.ktx2",
            &ktx2_with_level(ktx2::Format::BC1_RGB_UNORM_BLOCK, &block),
        );

        let a = metadata(&p, "ktx2").expect("header");
        assert_eq!(a.texture_format.as_deref(), Some("BC1_RGB_UNORM_BLOCK"));
        assert_eq!(a.has_alpha, Some(false), "BC1_RGB carries no alpha");

        let img = decode_rgba(&p, "ktx2").expect("BC1 must decode");
        assert_eq!(img.dimensions(), (4, 4));
        let px = img.get_pixel(0, 0).0;
        assert!(
            px[0] > 200 && px[2] < 60,
            "expected red; blue here means the BGRA swizzle was dropped: {px:?}"
        );
    }

    /// Fail-soft, per the handler contract: a truncated or hostile container is a per-item answer,
    /// never a panic and never a scan abort.
    /// Guards the whole-pixel-plane decode, not just pixel (0,0): a decoder that filled only the
    /// first block, or wrote into the wrong rows, would still pass a single-pixel assertion.
    #[test]
    fn every_pixel_of_a_solid_block_decodes() {
        let (_d, p) = write("solid.dds", &dds_bc1_4x4());
        let img = decode_rgba(&p, "dds").unwrap();
        for (x, y, px) in img.enumerate_pixels() {
            assert!(
                px.0[0] > 200 && px.0[1] < 60 && px.0[2] < 60,
                "pixel ({x},{y}) is {:?}, expected the block's red",
                px.0
            );
        }
    }

    #[test]
    fn a_corrupt_container_is_refused_not_fatal() {
        let (_d, p) = write("truncated.dds", &dds_bc1_4x4()[..60]);
        assert!(metadata(&p, "dds").is_none());
        assert!(decode_rgba(&p, "dds").is_err());

        let (_d2, p2) = write("garbage.ktx2", b"not a texture at all, just bytes");
        assert!(metadata(&p2, "ktx2").is_none());
        assert!(decode_rgba(&p2, "ktx2").is_err());
    }

    /// A level index can point anywhere, including past the end of the file. Believing it would
    /// allocate whatever it declared before the read failed.
    #[test]
    fn a_level_reaching_past_the_end_of_the_file_is_refused() {
        let mut bytes = ktx2_rgba_4x4(ktx2::Format::R8G8B8A8_UNORM);
        // The level index sits right after the 80-byte header; its byte_length is the second u64.
        let len_at = ktx2::Header::LENGTH + 8;
        bytes[len_at..len_at + 8].copy_from_slice(&(200u64 * 1024 * 1024).to_le_bytes());
        let (_d, p) = write("lying.ktx2", &bytes);
        // The cheap tier still answers — it only reads the header, which is intact.
        assert!(metadata(&p, "ktx2").is_some());
        let err = decode_rgba(&p, "ktx2").expect_err("must refuse");
        assert!(
            matches!(err, HandlerError::Corrupt(_)),
            "expected a refusal before allocating, got {err:?}"
        );
    }

    /// A header can claim any size it likes; it is data from a scanned folder. Decoding must refuse
    /// rather than attempt the allocation the header asks for.
    #[test]
    fn an_absurd_declared_size_is_refused_before_allocating() {
        let mut bytes = dds_bc1_4x4();
        // 65535 x 65535 RGBA would be ~17 GB.
        bytes[12..16].copy_from_slice(&65535u32.to_le_bytes()); // height
        bytes[16..20].copy_from_slice(&65535u32.to_le_bytes()); // width
        let (_d, p) = write("huge.dds", &bytes);
        let err = decode_rgba(&p, "dds").expect_err("must refuse");
        assert!(
            matches!(err, HandlerError::Unsupported(_)),
            "expected a refusal, got {err:?}"
        );
    }
}
