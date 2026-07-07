//! The automation brain (tech-spec 05): the analysis pass that turns a scanned asset into embeddings,
//! derived signals, and auto-tag/-category *suggestions*, plus the background job that drives it.
//!
//! **Model-free v1.** Per ADR 0006 / tech-spec 05 §2.3 the default build ships no model weights and
//! makes no network call, so the embedders here are deterministic descriptors (image = a normalised
//! low-res luminance grid from `dam-media`; audio/3D = normalised cheap-attribute stats). They sit
//! behind the same `EmbeddingSpace` seam the SigLIP/CLAP path will use, so a real model is a
//! `model_version` bump (§7), not a rewrite. Spaces are named honestly (`*-stats-v1`).
//!
//! Every stage is fail-soft (DESIGN_GUIDELINES §2): a bad decode degrades that one asset to "no
//! embedding" and the pass moves on — never an aborted job.

use crate::emit_progress;
use dam_api::dto::*;
use dam_api::event::{ChangeKind, LibraryEvent};
use dam_api::id::JobId;
use dam_store::{AnalysisTarget, ImageAnalysis, Store};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

/// The current analysis pipeline version. Bump when any extractor's algorithm/output changes so the
/// Plan stage re-enqueues assets behind it (§7.2). One monotonic number covers the v1 extractor set.
/// V2: the audio classifier moved from a duration heuristic to measured DSP features (loopability,
/// tonality/key, tempo, envelope) — every audio asset re-analyses to get an honest class.
pub const PIPELINE_VERSION: i64 = 2;

/// Embedding-space ids (§2.1). Model-free descriptors in v1 — see module docs. One logical index per
/// media type; vectors from different spaces are never cross-ranked (§3.1).
const IMAGE_SPACE: &str = "image-stats-v1";
const AUDIO_SPACE: &str = "audio-stats-v1";
const MODEL_SPACE: &str = "model-stats-v1";

const PROGRESS_EVERY: u64 = 8;

/// The background analysis job (mirrors `scan::run_scan`): plan is already done (the caller passed the
/// due `targets`), so this runs Extract→Derive→Classify→Index per asset, emitting progress + a
/// `Reanalyzed` change event as each completes (§1.3 incremental). Runs on a blocking thread.
pub(crate) fn run_analyze(
    store: Arc<Store>,
    events: broadcast::Sender<LibraryEvent>,
    job: JobId,
    targets: Vec<AnalysisTarget>,
    cancel: Arc<AtomicBool>,
    model: Option<Arc<dyn crate::semantic::SemanticModel>>,
) {
    let total = targets.len() as u64;
    let _ = store.update_job_progress(&job, JobState::Running, 0, Some(total), None);
    let mut done: u64 = 0;
    let mut warnings: u64 = 0;

    for t in targets {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match analyze_one(&store, &t, model.as_deref()) {
            Ok(()) => {
                let _ = events.send(LibraryEvent::AssetChanged {
                    id: t.id,
                    kind: ChangeKind::Reanalyzed,
                });
            }
            Err(e) => {
                warnings += 1;
                tracing::warn!(asset = %t.id, path = %t.path, error = %e, "analysis skipped asset");
            }
        }
        done += 1;
        if done.is_multiple_of(PROGRESS_EVERY) {
            let _ = store.update_job_progress(
                &job,
                JobState::Running,
                done,
                Some(total),
                Some(&t.path),
            );
            emit_progress(&store, &events, &job);
        }
    }

    if cancel.load(Ordering::Relaxed) {
        let _ = store.set_job_state(&job, JobState::Cancelled, None);
    } else {
        let _ = store.update_job_progress(&job, JobState::Done, done, Some(total), None);
        let note = (warnings > 0).then(|| format!("{warnings} item(s) skipped"));
        let _ = store.set_job_state(&job, JobState::Done, note.as_deref());
    }
    emit_progress(&store, &events, &job);
    tracing::info!(%job, done, warnings, "analysis finished");
}

