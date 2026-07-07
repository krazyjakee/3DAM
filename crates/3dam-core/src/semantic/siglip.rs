//! Real candle-backed SigLIP encoder (semantic-search M4) — the model behind the [`SemanticModel`]
//! seam for images (and, later, 3D via multi-view render). SigLIP base/patch16-224, 768-d, Apache-2.0,
//! candle-native — the v1 pick from `spikes/embedding-models/findings-image.md` under ADR 0006.
//!
//! Its sigmoid-loss contrastive training puts the **image and text towers in one shared space**, so a
//! query string ([`encode_text`]) and an image ([`encode_asset`]) embed comparably and cosine-rank
//! together — the capability the model-free `*-stats-v1` descriptors structurally cannot provide.
//!
//! Weights are loaded from `<data_dir>/models/siglip/` (`model.safetensors` + `tokenizer.json`) — no
//! cloud, no runtime download (PRODUCT_SPEC §2 on-device). Absent weights ⇒ [`load`] returns `None`
//! and the engine falls back to the model-free path.
//!
//! [`encode_text`]: SemanticModel::encode_text
//! [`encode_asset`]: SemanticModel::encode_asset

use super::{semantic_space_id, SemanticModel, SIGLIP_MODEL};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::siglip;
use dam_api::dto::MediaType;
use std::path::Path;
use std::sync::Mutex;
use tokenizers::Tokenizer;

/// SigLIP base/patch16-224 fixed parameters (must match the shipped checkpoint).
const DIM: usize = 768;
const IMAGE_SIZE: usize = 224;
const SEQ_LEN: usize = 64; // text `max_position_embeddings`; SigLIP pads to a fixed length
const VERSION: u32 = 1;

/// A loaded SigLIP model. candle `Model`s are not `Sync` (interior tensors), so the model + tokenizer
/// sit behind a `Mutex`; inference is CPU-bound and already runs on the analysis lane's blocking
/// threads, so the serialization is not a hot-path concern in v1.
pub struct SiglipModel {
    inner: Mutex<Inner>,
    device: Device,
    pad_id: u32,
}

struct Inner {
    model: siglip::Model,
    tokenizer: Tokenizer,
}

/// Try to load SigLIP from `<data_dir>/models/siglip/`. `None` (with a log) if the directory or a
/// required file is missing, or the model fails to build — enabling the feature is always safe.
pub fn load(data_dir: &Path) -> Option<Box<dyn SemanticModel>> {
    let dir = data_dir.join("models").join("siglip");
    let weights = first_existing(&dir, &["model.safetensors", "pytorch_model.safetensors"])?;
    let tok_path = dir.join("tokenizer.json");
    if !tok_path.exists() {
        tracing::info!(dir = %dir.display(), "SigLIP tokenizer.json missing — semantic model disabled");
        return None;
    }
    match SiglipModel::build(&weights, &tok_path) {
        Ok(m) => {
            tracing::info!(dir = %dir.display(), "SigLIP semantic model loaded (M4)");
            Some(Box::new(m))
        }
        Err(e) => {
            tracing::warn!(error = %e, "SigLIP failed to load — semantic model disabled");
            None
        }
    }
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<std::path::PathBuf> {
    let hit = names.iter().map(|n| dir.join(n)).find(|p| p.exists());
    if hit.is_none() {
        tracing::info!(dir = %dir.display(), "no SigLIP weights (model.safetensors) — semantic model disabled");
    }
    hit
}

impl SiglipModel {
    fn build(weights: &Path, tokenizer: &Path) -> candle_core::Result<Self> {
        let device = Device::Cpu;
        let cfg = siglip::Config::base_patch16_224();
        // SAFETY: mmap of a read-only weights file we opened; standard candle loading path.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights.to_path_buf()], DType::F32, &device)?
        };
        let model = siglip::Model::new(&cfg, vb)?;
        let tokenizer = Tokenizer::from_file(tokenizer)
            .map_err(|e| candle_core::Error::Msg(format!("tokenizer: {e}")))?;
        let pad_id = cfg.text_config.pad_token_id;
        Ok(Self {
            inner: Mutex::new(Inner { model, tokenizer }),
            device,
            pad_id,
        })
    }

    /// Tokenize + right-pad/truncate to the fixed `SEQ_LEN` SigLIP expects.
    fn token_ids(&self, inner: &Inner, text: &str) -> candle_core::Result<Vec<u32>> {
        let enc = inner
            .tokenizer
            .encode(text, true)
            .map_err(|e| candle_core::Error::Msg(format!("encode: {e}")))?;
        let mut ids = enc.get_ids().to_vec();
        ids.resize(SEQ_LEN, self.pad_id); // pad (or truncate) to exactly SEQ_LEN
        ids.truncate(SEQ_LEN);
        Ok(ids)
    }
}

