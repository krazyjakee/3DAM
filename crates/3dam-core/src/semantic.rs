//! Model-backed semantic embeddings — the feature-gated bump behind the EmbeddingSpace seam
//! (semantic-search M4; tech-spec 05 §2.3, ADR 0006).
//!
//! The model-free `*-stats-v1` descriptors in [`crate::analysis`] describe *shape/stats*, not
//! *meaning*: they can tell two grey props apart by geometry but have no notion that an "ak47" is a
//! gun, and — decisively — there is no way to compare a **text query** against them. Real semantic
//! search needs a model that maps media *and* text into one **shared** vector space (SigLIP for
//! image + 3D turntable renders, CLAP for audio), so a query string and an asset embed into the same
//! space and cosine-rank together. The hybrid path (M5) rides `encode_text` for exactly this.
//!
//! **Status.** The **image + text** (candle SigLIP, [`siglip`]) and **audio** (ONNX CLAP via `ort`,
//! [`clap`]) paths are both real: `--features semantic` compiles them, and [`load`] activates each
//! when its weights are present under the data dir (`<data_dir>/models/{siglip,clap}/`), else logs
//! and returns `None` — so the default build and any weight-less run transparently fall back to the
//! model-free descriptors. A loaded model both writes its space at analyse time ([`crate::analysis`])
//! and answers text→asset queries ([`crate::EmbeddedLibrary::query`]); CLAP additionally emits
//! zero-shot content labels (music/speech/sfx, genre, mood) via [`SemanticModel::zero_shot_labels`],
//! the classification the model-free DSP tier cannot produce. A [`Composite`] dispatches per media so
//! the engine holds one model. **Remaining M4 sub-path:** 3D (multi-view render → SigLIP → mean-pool),
//! which still returns `None` from [`SemanticModel::encode_asset`] for [`MediaType::Model`].

use dam_api::dto::MediaType;
use std::path::Path;

#[cfg(feature = "semantic")]
mod clap;
#[cfg(feature = "semantic")]
mod siglip;

/// Reserved model identifiers for the model-backed spaces (never cross-ranked with `*-stats-v1`).
pub const SIGLIP_MODEL: &str = "siglip";
pub const CLAP_MODEL: &str = "clap";

/// Content-addressed EmbeddingSpace id: `model@version+media+dim+metric` (§2.1). Isolating a
/// generation behind this id means a model bump invalidates only its own slice of the `embedding`
/// table, never the model-free spaces.
pub fn semantic_space_id(model: &str, version: u32, media: MediaType, dim: usize) -> String {
    format!("{model}@{version}+{}+{dim}+cos", media.as_str())
}

/// A model that maps both a decoded asset and free text into one shared, L2-normalised embedding
/// space. Media-typed so the engine picks the right model per asset (SigLIP vs CLAP) and never mixes
/// spaces. The text-query path (M5 semantic mode) uses [`encode_text`](SemanticModel::encode_text).
pub trait SemanticModel: Send + Sync {
    /// The EmbeddingSpace id this model reads/writes for `media` (see [`semantic_space_id`]).
    fn space_id(&self, media: MediaType) -> String;

    /// Encode a query string into the shared space for `media`. `None` if this model has no text
    /// tower for that media type.
    fn encode_text(&self, media: MediaType, text: &str) -> Option<Vec<f32>>;

    /// Encode a decoded asset at `path` into the shared space. `None` on decode/inference failure
    /// (fail-soft: the asset degrades to its model-free embedding).
    fn encode_asset(&self, media: MediaType, path: &Path) -> Option<Vec<f32>>;

    /// Zero-shot content labels for an asset (the classification the model-free DSP tier cannot
    /// produce): cosine-rank the asset embedding against a fixed prompt set — music / speech / sfx,
    /// genre, mood, instrument for CLAP audio — and return each `(tag, confidence)` above a floor.
    /// Suggested (not confirmed) tags, so they flow through the same accept/reject lifecycle as every
    /// other auto-tag. Default: none — a pure embedder with no label taxonomy classifies nothing.
    fn zero_shot_labels(&self, media: MediaType, path: &Path) -> Vec<(String, f32)> {
        let _ = (media, path);
        Vec::new()
    }
}

/// Load the configured semantic model, or `None` when this build ships no usable weights.
///
/// Default build: always `None` — the model-free descriptors stand in and hybrid search still works
/// off them. With `--features semantic`, this is the single place the candle (SigLIP) / ONNX (CLAP)
/// model is constructed from weights under `data_dir`; until those weights ship it logs the gap and
/// returns `None`, so turning the feature on is always safe.
#[cfg(feature = "semantic")]
pub fn load(data_dir: &Path) -> Option<Box<dyn SemanticModel>> {
    // SigLIP (image + 3D) and CLAP (audio) are independent checkpoints; load whichever have weights
    // and dispatch per media through the composite. `None` only when neither is present.
    let siglip = siglip::load(data_dir);
    let clap = clap::load(data_dir);
    match (siglip, clap) {
        (None, None) => None,
        (siglip, clap) => Some(Box::new(Composite { siglip, clap })),
    }
}

