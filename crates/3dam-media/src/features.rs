//! Image feature extraction — the EXPENSIVE tier's analysis output (tech-spec 04 §4 `extract_features`,
//! consumed by the analysis pipeline in tech-spec 05). One full decode produces every image signal the
//! automation brain needs: a perceptual hash (near-dup, §4.2), the tileability metric (§6), dominant
//! colours (§5), and a model-free embedding vector for similarity (§2–§3).
//!
//! **Model-free embedding.** v1 ships without the SigLIP/DINOv2 weights (ADR 0006: models are optional,
//! fetched on first use, and the default build must run offline with no native dep). So the embedding
//! here is a deterministic perceptual descriptor — a mean-centred, L2-normalised low-resolution
//! luminance thumbnail — tagged as the `image-stats-v1` space. It gives real visual-similarity ranking
//! for game textures today; when the SigLIP path lands it is a `model_version` bump behind the same
//! `Embedder` seam (tech-spec 05 §2.1, §7), not a rewrite.

use image::{DynamicImage, GenericImageView, ImageReader};
use std::path::Path;

use crate::HandlerError;

/// Every derived image signal from a single decode (tech-spec 05 §5, §6, §2–§3).
#[derive(Clone, Debug)]
pub struct ImageFeatures {
    /// 64-bit dHash (difference hash) — the perceptual near-dup signal (§4.2).
    pub phash: u64,
    /// Edge-continuity tileability score in [0,1] (§6.2).
    pub tileability: f32,
    /// Detected internal repeat period (source px) if the image already tiles (§6.3).
    pub repeat_period: Option<u32>,
    /// `seamless` | `tiled` | `non_tiling` (§6.4).
    pub tile_class: &'static str,
    /// Dominant colours, most-prominent first, as `#rrggbb`.
    pub dominant_colors: Vec<String>,
    /// Coarse content guess (`texture` | `photo` | `sprite`).
    pub class: &'static str,
    /// L2-normalised embedding vector for the `image-stats-v1` space (§2.1).
    pub embedding: Vec<f32>,
}

/// The embedding side length: a 16×16 luminance grid → a 256-dim descriptor. Small enough to store
/// and brute-force cosine cheaply; large enough to separate visually distinct textures.
const EMBED_EDGE: u32 = 16;
/// Longest edge for the tileability working copy (§6.1) — enough for seam/periodicity, cheap.
const TILE_EDGE: u32 = 256;
/// `seamless` cutoff on the edge-continuity score (§6.4); conservative v1 default (§8, tuned later).
const SEAMLESS_THRESHOLD: f32 = 0.86;
/// Autocorrelation peak-prominence threshold `tau` for the periodicity pass (§6.3); v1 default.
const REPEAT_TAU: f32 = 0.55;

/// Decode `path` once and derive every image signal. EXPENSIVE — only called by the analysis pass,
/// never at ingest scale. Fail-soft is the caller's job: a decode error returns `Corrupt` and the
/// asset simply keeps its cheap-tier metadata with no embedding (tech-spec 05 §2.3).
pub fn extract_image_features(path: &Path) -> Result<ImageFeatures, HandlerError> {
    let img = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(HandlerError::Io)?
        .decode()
        .map_err(|e| HandlerError::Corrupt(e.to_string()))?;

    let phash = dhash(&img);
    let embedding = embed(&img);
    let dominant_colors = dominant_colors(&img);
    let (tileability, repeat_period, tile_class) = tileability(&img);
    let class = classify(&img, tileability, &dominant_colors);

    Ok(ImageFeatures {
        phash,
        tileability,
        repeat_period,
        tile_class,
        dominant_colors,
        class,
        embedding,
    })
}

/// dHash: resize to 9×8 luminance, then each bit is "is this pixel brighter than the one to its
/// right?" — 8×8 = 64 comparisons. Robust to scaling/mild re-encoding; Hamming distance is the
/// near-dup metric (tech-spec 05 §4.2).
fn dhash(img: &DynamicImage) -> u64 {
    let small = img.resize_exact(9, 8, image::imageops::FilterType::Triangle);
    let lum = small.to_luma8();
    let mut hash = 0u64;
    let mut bit = 0;
    for y in 0..8u32 {
        for x in 0..8u32 {
            let l = lum.get_pixel(x, y).0[0];
            let r = lum.get_pixel(x + 1, y).0[0];
            if l > r {
                hash |= 1 << bit;
            }
            bit += 1;
        }
    }
    hash
}

