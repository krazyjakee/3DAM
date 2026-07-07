//! CLAP audio encoder (semantic-search M4, audio sub-path) — the ONNX/`ort` model behind the
//! [`SemanticModel`] seam for sound, the audio analogue of [`super::siglip`]. LAION-CLAP (an HTSAT
//! audio tower + a RoBERTa text tower projected to a shared 512-d space) is the v1 audio pick from
//! `spikes/embedding-models/` under ADR 0006.
//!
//! Because the audio and text towers share one space, a caption ([`encode_text`]) and a clip
//! ([`encode_asset`]) embed comparably — which powers two things the model-free DSP tier cannot do:
//! text→audio semantic search, and **zero-shot content labels** ([`zero_shot_labels`]) — music /
//! speech / sfx / ambience and coarse genre/mood — by cosine-ranking the clip against fixed caption
//! prompts. The waveform is turned into the tower's log-mel input by pure [`dam_media`] DSP.
//!
//! **Reality check (mirrors [`super::siglip`]).** The seam, preprocessing, session wiring, prompt
//! taxonomy, and zero-shot math are all real. What is not bundled is the ONNX weights + the
//! libonnxruntime shared library (`ort` uses `load-dynamic`): [`load`] returns `None` until both are
//! present under the data dir, so enabling `--features semantic` never breaks a build or a run. The
//! input/output tensor layout below matches the reference export documented in the spike; a different
//! checkpoint needs these constants adjusted (exactly as SigLIP hardcodes `base_patch16_224`).
//!
//! [`encode_text`]: SemanticModel::encode_text
//! [`encode_asset`]: SemanticModel::encode_asset
//! [`zero_shot_labels`]: SemanticModel::zero_shot_labels

use super::{semantic_space_id, SemanticModel, CLAP_MODEL};
use dam_api::dto::MediaType;
use dam_media::MelConfig;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use std::path::Path;
use std::sync::Mutex;
use tokenizers::Tokenizer;

/// LAION-CLAP fixed parameters (must match the shipped checkpoint export).
const DIM: usize = 512;
const VERSION: u32 = 1;
/// Text tower context length (RoBERTa); token ids are right-padded / truncated to this.
const SEQ_LEN: usize = 77;
/// HTSAT's fixed audio context in seconds — the mel is padded / truncated to this many frames.
const AUDIO_SECONDS: usize = 10;
/// CLAP mel front-end: 48 kHz, 1024-pt FFT, 480 hop, 64 bands over 50 Hz–14 kHz.
const MEL: MelConfig = MelConfig {
    sample_rate: 48_000,
    n_fft: 1024,
    hop: 480,
    n_mels: 64,
    fmin: 50.0,
    fmax: 14_000.0,
};
/// Frames in the fixed audio window (derived from `AUDIO_SECONDS` at the mel hop).
const AUDIO_FRAMES: usize = (AUDIO_SECONDS * 48_000 - MEL.n_fft) / MEL.hop + 1;

/// A group of zero-shot candidate labels scored together via softmax over their prompt similarities.
/// `floor` is the probability a label must clear to be suggested (a low floor on the coarse group
/// yields one confident label; a higher floor on the descriptive groups suppresses weak guesses).
struct LabelGroup {
    floor: f32,
    labels: &'static [(&'static str, &'static str)], // (tag, caption prompt)
}

/// The fixed audio taxonomy. Prompts use CLAP's caption style; extend freely — each group is scored
/// independently so adding a group cannot skew another.
const GROUPS: &[LabelGroup] = &[
    LabelGroup {
        floor: 0.40,
        labels: &[
            ("music", "music"),
            ("speech", "a person speaking"),
            ("sfx", "a sound effect"),
            ("ambience", "ambient background noise"),
        ],
    },
    LabelGroup {
        floor: 0.55,
        labels: &[
            ("orchestral", "orchestral music"),
            ("electronic", "electronic music"),
            ("percussion", "drums and percussion"),
            ("vocal", "singing vocals"),
            ("tense", "tense suspenseful music"),
            ("calm", "calm peaceful music"),
        ],
    },
];