impl SemanticModel for SiglipModel {
    fn space_id(&self, media: MediaType) -> String {
        // Image and 3D-shape share SigLIP weights but get distinct spaces (never cross-ranked):
        // single-image vs. pooled multi-view vectors are not comparable (spike §cross-cutting).
        semantic_space_id(SIGLIP_MODEL, VERSION, media, DIM)
    }

    fn encode_text(&self, media: MediaType, text: &str) -> Option<Vec<f32>> {
        // SigLIP's text tower serves the image + shape spaces; audio has its own model (CLAP).
        if !matches!(media, MediaType::Image | MediaType::Model) {
            return None;
        }
        let inner = self.inner.lock().ok()?;
        let ids = self.token_ids(&inner, text).ok()?;
        let input = Tensor::new(ids.as_slice(), &self.device)
            .ok()?
            .unsqueeze(0)
            .ok()?; // [1, SEQ_LEN]
        let feats = inner.model.get_text_features(&input).ok()?;
        tensor_to_normalised_vec(&feats)
    }

    fn encode_asset(&self, media: MediaType, path: &Path) -> Option<Vec<f32>> {
        // v1: images only. 3D goes through the multi-view render path (M4 follow-up); audio → CLAP.
        if media != MediaType::Image {
            return None;
        }
        let pixels = load_image_tensor(path, &self.device).ok()?;
        let inner = self.inner.lock().ok()?;
        let feats = inner.model.get_image_features(&pixels).ok()?;
        tensor_to_normalised_vec(&feats)
    }
}

/// Decode an image, resize to 224², and build the `[1, 3, 224, 224]` tensor SigLIP expects, scaled
/// to `[-1, 1]` (SigLIP preprocessing: `pixel/127.5 - 1`).
fn load_image_tensor(path: &Path, device: &Device) -> candle_core::Result<Tensor> {
    let img = image::ImageReader::open(path)
        .map_err(|e| candle_core::Error::Msg(format!("open image: {e}")))?
        .decode()
        .map_err(|e| candle_core::Error::Msg(format!("decode image: {e}")))?
        .resize_exact(
            IMAGE_SIZE as u32,
            IMAGE_SIZE as u32,
            image::imageops::FilterType::Triangle,
        )
        .to_rgb8();
    // HWC u8 → CHW f32 in [-1, 1].
    let mut data = vec![0f32; 3 * IMAGE_SIZE * IMAGE_SIZE];
    for (x, y, px) in img.enumerate_pixels() {
        let (x, y) = (x as usize, y as usize);
        for c in 0..3 {
            data[c * IMAGE_SIZE * IMAGE_SIZE + y * IMAGE_SIZE + x] = px[c] as f32 / 127.5 - 1.0;
        }
    }
    Tensor::from_vec(data, (1, 3, IMAGE_SIZE, IMAGE_SIZE), device)
}

/// Flatten a `[1, DIM]` feature tensor to an L2-normalised `Vec<f32>` (the stored embedding form).
fn tensor_to_normalised_vec(t: &Tensor) -> Option<Vec<f32>> {
    let mut v: Vec<f32> = t.flatten_all().ok()?.to_vec1().ok()?;
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut v {
            *x /= norm;
        }
    }
    Some(v)
}