/// Model-free embedding: a 16×16 luminance grid, mean-centred and L2-normalised so cosine similarity
/// is a dot product (tech-spec 05 §2.1). Mean-centring makes it invariant to overall brightness.
fn embed(img: &DynamicImage) -> Vec<f32> {
    let small = img.resize_exact(EMBED_EDGE, EMBED_EDGE, image::imageops::FilterType::Triangle);
    let lum = small.to_luma8();
    let mut v: Vec<f32> = lum.pixels().map(|p| p.0[0] as f32).collect();
    let mean = v.iter().sum::<f32>() / v.len() as f32;
    for x in v.iter_mut() {
        *x -= mean;
    }
    l2_normalise(&mut v);
    v
}

/// L2-normalise in place; a zero (flat image) vector is left as zeros — cosine against it is 0,
/// which is the honest "no signal" answer.
fn l2_normalise(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Dominant colours by coarse 3-bit-per-channel quantisation over a downscaled copy, ranked by
/// frequency (tech-spec 05 §5). Returns up to 4 `#rrggbb` swatches, most-common first.
fn dominant_colors(img: &DynamicImage) -> Vec<String> {
    let small = img
        .resize(48, 48, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let mut buckets: std::collections::HashMap<u16, (u64, [u32; 3])> = std::collections::HashMap::new();
    for p in small.pixels() {
        let [r, g, b] = p.0;
        let key = ((r as u16 >> 5) << 6) | ((g as u16 >> 5) << 3) | (b as u16 >> 5);
        let e = buckets.entry(key).or_insert((0, [0, 0, 0]));
        e.0 += 1;
        e.1[0] += r as u32;
        e.1[1] += g as u32;
        e.1[2] += b as u32;
    }
    let mut ranked: Vec<(u64, [u32; 3])> = buckets.into_values().collect();
    ranked.sort_by_key(|b| std::cmp::Reverse(b.0));
    ranked
        .into_iter()
        .take(4)
        .map(|(count, sum)| {
            let r = (sum[0] / count as u32) as u8;
            let g = (sum[1] / count as u32) as u8;
            let b = (sum[2] / count as u32) as u8;
            format!("#{r:02x}{g:02x}{b:02x}")
        })
        .collect()
}

/// The tileability metric (tech-spec 05 §6): edge-continuity score, optional repeat period, class.
/// Works in linearised luminance on a downscaled copy.
fn tileability(img: &DynamicImage) -> (f32, Option<u32>, &'static str) {
    let src_long = img.width().max(img.height());
    let work = if src_long > TILE_EDGE {
        img.resize(TILE_EDGE, TILE_EDGE, image::imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let (w, h) = (work.width(), work.height());
    if w < 2 || h < 2 {
        return (0.0, None, "non_tiling");
    }
    // Linearised luminance plane (undo sRGB gamma so seam/gradient math is physical — §6.1).
    let lum = work.to_luma8();
    let lin: Vec<f32> = lum.pixels().map(|p| srgb_to_linear(p.0[0])).collect();
    let at = |x: u32, y: u32| lin[(y * w + x) as usize];

    // Seam discontinuity: opposite edges as if tiled (§6.2).
    let seam_lr = (0..h).map(|y| (at(0, y) - at(w - 1, y)).abs()).sum::<f32>() / h as f32;
    let seam_tb = (0..w).map(|x| (at(x, 0) - at(x, h - 1)).abs()).sum::<f32>() / w as f32;
    let seam = 0.5 * (seam_lr + seam_tb);

    // Internal reference: neighbour-to-neighbour variation over the interior.
    let mut internal_sum = 0.0f32;
    let mut internal_n = 0u32;
    for y in 0..h {
        for x in 0..w - 1 {
            internal_sum += (at(x, y) - at(x + 1, y)).abs();
            internal_n += 1;
        }
    }
    for y in 0..h - 1 {
        for x in 0..w {
            internal_sum += (at(x, y) - at(x, y + 1)).abs();
            internal_n += 1;
        }
    }
    let internal = if internal_n > 0 {
        internal_sum / internal_n as f32
    } else {
        0.0
    };

    // Normalise the seam against the internal gradient; map ratio→score (§6.2). k=1 linear v1.
    let ratio = seam / (internal + 1e-4);
    let score = (1.0 - (ratio - 1.0)).clamp(0.0, 1.0);

    let repeat_period = repeat_period(&lin, w, h, src_long);
    let class = if repeat_period.is_some() {
        "tiled"
    } else if score >= SEAMLESS_THRESHOLD {
        "seamless"
    } else {
        "non_tiling"
    };
    (score, repeat_period, class)
}

/// Periodicity pass (§6.3): 1-D autocorrelation of the per-axis mean projection; the strongest
/// non-trivial peak, if prominent enough, is the repeat period (rescaled to source pixels).
fn repeat_period(lin: &[f32], w: u32, h: u32, src_long: u32) -> Option<u32> {
    let col_proj: Vec<f32> = (0..w)
        .map(|x| (0..h).map(|y| lin[(y * w + x) as usize]).sum::<f32>() / h as f32)
        .collect();
    let row_proj: Vec<f32> = (0..h)
        .map(|y| (0..w).map(|x| lin[(y * w + x) as usize]).sum::<f32>() / w as f32)
        .collect();
    let work_long = w.max(h) as f32;
    let scale = src_long as f32 / work_long; // work-px → source-px
    axis_period(&col_proj)
        .or_else(|| axis_period(&row_proj))
        .map(|lag| (lag as f32 * scale).round() as u32)
}

/// The strongest prominent autocorrelation peak of a 1-D signal, or None if it looks non-repeating.
fn axis_period(signal: &[f32]) -> Option<u32> {
    let n = signal.len();
    if n < 8 {
        return None;
    }
    let mean = signal.iter().sum::<f32>() / n as f32;
    let centred: Vec<f32> = signal.iter().map(|x| x - mean).collect();
    let denom = centred.iter().map(|x| x * x).sum::<f32>();
    if denom < 1e-6 {
        return None; // flat: no periodicity
    }
    let max_lag = n / 2;
    let mut best = (0usize, 0.0f32);
    for lag in 2..max_lag {
        let mut acc = 0.0f32;
        for i in 0..(n - lag) {
            acc += centred[i] * centred[i + lag];
        }
        let corr = acc / denom;
        if corr > best.1 {
            best = (lag, corr);
        }
    }
    if best.1 >= REPEAT_TAU {
        Some(best.0 as u32)
    } else {
        None
    }
}

/// Coarse content class (tech-spec 05 §5): a square, high-tileability image reads as a `texture`;
/// an image with alpha and few colours as a `sprite`; otherwise `photo`.
fn classify(img: &DynamicImage, tileability: f32, dominant: &[String]) -> &'static str {
    let (w, h) = img.dimensions();
    let squarish = {
        let (a, b) = (w.min(h) as f32, w.max(h) as f32);
        a / b > 0.8
    };
    let has_alpha = img.color().has_alpha();
    if squarish && tileability >= SEAMLESS_THRESHOLD {
        "texture"
    } else if has_alpha && dominant.len() <= 3 {
        "sprite"
    } else {
        "photo"
    }
}

/// sRGB 8-bit → linear float in [0,1] (standard IEC 61966-2-1 transfer).
fn srgb_to_linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("3dam-feat-{}-{name}", std::process::id()))
    }

    #[test]
    fn seamless_flat_image_scores_high_and_hashes() {
        // A solid colour tiles perfectly: seam == internal == 0 → ratio 0 → score 1.
        let p = tmp("flat.png");
        image::RgbaImage::from_pixel(128, 128, image::Rgba([80, 120, 160, 255]))
            .save(&p)
            .unwrap();
        let f = extract_image_features(&p).unwrap();
        assert!(f.tileability > 0.9, "flat image is seamless: {}", f.tileability);
        assert_eq!(f.tile_class, "seamless");
        assert_eq!(f.embedding.len(), (EMBED_EDGE * EMBED_EDGE) as usize);
        assert!(!f.dominant_colors.is_empty());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn hard_seam_scores_low() {
        // Left half black, right half white: the L–R seam is a huge discontinuity vs a flat interior.
        let mut img = image::RgbaImage::new(128, 128);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            let v = if x < 64 { 0 } else { 255 };
            *px = image::Rgba([v, v, v, 255]);
        }
        let p = tmp("seam.png");
        img.save(&p).unwrap();
        let f = extract_image_features(&p).unwrap();
        assert!(f.tileability < 0.5, "visible seam scores low: {}", f.tileability);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn identical_images_hash_identically() {
        let p1 = tmp("a.png");
        let p2 = tmp("b.png");
        let mut img = image::RgbaImage::new(64, 64);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x * 4) as u8, (y * 4) as u8, 90, 255]);
        }
        img.save(&p1).unwrap();
        img.save(&p2).unwrap();
        let a = extract_image_features(&p1).unwrap();
        let b = extract_image_features(&p2).unwrap();
        assert_eq!(a.phash, b.phash, "same pixels → same dHash");
        // Cosine of identical normalised embeddings ≈ 1.
        let dot: f32 = a.embedding.iter().zip(&b.embedding).map(|(x, y)| x * y).sum();
        assert!(dot > 0.99, "identical embeddings cosine ~1: {dot}");
        std::fs::remove_file(&p1).ok();
        std::fs::remove_file(&p2).ok();
    }
}