/// A loaded CLAP model: both ONNX sessions behind `Mutex` (ONNX Runtime session state is not shared
/// across threads in v1; inference already runs on the analysis lane's blocking threads, so the
/// serialization is not a hot path), the text tokenizer, and the prompt embeddings precomputed once
/// at load so zero-shot classification is a single audio forward pass plus cosines.
pub struct ClapModel {
    audio: Mutex<Session>,
    text: Mutex<Session>,
    tokenizer: Tokenizer,
    /// `prompts[group][label]` — the L2-normalised text embedding of each candidate's caption.
    prompts: Vec<Vec<Vec<f32>>>,
}

/// Try to load CLAP from `<data_dir>/models/clap/` (`audio_encoder.onnx`, `text_encoder.onnx`,
/// `tokenizer.json`). `None` (with a log) if any file is missing or a session/tokenizer fails to
/// build — enabling the feature is always safe.
pub fn load(data_dir: &Path) -> Option<Box<dyn SemanticModel>> {
    let dir = data_dir.join("models").join("clap");
    let audio_path = dir.join("audio_encoder.onnx");
    let text_path = dir.join("text_encoder.onnx");
    let tok_path = dir.join("tokenizer.json");
    if !audio_path.exists() || !text_path.exists() || !tok_path.exists() {
        tracing::info!(dir = %dir.display(), "CLAP weights/tokenizer missing — audio semantic model disabled");
        return None;
    }
    match ClapModel::build(&audio_path, &text_path, &tok_path) {
        Ok(m) => {
            tracing::info!(dir = %dir.display(), "CLAP audio semantic model loaded (M4)");
            Some(Box::new(m))
        }
        Err(e) => {
            tracing::warn!(error = %e, "CLAP failed to load — audio semantic model disabled");
            None
        }
    }
}

impl ClapModel {
    fn build(audio_path: &Path, text_path: &Path, tok_path: &Path) -> Result<Self, String> {
        let audio = build_session(audio_path)?;
        let mut text = build_session(text_path)?;
        let tokenizer =
            Tokenizer::from_file(tok_path).map_err(|e| format!("CLAP tokenizer: {e}"))?;

        // Precompute every prompt embedding once (a handful of text forward passes at load).
        let mut prompts = Vec::with_capacity(GROUPS.len());
        for group in GROUPS {
            let mut embs = Vec::with_capacity(group.labels.len());
            for (_, prompt) in group.labels {
                let e = encode_text_inner(&mut text, &tokenizer, prompt)
                    .ok_or_else(|| format!("prompt embed failed: {prompt}"))?;
                embs.push(e);
            }
            prompts.push(embs);
        }

        Ok(Self {
            audio: Mutex::new(audio),
            text: Mutex::new(text),
            tokenizer,
            prompts,
        })
    }
}

impl SemanticModel for ClapModel {
    fn space_id(&self, media: MediaType) -> String {
        semantic_space_id(CLAP_MODEL, VERSION, media, DIM)
    }

    fn encode_text(&self, media: MediaType, text: &str) -> Option<Vec<f32>> {
        if media != MediaType::Audio {
            return None;
        }
        let mut session = self.text.lock().ok()?;
        encode_text_inner(&mut session, &self.tokenizer, text)
    }

    fn encode_asset(&self, media: MediaType, path: &Path) -> Option<Vec<f32>> {
        if media != MediaType::Audio {
            return None;
        }
        let format = path.extension().and_then(|s| s.to_str()).unwrap_or("wav");
        let mel = dam_media::log_mel(path, format, &MEL).ok()?;
        let data = fit_mel(&mel);
        let shape = vec![1i64, MEL.n_mels as i64, AUDIO_FRAMES as i64];
        let mut session = self.audio.lock().ok()?;
        let out = run_tower(&mut session, shape, data)?;
        Some(l2_normalise(out))
    }