/// Analyse one asset end-to-end: extract features, derive signals, classify → suggest, index the
/// embedding, and mark it analysed at the current version. Fail-soft per stage.
fn analyze_one(
    store: &Store,
    t: &AnalysisTarget,
    model: Option<&dyn crate::semantic::SemanticModel>,
) -> Result<(), String> {
    let abs = resolve(&t.source_uri, &t.path)?;
    let det = dam_media::Detected {
        media: t.media,
        format: t.format.clone(),
    };
    match t.media {
        MediaType::Image => analyze_image(store, t, &abs)?,
        MediaType::Audio => analyze_audio(store, t, &abs, &det)?,
        MediaType::Model => analyze_model(store, t, &abs, &det)?,
    }
    // Model-backed semantic embedding (semantic-search M4): when a model ships, also index the
    // shared text/media space so text queries can rank against it (M5 semantic mode). Additive to
    // the model-free space above — different `space_id`, never cross-ranked. `None` on the default
    // build, so this is a no-op there.
    if let Some(m) = model {
        if let Some(vec) = m.encode_asset(t.media, &abs) {
            let space = m.space_id(t.media);
            if let Err(e) = store.set_embedding(&t.id, &space, t.media, &vec, "semantic@1") {
                tracing::warn!(asset = %t.id, error = %e, "semantic embedding failed");
            }
        }
        // Zero-shot content labels — the semantic tier CLAP/SigLIP add on top of the model-free DSP
        // class (music/speech/sfx by timbre, genre, mood, instrument): tags pure DSP can't derive.
        // Suggested (not confirmed) so they share the accept/reject lifecycle. No-op when the model
        // has no taxonomy for this media (default trait impl returns empty).
        for (tag, conf) in m.zero_shot_labels(t.media, &abs) {
            if let Err(e) = store.suggest_tag(&t.id, &tag, conf, "semantic@1") {
                tracing::warn!(asset = %t.id, tag = %tag, error = %e, "semantic label suggest_tag failed");
            }
        }
    }
    // Filename-derived tag suggestions (semantic-search M2): the words in a name ("ak47", "lowpoly")
    // are a real, if weak, content signal. Suggested (not confirmed) so they flow through the same
    // accept/reject lifecycle as the media-derived tags — a reject is remembered.
    suggest_filename_tags(store, t);
    store
        .mark_analysed(&t.id, PIPELINE_VERSION)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Suggest a tag for each meaningful whole word in the filename: alphanumeric runs of ≥3 chars that
/// aren't purely numeric and aren't the format/extension. Low confidence — a name is a weaker signal
/// than a decoded attribute. Fail-soft per tag.
fn suggest_filename_tags(store: &Store, t: &AnalysisTarget) {
    let filename = Path::new(&t.path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&t.path);
    let fmt = t.format.to_lowercase();
    for run in filename.split(|c: char| !c.is_alphanumeric()) {
        let tok = run.to_lowercase();
        if tok.len() < 3 || tok == fmt || tok.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Err(e) = store.suggest_tag(&t.id, &tok, 0.4, "filename@1") {
            tracing::warn!(asset = %t.id, tag = %tok, error = %e, "filename suggest_tag failed");
        }
    }
}

fn analyze_image(store: &Store, t: &AnalysisTarget, abs: &Path) -> Result<(), String> {
    let f = dam_media::extract_image_features(abs).map_err(|e| e.to_string())?;
    // Derive: persist perceptual/tileability/colour signals (§5, §6).
    store
        .set_image_analysis(
            &t.id,
            &ImageAnalysis {
                phash: f.phash,
                tileability: f.tileability,
                repeat_period: f.repeat_period.map(|p| p as i64),
                tile_class: f.tile_class.to_string(),
                dominant_colors: f.dominant_colors.clone(),
                class: f.class.to_string(),
            },
        )
        .map_err(|e| e.to_string())?;
    // Index: the visual embedding (§3.1).
    store
        .set_embedding(
            &t.id,
            IMAGE_SPACE,
            MediaType::Image,
            &f.embedding,
            "image-stats@1",
        )
        .map_err(|e| e.to_string())?;
    // Classify → suggest (§1.4): tileability + content class drive auto-tags with a confidence.
    let mut suggestions: Vec<(&str, f32)> = vec![(f.class, 0.7)];
    match f.tile_class {
        "seamless" => suggestions.push(("seamless", f.tileability.clamp(0.5, 1.0))),
        "tiled" => suggestions.push(("tileable", 0.8)),
        _ => {}
    }
    suggest_all(store, &t.id, &suggestions);
    Ok(())
}

fn analyze_audio(
    store: &Store,
    t: &AnalysisTarget,
    abs: &Path,
    det: &dam_media::Detected,
) -> Result<(), String> {
    let MediaAttributes::Audio(a) = dam_media::extract_metadata(abs, det) else {
        return Err("audio metadata unavailable".into());
    };
    // Model-free stats embedding: a normalised cheap-attribute vector (§2.3). Duration dominates.
    let dur = a.duration_ms.unwrap_or(0) as f32;
    let vec = normalise(vec![
        (1.0 + dur).log10(),
        a.sample_rate.unwrap_or(0) as f32 / 48_000.0,
        a.channels.unwrap_or(0) as f32 / 2.0,
        a.bit_depth.unwrap_or(0) as f32 / 24.0,
    ]);
    store
        .set_embedding(&t.id, AUDIO_SPACE, MediaType::Audio, &vec, "audio-stats@1")
        .map_err(|e| e.to_string())?;
    // Classify from *measured* DSP signals, not duration (§4.2). A full decode yields loopability
    // (authored `smpl` loop points, else a seamless wrap boundary), tonality + key, tempo, and
    // envelope shape — the orthogonal "does it loop" and "what is it" axes a length threshold
    // conflates. Fail-soft: if the decode fails we fall back to a neutral duration split so the asset
    // still gets *a* class (never silently "loop", the old bug).
    let (class, conf, mut extra): (&str, f32, Vec<(&str, f32)>) =
        match dam_media::extract_audio_features(abs, &det.format) {
            Ok(f) => {
                let mut tags: Vec<(&str, f32)> = Vec::new();
                if f.is_loop {
                    tags.push(("loop", 0.6));
                }
                tags.push(if f.tonal {
                    ("tonal", 0.6)
                } else {
                    ("atonal", 0.5)
                });
                if f.bpm.is_some() {
                    tags.push(("rhythmic", 0.6));
                }
                tags.push(if f.sustained {
                    ("sustained", 0.5)
                } else {
                    ("transient", 0.5)
                });
                suggest_audio_extras(store, &t.id, &f);
                let conf = if f.loop_source == dam_media::LoopSource::Metadata {
                    0.95
                } else {
                    0.6
                };
                (f.class, conf, tags)
            }
            Err(e) => {
                tracing::warn!(asset = %t.id, error = %e, "audio feature extraction failed; duration fallback");
                let (c, cf) = match a.duration_ms {
                    Some(ms) if ms < 2_000 => ("one_shot", 0.4),
                    _ => ("sfx", 0.3),
                };
                (c, cf, Vec::new())
            }
        };
    store
        .set_media_class(&t.id, MediaType::Audio, class)
        .map_err(|e| e.to_string())?;
    extra.insert(0, (class, conf));
    suggest_all(store, &t.id, &extra);
    Ok(())
}

fn analyze_model(
    store: &Store,
    t: &AnalysisTarget,
    abs: &Path,
    det: &dam_media::Detected,
) -> Result<(), String> {
    let MediaAttributes::Model(m) = dam_media::extract_metadata(abs, det) else {
        return Err("model metadata unavailable".into());
    };
    // Model-free stats embedding from geometry counts + rig/anim/uv flags (§2.3, §4.2 corroboration).
    let vec = normalise(vec![
        (1.0 + m.vertex_count.unwrap_or(0) as f32).log10(),
        (1.0 + m.triangle_count.unwrap_or(0) as f32).log10(),
        (1.0 + m.mesh_count.unwrap_or(0) as f32).log10(),
        m.material_count.unwrap_or(0) as f32 / 8.0,
        m.texture_count.unwrap_or(0) as f32 / 8.0,
        m.has_rig.unwrap_or(false) as u8 as f32,
        m.has_animation.unwrap_or(false) as u8 as f32,
        m.has_uvs.unwrap_or(false) as u8 as f32,
    ]);
    store
        .set_embedding(&t.id, MODEL_SPACE, MediaType::Model, &vec, "model-stats@1")
        .map_err(|e| e.to_string())?;
    // Category guess from triangle budget (§5): a coarse low/mid/high-poly bucket.
    let class = match m.triangle_count {
        Some(tris) if tris < 2_000 => "prop_lowpoly",
        Some(tris) if tris < 50_000 => "prop",
        Some(_) => "prop_highpoly",
        None => "prop",
    };
    store
        .set_media_class(&t.id, MediaType::Model, class)
        .map_err(|e| e.to_string())?;
    // Structural facts make good high-confidence suggestions (§1.4).
    let mut suggestions: Vec<(&str, f32)> = vec![(class, 0.6)];
    if m.has_rig.unwrap_or(false) {
        suggestions.push(("rigged", 0.95));
    }
    if m.has_animation.unwrap_or(false) {
        suggestions.push(("animated", 0.95));
    }
    if m.has_uvs.unwrap_or(false) {
        suggestions.push(("uv_mapped", 0.9));
    }
    suggest_all(store, &t.id, &suggestions);
    Ok(())
}

/// Suggest the audio tags that need an owned string (tempo bucket, musical key) — kept out of the
/// `&'static str` batch below. Tempo is bucketed to the nearest 5 BPM so near-identical estimates
/// collapse to one filterable tag. Fail-soft per tag.
fn suggest_audio_extras(store: &Store, id: &dam_api::id::AssetId, f: &dam_media::AudioFeatures) {
    if let Some(bpm) = f.bpm {
        let bucket = ((bpm / 5.0).round() * 5.0) as i64;
        let tag = format!("{bucket}bpm");
        if let Err(e) = store.suggest_tag(id, &tag, 0.5, "analyze@1") {
            tracing::warn!(asset = %id, tag = %tag, error = %e, "bpm suggest_tag failed");
        }
    }
    if let Some(key) = f.key {
        let tag = format!("key-{key}");
        if let Err(e) = store.suggest_tag(id, &tag, 0.5, "analyze@1") {
            tracing::warn!(asset = %id, tag = %tag, error = %e, "key suggest_tag failed");
        }
    }
}

/// Write a batch of suggested tags fail-soft (a single insert failure never sinks the asset).
fn suggest_all(store: &Store, id: &dam_api::id::AssetId, suggestions: &[(&str, f32)]) {
    for (name, conf) in suggestions {
        if let Err(e) = store.suggest_tag(id, name, *conf, "analyze@1") {
            tracing::warn!(asset = %id, tag = name, error = %e, "suggest_tag failed");
        }
    }
}

/// Resolve a source root + stored relative path to an absolute file, traversal-guarded (same rule as
/// `read_asset_file`). A stored path that escapes its root is rejected — defence in depth.
fn resolve(source_uri: &str, rel_path: &str) -> Result<std::path::PathBuf, String> {
    let rel = Path::new(rel_path);
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("asset path escapes its source root".into());
    }
    Ok(Path::new(source_uri).join(rel))
}

/// Owned-vector adapter over the shared in-place L2 normaliser (`dam_media::l2_normalise`), so the
/// stats embedders below can build a vector inline. A zero vector stays zero (cosine 0 = "no signal").
fn normalise(mut v: Vec<f32>) -> Vec<f32> {
    dam_media::l2_normalise(&mut v);
    v
}