/// Fans a per-media call out to the checkpoint that owns that media (image/3D → SigLIP, audio →
/// CLAP), so the engine holds one `Box<dyn SemanticModel>` and never has to know two models exist.
#[cfg(feature = "semantic")]
struct Composite {
    siglip: Option<Box<dyn SemanticModel>>,
    clap: Option<Box<dyn SemanticModel>>,
}

#[cfg(feature = "semantic")]
impl Composite {
    fn pick(&self, media: MediaType) -> Option<&dyn SemanticModel> {
        match media {
            MediaType::Audio => self.clap.as_deref(),
            MediaType::Image | MediaType::Model => self.siglip.as_deref(),
            // Deliberate: neither checkpoint covers video or prose, and inventing one (feeding a
            // poster frame to SigLIP, say) would make the space heterogeneous. Both already rank in
            // their model-free spaces (`video-stats-v1` / `text-hash-v1`); a video encoder or a
            // model-backed text encoder is a later bump into its own space, not a reuse of these.
            MediaType::Video | MediaType::Document => None,
        }
    }
}

#[cfg(feature = "semantic")]
impl SemanticModel for Composite {
    fn space_id(&self, media: MediaType) -> String {
        self.pick(media)
            .map(|m| m.space_id(media))
            .unwrap_or_default()
    }
    fn encode_text(&self, media: MediaType, text: &str) -> Option<Vec<f32>> {
        self.pick(media)?.encode_text(media, text)
    }
    fn encode_asset(&self, media: MediaType, path: &Path) -> Option<Vec<f32>> {
        self.pick(media)?.encode_asset(media, path)
    }
    fn zero_shot_labels(&self, media: MediaType, path: &Path) -> Vec<(String, f32)> {
        self.pick(media)
            .map(|m| m.zero_shot_labels(media, path))
            .unwrap_or_default()
    }
}

/// Default build: no model — the model-free descriptors stand in.
#[cfg(not(feature = "semantic"))]
pub fn load(_data_dir: &Path) -> Option<Box<dyn SemanticModel>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_id_is_content_addressed() {
        assert_eq!(
            semantic_space_id(SIGLIP_MODEL, 1, MediaType::Image, 768),
            "siglip@1+image+768+cos"
        );
        assert_eq!(
            semantic_space_id(CLAP_MODEL, 1, MediaType::Audio, 512),
            "clap@1+audio+512+cos"
        );
        // A version bump changes the slice, leaving the model-free space untouched.
        assert_ne!(
            semantic_space_id(SIGLIP_MODEL, 1, MediaType::Image, 768),
            semantic_space_id(SIGLIP_MODEL, 2, MediaType::Image, 768),
        );
    }

    #[test]
    fn default_build_has_no_model() {
        assert!(load(Path::new("/tmp")).is_none());
    }

    /// A pure embedder (no label taxonomy) classifies nothing — the default trait method holds so the
    /// `analyze_one` wiring is a safe no-op for such models.
    struct PureEmbedder;
    impl SemanticModel for PureEmbedder {
        fn space_id(&self, media: MediaType) -> String {
            semantic_space_id(CLAP_MODEL, 1, media, 4)
        }
        fn encode_text(&self, _m: MediaType, _t: &str) -> Option<Vec<f32>> {
            None
        }
        fn encode_asset(&self, _m: MediaType, _p: &Path) -> Option<Vec<f32>> {
            None
        }
    }

    #[test]
    fn zero_shot_labels_default_is_empty() {
        let m = PureEmbedder;
        assert!(m
            .zero_shot_labels(MediaType::Audio, Path::new("/tmp/x.wav"))
            .is_empty());
    }

    /// A classifying model overrides the default; the pipeline suggests whatever it returns.
    struct Classifier;
    impl SemanticModel for Classifier {
        fn space_id(&self, media: MediaType) -> String {
            semantic_space_id(CLAP_MODEL, 1, media, 4)
        }
        fn encode_text(&self, _m: MediaType, _t: &str) -> Option<Vec<f32>> {
            None
        }
        fn encode_asset(&self, _m: MediaType, _p: &Path) -> Option<Vec<f32>> {
            None
        }
        fn zero_shot_labels(&self, media: MediaType, _p: &Path) -> Vec<(String, f32)> {
            match media {
                MediaType::Audio => vec![("music".into(), 0.8), ("orchestral".into(), 0.6)],
                _ => vec![],
            }
        }
    }

    #[test]
    fn zero_shot_labels_override_is_media_scoped() {
        let m = Classifier;
        let audio = m.zero_shot_labels(MediaType::Audio, Path::new("/x.wav"));
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[0].0, "music");
        assert!(m
            .zero_shot_labels(MediaType::Image, Path::new("/x.png"))
            .is_empty());
    }
}
