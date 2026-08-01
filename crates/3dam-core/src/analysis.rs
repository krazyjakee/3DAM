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
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

/// The current analysis pipeline version. Bump when any extractor's algorithm/output changes so the
/// Plan stage re-enqueues assets behind it (§7.2). One monotonic number covers the v1 extractor set.
/// V2: the audio classifier moved from a duration heuristic to measured DSP features (loopability,
/// tonality/key, tempo, envelope) — every audio asset re-analyses to get an honest class.
/// V3: the analyze pass also extracts continuous acoustic features (loudness/brightness/harmonicity,
/// issue #61), so audio re-analyses once more to populate them.
pub const PIPELINE_VERSION: i64 = 3;

/// Embedding-space ids (§2.1). Model-free descriptors in v1 — see module docs. One logical index per
/// media type; vectors from different spaces are never cross-ranked (§3.1).
///
/// This match is the **only** place the model-free space names are written. The analyse pass embeds
/// into it and [`crate::EmbeddedLibrary::embedding_spaces`] advertises it to peers, so a peer can
/// never gate cross-peer similarity on a name nothing was ever written under. Deliberately a match
/// rather than a `format!` over the media name: the mapping is not derivable (documents rank in a
/// hashed-text space, not a stats one), and being exhaustive means a sixth media type is a compile
/// error here rather than a silently missing space.
pub(crate) fn model_free_space(media: MediaType) -> &'static str {
    match media {
        MediaType::Image => "image-stats-v1",
        MediaType::Audio => "audio-stats-v1",
        MediaType::Model => "model-stats-v1",
        // Video: container shape only (see [`analyze_video`]) — a weak descriptor, honestly named.
        MediaType::Video => "video-stats-v1",
        // Documents: hashed bag-of-words over the extracted text (`dam_media::text_descriptor`).
        // Text is the one media type where the model-free descriptor is genuinely useful rather
        // than a placeholder, because lexical overlap *is* a real similarity signal for prose. The
        // model-backed text encoder is a later feature-gated bump into its own space (issue #47),
        // never a redefinition of this one.
        MediaType::Document => "text-hash-v1",
    }
}

const PROGRESS_EVERY: u64 = 8;