    fn zero_shot_labels(&self, media: MediaType, path: &Path) -> Vec<(String, f32)> {
        if media != MediaType::Audio {
            return Vec::new();
        }
        let Some(audio) = self.encode_asset(media, path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (gi, group) in GROUPS.iter().enumerate() {
            // Cosine (both sides L2-normalised → dot product) → softmax over the group's candidates.
            let sims: Vec<f32> = self.prompts[gi].iter().map(|p| dot(&audio, p)).collect();
            for (li, prob) in softmax(&sims).into_iter().enumerate() {
                if prob >= group.floor {
                    out.push((group.labels[li].0.to_string(), prob));
                }
            }
        }
        out
    }
}

/// Build an ONNX Runtime session from a model file with graph optimisation on.
fn build_session(path: &Path) -> Result<Session, String> {
    Session::builder()
        .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level3))
        .and_then(|b| b.commit_from_file(path))
        .map_err(|e| format!("ort session {}: {e}", path.display()))
}

/// Tokenize + run the text tower → L2-normalised embedding. Free function so the build-time prompt
/// precompute and the query path share one implementation over an already-locked session.
fn encode_text_inner(session: &mut Session, tokenizer: &Tokenizer, text: &str) -> Option<Vec<f32>> {
    let enc = tokenizer.encode(text, true).ok()?;
    let mut ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
    ids.resize(SEQ_LEN, 0);
    ids.truncate(SEQ_LEN);
    let tensor = Tensor::from_array((vec![1i64, SEQ_LEN as i64], ids)).ok()?;
    let outputs = session.run(ort::inputs![tensor]).ok()?;
    let (_shape, data) = outputs[0].try_extract_tensor::<f32>().ok()?;
    Some(l2_normalise(data.to_vec()))
}

/// Run the audio tower on a prepared mel tensor → raw embedding (caller normalises).
fn run_tower(session: &mut Session, shape: Vec<i64>, data: Vec<f32>) -> Option<Vec<f32>> {
    let tensor = Tensor::from_array((shape, data)).ok()?;
    let outputs = session.run(ort::inputs![tensor]).ok()?;
    let (_shape, out) = outputs[0].try_extract_tensor::<f32>().ok()?;
    Some(out.to_vec())
}

/// Pad or truncate a mel spectrogram to exactly `AUDIO_FRAMES` columns, row-major `[n_mels, frames]`.
fn fit_mel(mel: &dam_media::MelSpectrogram) -> Vec<f32> {
    let mut data = vec![0.0f32; mel.n_mels * AUDIO_FRAMES];
    for m in 0..mel.n_mels {
        let take = mel.n_frames.min(AUDIO_FRAMES);
        for f in 0..take {
            data[m * AUDIO_FRAMES + f] = mel.data[m * mel.n_frames + f];
        }
    }
    data
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn softmax(v: &[f32]) -> Vec<f32> {
    let max = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = v.iter().map(|x| (x - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    if sum <= f32::EPSILON {
        return vec![0.0; v.len()];
    }
    exps.into_iter().map(|e| e / sum).collect()
}

fn l2_normalise(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn softmax_is_a_distribution() {
        let p = softmax(&[1.0, 2.0, 3.0]);
        let sum: f32 = p.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5);
        assert!(p[2] > p[1] && p[1] > p[0]);
    }

    #[test]
    fn audio_frames_is_ten_seconds() {
        // 10 s at 48 kHz / 480 hop ≈ 1000 frames.
        assert!((990..=1005).contains(&AUDIO_FRAMES));
    }

    #[test]
    fn missing_weights_disable_the_model() {
        assert!(load(Path::new("/nonexistent-3dam-clap")).is_none());
    }
}