/// The background analysis job (mirrors `scan::run_scan`): plan is already done (the caller passed the
/// due `targets`), so this runs Extract→Derive→Classify→Index per asset, emitting progress + a
/// `Reanalyzed` change event as each completes (§1.3 incremental). Runs on a blocking thread.
///
/// Per-asset work is CPU-bound (decode → derive → classify → embed), so it fans out across the rayon
/// pool (ADR 0007, issue #67) — a whole-library pass now scales with cores instead of pinning one. The
/// async caller already handed off via `spawn_blocking`, so this stays a one-shot async→CPU hop. The
/// store's writes still serialise on its single connection mutex (no `SQLITE_BUSY` — one guarded
/// connection), but the expensive compute overlaps, which is where the time goes.
#[allow(clippy::too_many_arguments)] // the job runner's full context; a struct would just rename it
pub(crate) fn run_analyze(
    store: Arc<Store>,
    events: broadcast::Sender<LibraryEvent>,
    job: JobId,
    targets: Vec<AnalysisTarget>,
    cancel: Arc<AtomicBool>,
    model: Option<Arc<dyn crate::semantic::SemanticModel>>,
    pool: &rayon::ThreadPool,
    governor: &crate::resources::Governor,
    scratch: &Path,
) {
    let total = targets.len() as u64;
    let _ = store.update_job_progress(&job, JobState::Running, 0, Some(total), None);
    // Shared across the rayon workers: a monotonic completion counter and a skip counter.
    let done = AtomicU64::new(0);
    let warnings = AtomicU64::new(0);

    // Rebuild each distinct source's backend **once**, before the fan-out. Opening per asset would
    // mean an SSH handshake or an SMB session setup per file, which for a remote pass is most of the
    // wall clock. `FileSource` is `Send + Sync`, so one instance serves every worker.
    //
    // A source that won't open (host down, credentials rotated) is recorded here, not raised: its
    // assets each fail their own item below, exactly like an undecodable file. Degrade one edge, not
    // the job — golden rule 6.
    let backends = open_backends(&store, &targets, scratch);

    // Run on the bounded background pool (not the global rayon pool) so a whole-library pass leaves
    // cores free for interactive inspector reads instead of pinning every core (tech-spec 14).
    pool.install(|| {
        targets.par_iter().for_each(|t| {
        // Cooperative cancel: in-flight items finish; still-queued ones fall through as cheap no-ops.
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        // Good-neighbour pacing (tech-spec 14 §3.4): when the *host* runs short on memory or CPU,
        // every worker parks here between items until pressure clears — a whole-library pass must
        // never swap a shared box to death. Cancel still exits promptly.
        governor.pace(&cancel);
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let outcome = match backends.get(&t.source_id) {
            Some(Ok(fs)) => analyze_one(&store, t, model.as_deref(), fs.as_ref()),
            Some(Err(e)) => Err(e.clone()),
            None => Err("source backend missing".to_string()),
        };
        match outcome {
            Ok(()) => {
                let _ = events.send(LibraryEvent::AssetChanged {
                    id: t.id,
                    source_id: Some(t.source_id),
                    kind: ChangeKind::Reanalyzed,
                });
            }
            Err(e) => {
                warnings.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(asset = %t.id, path = %t.path, error = %e, "analysis skipped asset");
            }
        }
        // Report on every Nth completion — `fetch_add` returns the prior value, so `n` is this
        // worker's 1-based ordinal; the bar advances monotonically even as workers interleave.
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(PROGRESS_EVERY) {
            let _ =
                store.update_job_progress(&job, JobState::Running, n, Some(total), Some(&t.path));
            emit_progress(&store, &events, &job);
        }
    });
    });

    let done = done.load(Ordering::Relaxed);
    let warnings = warnings.load(Ordering::Relaxed);
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

/// Rebuild one `FileSource` per distinct source in `targets` (issue #48).
///
/// The result is keyed by `source_id` so the fan-out is a map lookup, and holds the *error* for a
/// source that could not be opened rather than dropping it — otherwise its assets would silently
/// vanish from the pass instead of each reporting why they were skipped. The error is also written
/// to `source.last_error`, so the offline state surfaces in the sources list and not only in a log
/// line nobody reads.
fn open_backends(
    store: &Store,
    targets: &[AnalysisTarget],
    scratch: &Path,
) -> HashMap<dam_api::id::SourceId, Result<Arc<dyn dam_sources::FileSource>, String>> {
    let mut out: HashMap<_, Result<Arc<dyn dam_sources::FileSource>, String>> = HashMap::new();
    for t in targets {
        if out.contains_key(&t.source_id) {
            continue;
        }
        let entry = match dam_sources::open_source(&t.connection, scratch) {
            Ok(fs) => Ok(Arc::from(fs)),
            Err(e) => {
                let msg = e.to_string();
                let _ = store.set_source_error(&t.source_id, &msg);
                tracing::warn!(source = %t.source_id, error = %msg, "source unavailable for analysis");
                Err(msg)
            }
        };
        out.insert(t.source_id, entry);
    }
    out
}

/// Analyse one asset end-to-end: extract features, derive signals, classify → suggest, index the
/// embedding, and mark it analysed at the current version. Fail-soft per stage.
///
/// Bytes arrive through `fs.fetch` (issue #48): in place for a local source — the same path the old
/// root-join produced, minus the chance of disagreeing with the scan about it — and a temp file for
/// SFTP/SMB. The temp is owned by `fetched` and deleted when this function returns, so peak scratch
/// usage across a remote pass is bounded by the background pool's width times the largest asset,
/// not by the size of the batch.
fn analyze_one(
    store: &Store,
    t: &AnalysisTarget,
    model: Option<&dyn crate::semantic::SemanticModel>,
    fs: &dyn dam_sources::FileSource,
) -> Result<(), String> {
    let fetched = fs.fetch(&t.path).map_err(|e| e.to_string())?;
    let abs = fetched.path().to_path_buf();
    let det = dam_media::Detected {
        media: t.media,
        format: t.format.clone(),
    };
    match t.media {
        MediaType::Image => analyze_image(store, t, &abs)?,
        MediaType::Audio => analyze_audio(store, t, &abs, &det)?,
        MediaType::Model => analyze_model(store, t, &abs, &det)?,
        MediaType::Video => analyze_video(store, t, &abs, &det)?,
        MediaType::Document => analyze_document(store, t, &abs, &det)?,
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
            model_free_space(MediaType::Image),
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
        .set_embedding(
            &t.id,
            model_free_space(MediaType::Audio),
            MediaType::Audio,
            &vec,
            "audio-stats@1",
        )
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
                // Persist the continuous acoustic features for the inspector bars (issue #61).
                if let Err(e) =
                    store.set_audio_features(&t.id, f.loudness_lufs, f.brightness, f.harmonicity)
                {
                    tracing::warn!(asset = %t.id, error = %e, "set_audio_features failed");
                }
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
    // Waveform peaks for the inspector (issue #73): computed once here, server-side, so no client
    // re-downloads + re-decodes the audio to draw the bars. Independent of feature extraction and
    // fail-soft — a decode fault just leaves `peaks` null and the client falls back to its own decode.
    match dam_media::compute_waveform_peaks(abs, &det.format) {
        Ok(peaks) => {
            if let Err(e) = store.set_audio_peaks(&t.id, &peaks) {
                tracing::warn!(asset = %t.id, error = %e, "set_audio_peaks failed");
            }
        }
        Err(e) => tracing::warn!(asset = %t.id, error = %e, "waveform peak computation failed"),
    }
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
    // Refine the scan's cheap counts with exact ones where this build can (issue #49). The cheap
    // tier reads FBX/Collada/3DS structurally, which is right for a scan but has to assume
    // triangles for a compressed FBX index array or a `<vcount>`-less Collada polygon list, and has
    // nothing to say about `.blend`. A full Assimp import settles all of that — and *this* is the
    // tier allowed to pay for a decode, which is why it lives here and not in the scan.
    //
    // Fail-soft in both directions: a build without `model-convert`, or a file Assimp cannot read,
    // simply leaves the cheap answer in place. The refined attributes are written back before the
    // embedding is computed so the stored row and the vector agree.
    let m = match dam_media::extract_model_metadata_deep(abs, &det.format) {
        Ok(exact) => {
            if let Err(e) = store.set_media_attrs(&t.id, &MediaAttributes::Model(exact.clone())) {
                tracing::warn!(asset = %t.id, error = %e, "storing exact model counts failed");
            }
            exact
        }
        Err(e) => {
            tracing::debug!(asset = %t.id, error = %e, "exact model counts unavailable");
            m
        }
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
        .set_embedding(
            &t.id,
            model_free_space(MediaType::Model),
            MediaType::Model,
            &vec,
            "model-stats@1",
        )
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

/// Analyse a video: container-shape embedding, a duration-based class, and structural tags.
///
/// Be clear about what this descriptor is. It is built from duration, resolution, frame rate and
/// bitrate — the *shape* of the file, not its pictures — so "similar" here means "another clip of
/// roughly this length and format", not "another clip that looks like this". That is the same
/// bargain `model-stats-v1` strikes with triangle budgets, and it is the honest ceiling for a
/// media type whose decode backend may not even be installed (ADR 0015).
///
/// A visual descriptor is a real follow-up: the poster frame already goes through
/// `extract_image_features`, so a `video-frame-v1` space is mostly plumbing. It is deliberately not
/// folded into *this* space, because a space must be homogeneous — half the library having visual
/// vectors and half having shape vectors, depending on whether ffmpeg happened to be installed at
/// scan time, is exactly what the `space_id` seam exists to prevent.
fn analyze_video(
    store: &Store,
    t: &AnalysisTarget,
    abs: &Path,
    det: &dam_media::Detected,
) -> Result<(), String> {
    let MediaAttributes::Video(v) = dam_media::extract_metadata(abs, det) else {
        return Err("video metadata unavailable".into());
    };
    // With no prober installed every field is None, which would make one identical vector for
    // every video in the library — worse than useless, since it would rank them all as perfect
    // matches for each other. Skip the embedding entirely rather than index a lie.
    if v.duration_ms.is_none() && v.width.is_none() {
        tracing::debug!(asset = %t.id, "no video metadata (no prober?); skipping embedding");
        return Ok(());
    }

    let secs = v.duration_ms.unwrap_or(0) as f32 / 1000.0;
    let vec = normalise(vec![
        (1.0 + secs).log10(),
        (1.0 + v.width.unwrap_or(0) as f32).log10(),
        (1.0 + v.height.unwrap_or(0) as f32).log10(),
        v.fps.unwrap_or(0.0) / 60.0,
        (1.0 + v.bitrate.unwrap_or(0) as f32).log10() / 8.0,
        v.has_audio.unwrap_or(false) as u8 as f32,
    ]);
    store
        .set_embedding(
            &t.id,
            model_free_space(MediaType::Video),
            MediaType::Video,
            &vec,
            "video-stats@1",
        )
        .map_err(|e| e.to_string())?;

    // Duration is the one axis that reliably separates the kinds of video a game project holds.
    let class = match v.duration_ms {
        Some(ms) if ms < 5_000 => "sting",
        Some(ms) if ms < 60_000 => "clip",
        Some(_) => "cutscene",
        None => "clip",
    };
    store
        .set_media_class(&t.id, MediaType::Video, class)
        .map_err(|e| e.to_string())?;

    let mut suggestions: Vec<(&str, f32)> = vec![(class, 0.6)];
    // A video with no audio track is very often a UI/VFX element or a video texture rather than a
    // watchable clip — a genuinely useful thing to be able to filter on.
    if v.has_audio == Some(false) {
        suggestions.push(("silent", 0.9));
    }
    if let (Some(w), Some(h)) = (v.width, v.height) {
        if w >= 3840 || h >= 2160 {
            suggestions.push(("4k", 0.95));
        } else if w >= 1920 || h >= 1080 {
            suggestions.push(("1080p", 0.95));
        }
    }
    suggest_all(store, &t.id, &suggestions);
    Ok(())
}

/// Analyse a document: extract its full text, index it for search, and embed it.
///
/// This is the only analyse path that writes to the FTS index rather than (or as well as) the
/// catalog, because for a document the *text is the content*. The order matters: text first, so a
/// document is findable even if the embedding step later fails.
fn analyze_document(
    store: &Store,
    t: &AnalysisTarget,
    abs: &Path,
    det: &dam_media::Detected,
) -> Result<(), String> {
    let MediaAttributes::Document(d) = dam_media::extract_metadata(abs, det) else {
        return Err("document metadata unavailable".into());
    };
    // Re-persist the cheap tier: word/page counts and the excerpt for a document scanned before
    // this pipeline version existed would otherwise stay empty until a rescan.
    store
        .set_media_attrs(&t.id, &MediaAttributes::Document(d.clone()))
        .map_err(|e| e.to_string())?;

    // A scanned-image PDF with no text layer legitimately yields nothing. That is not an error —
    // it is a document we can describe but not read, and it stays findable by filename and tags.
    // The empty string is still *written*: this pass also runs when a document is replaced by an
    // edited version, and returning early here would leave the previous body indexed forever,
    // matching searches for prose the file no longer contains.
    let text = dam_media::extract_text(abs, &det.format).unwrap_or_default();
    if text.is_empty() {
        tracing::debug!(asset = %t.id, "no extractable text; indexing by name only");
    }

    store
        .set_document_text(&t.id, &text)
        .map_err(|e| e.to_string())?;

    // `text_descriptor` returns `None` for text with no usable tokens, so an unreadable document
    // never gets a vector — but a *stale* one from a previous version must not survive either.
    match dam_media::text_descriptor(&text) {
        Some(vec) => store
            .set_embedding(
                &t.id,
                model_free_space(MediaType::Document),
                MediaType::Document,
                &vec,
                "text-hash@1",
            )
            .map_err(|e| e.to_string())?,
        None => store
            .clear_embedding(&t.id, model_free_space(MediaType::Document))
            .map_err(|e| e.to_string())?,
    }

    // Classify by what the document *is* to a project. Filename is the strongest signal here —
    // a `LICENSE` is a licence whatever its prose says — with a text fallback for the rest.
    let name = Path::new(&t.path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let head: String = text.chars().take(600).collect::<String>().to_lowercase();
    let class = if name.contains("licen") || name.contains("copying") || name.contains("eula") {
        "license"
    } else if name.contains("readme") {
        "readme"
    } else if name.contains("changelog") || name.contains("changes") {
        "changelog"
    } else if name.contains("invoice") || name.contains("receipt") || name.contains("order") {
        "receipt"
    } else if head.contains("permission is hereby granted")
        || head.contains("all rights reserved")
        || head.contains("licensed under")
    {
        "license"
    } else {
        "document"
    };
    store
        .set_media_class(&t.id, MediaType::Document, class)
        .map_err(|e| e.to_string())?;

    let mut suggestions: Vec<(&str, f32)> = vec![(class, 0.6)];
    if d.page_count.is_some_and(|p| p > 20) {
        suggestions.push(("long_form", 0.8));
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

/// Owned-vector adapter over the shared in-place L2 normaliser (`dam_media::l2_normalise`), so the
/// stats embedders below can build a vector inline. A zero vector stays zero (cosine 0 = "no signal").
fn normalise(mut v: Vec<f32>) -> Vec<f32> {
    dam_media::l2_normalise(&mut v);
    v
}
