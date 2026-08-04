//! Phase-3 (Automation) end-to-end coverage through the engine (tech-spec 05): the analyze pass
//! derives tileability/perceptual signals + embeddings + auto-tag suggestions; `find_similar` ranks
//! neighbours by embedding cosine; exact + near duplicate grouping surfaces review clusters; and the
//! suggestion accept/reject lifecycle promotes/negates tags and survives re-analysis.

use dam_api::dto::*;
use dam_api::event::{ChangeKind, LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, SourceId};
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unique_tmp() -> PathBuf {
    // Per-process atomic counter as well as a timestamp, for the reason `scan.rs` documents:
    // these tests run in parallel within one process and `as_nanos()` can coincide for two that
    // start in the same clock tick, silently sharing a data dir. The window is only as narrow as
    // `open` is fast, so it widens whenever engine startup gains work.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "3dam-automation-{}-{}-{}",
        std::process::id(),
        nanos,
        n
    ))
}

/// A smooth diagonal gradient — the "reference" image; `b` is a near-identical copy of it.
fn gradient(w: u32, h: u32) -> image::RgbaImage {
    image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x * 4 % 256) as u8, (y * 4 % 256) as u8, 128, 255])
    })
}

async fn wait_job(lib: &EmbeddedLibrary, ctx: &AuthContext, job: &dam_api::id::JobId) {
    loop {
        let j = lib.get_job(ctx, job).await.unwrap();
        if matches!(
            j.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn scan(lib: &EmbeddedLibrary, ctx: &AuthContext, sid: SourceId) {
    let job = lib
        .submit_scan(
            ctx,
            ScanRequest {
                sources: vec![sid],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    wait_job(lib, ctx, &job).await;
}

/// Map filename → asset id for the whole library (small fixtures).
async fn ids_by_name(lib: &EmbeddedLibrary, ctx: &AuthContext) -> HashMap<String, AssetId> {
    let page = lib
        .query(
            ctx,
            QueryRequest {
                page: PageParams {
                    after: None,
                    limit: 100,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    page.items.into_iter().map(|a| (a.name, a.id)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn analyze_similar_dedup_and_review() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // a: gradient. b: gradient with 3 pixels nudged (near-dup of a, different bytes).
    let a = gradient(64, 64);
    a.save(src.join("a.png")).unwrap();
    let mut b = a.clone();
    for i in 0..3u32 {
        b.put_pixel(i, i, image::Rgba([200, 10, 10, 255]));
    }
    b.save(src.join("b.png")).unwrap();
    // c: inverse gradient — visually opposite (low cosine to a).
    image::RgbaImage::from_fn(64, 64, |x, y| {
        image::Rgba([
            (255 - x * 4 % 256) as u8,
            (255 - y * 4 % 256) as u8,
            64,
            255,
        ])
    })
    .save(src.join("c.png"))
    .unwrap();
    // d + d_copy: byte-identical (exact-dup pair). Vertical stripes, distinct from a/b/c.
    let d = image::RgbaImage::from_fn(64, 64, |x, _| {
        let v = if (x / 4) % 2 == 0 { 240 } else { 20 };
        image::Rgba([v, v, v, 255])
    });
    d.save(src.join("d.png")).unwrap();
    std::fs::copy(src.join("d.png"), src.join("d_copy.png")).unwrap();

    let lib =
        EmbeddedLibrary::open_with(&tmp.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap();
    let ctx = AuthContext::embedded();
    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: src.to_string_lossy().into_owned(),
                name: Some("fixtures".into()),
                options: Default::default(),
            },
        )
        .await
        .unwrap();
    scan(&lib, &ctx, sid).await;

    // ── analyze ────────────────────────────────────────────────────────────
    let job = lib
        .submit_analyze(&ctx, AnalyzeRequest::default())
        .await
        .unwrap();
    wait_job(&lib, &ctx, &job).await;

    let ids = ids_by_name(&lib, &ctx).await;
    assert_eq!(ids.len(), 5, "five image fixtures scanned");
    let id_a = ids["a.png"];
    let id_b = ids["b.png"];

    // Derived signals + suggested tags landed on `a` (tech-spec 05 §5, §6, §1.4).
    let asset_a = lib.get_asset(&ctx, &id_a).await.unwrap();
    let MediaAttributes::Image(attrs) = &asset_a.attributes else {
        panic!("expected image attributes");
    };
    assert!(attrs.tileability.is_some(), "tileability derived");
    assert!(attrs.phash.is_some(), "perceptual hash derived");
    assert!(attrs.tile_class.is_some(), "tile class derived");
    assert!(
        !attrs.dominant_colors.is_empty(),
        "dominant colours derived"
    );
    assert!(
        asset_a
            .tags
            .iter()
            .any(|t| t.state == SuggestionState::Pending && t.source == "auto"),
        "at least one auto-suggested tag: {:?}",
        asset_a.tags
    );
    assert!(
        asset_a
            .tags
            .iter()
            .filter(|tag| tag.source == "auto")
            .all(|tag| tag.confidence.is_some()
                && tag.why.as_deref().is_some_and(|why| !why.is_empty())),
        "automation carries visible confidence and why: {:?}",
        asset_a.tags
    );

    // ── find_similar: b is the nearest neighbour of a ────────────────────────
    let sim = lib
        .find_similar(
            &ctx,
            SimilarRequest {
                asset: id_a,
                k: 4,
                filters: Vec::new(),
                local_only: false,
            },
        )
        .await
        .unwrap();
    assert!(!sim.items.is_empty(), "a has neighbours");
    assert_eq!(sim.items[0].asset.id, id_b, "b is a's top neighbour");
    assert!(
        sim.items[0].score > 0.9,
        "near-identical cosine: {}",
        sim.items[0].score
    );
    assert!(
        !sim.items.iter().any(|h| h.asset.id == id_a),
        "self is dropped"
    );
    assert_eq!(sim.items[0].space, "image-stats-v1");

    // ── dedup: exact groups d with d_copy ────────────────────────────────────
    let exact = lib
        .list_duplicates(
            &ctx,
            DupRequest {
                kind: DupKind::Exact,
                media: None,
                limit: 50,
                after: None,
                review: DupReviewFilter::Pending,
            },
        )
        .await
        .unwrap();
    assert_eq!(exact.items.len(), 1, "one exact-dup group");
    let g = &exact.items[0];
    assert_eq!(g.members.len(), 2, "d + d_copy");
    let names: Vec<&str> = g.members.iter().map(|m| m.asset.name.as_str()).collect();
    assert!(names.contains(&"d.png") && names.contains(&"d_copy.png"));

    let oversized_membership = lib
        .duplicate_membership(
            &ctx,
            DupMembershipRequest {
                assets: vec![AssetId::new(); DUP_MEMBERSHIP_ASSET_MAX + 1],
            },
        )
        .await;
    assert!(matches!(
        oversized_membership,
        Err(dam_api::LibError::BadRequest(_))
    ));

    // ── dedup: near groups a with b (high embedding cosine) ──────────────────
    let near = lib
        .list_duplicates(
            &ctx,
            DupRequest {
                kind: DupKind::Near,
                media: Some(MediaType::Image),
                limit: 50,
                after: None,
                review: DupReviewFilter::Pending,
            },
        )
        .await
        .unwrap();
    assert!(
        near.items.iter().any(|grp| {
            let m: Vec<AssetId> = grp.members.iter().map(|x| x.asset.id).collect();
            m.contains(&id_a) && m.contains(&id_b)
        }),
        "a and b form a near-dup group: {near:?}"
    );

    // ── review: accept one suggestion, reject another; both persist ──────────
    let suggested: Vec<String> = asset_a
        .tags
        .iter()
        .filter(|t| t.state == SuggestionState::Pending)
        .map(|t| t.name.clone())
        .collect();
    let accept = suggested[0].clone();
    lib.review_suggestion(
        &ctx,
        SuggestionReview {
            asset: id_a,
            tag: accept.clone(),
            action: ReviewAction::Accept,
        },
    )
    .await
    .unwrap();

    let reject = suggested.get(1).cloned().unwrap_or_else(|| accept.clone());
    if reject != accept {
        lib.review_suggestion(
            &ctx,
            SuggestionReview {
                asset: id_a,
                tag: reject.clone(),
                action: ReviewAction::Reject,
            },
        )
        .await
        .unwrap();
    }

    let after = lib.get_asset(&ctx, &id_a).await.unwrap();
    assert!(
        after.summary.top_tags.contains(&accept),
        "accepted tag is confirmed on the summary: {:?}",
        after.summary.top_tags
    );
    assert!(
        after
            .tags
            .iter()
            .any(|t| t.name == accept && t.state == SuggestionState::Confirmed),
        "accepted tag state is confirmed"
    );

    lib.review_suggestion(
        &ctx,
        SuggestionReview {
            asset: id_a,
            tag: accept.clone(),
            action: ReviewAction::Undo,
        },
    )
    .await
    .unwrap();
    let undone = lib.get_asset(&ctx, &id_a).await.unwrap();
    assert!(!undone.summary.top_tags.contains(&accept));
    assert!(undone
        .tags
        .iter()
        .any(|tag| tag.name == accept && tag.state == SuggestionState::Pending));
    lib.review_suggestion(
        &ctx,
        SuggestionReview {
            asset: id_a,
            tag: accept.clone(),
            action: ReviewAction::Accept,
        },
    )
    .await
    .unwrap();
    if reject != accept {
        assert!(
            after
                .tags
                .iter()
                .any(|t| t.name == reject && t.state == SuggestionState::Rejected),
            "rejected tag state is rejected"
        );
    }

    // ── re-analysis is incremental + respects the reject ─────────────────────
    // No force ⇒ nothing due (already at current version).
    let redo = lib.submit_analyze(&ctx, AnalyzeRequest::default()).await;
    assert!(
        matches!(redo, Err(dam_api::LibError::BadRequest(_))),
        "nothing due to re-analyse"
    );
    // Force re-run must not resurrect the rejected suggestion.
    let job = lib
        .submit_analyze(
            &ctx,
            AnalyzeRequest {
                assets: vec![id_a],
                force: true,
            },
        )
        .await
        .unwrap();
    wait_job(&lib, &ctx, &job).await;
    let reanalysed = lib.get_asset(&ctx, &id_a).await.unwrap();
    if reject != accept {
        assert!(
            reanalysed
                .tags
                .iter()
                .any(|t| t.name == reject && t.state == SuggestionState::Rejected),
            "reject survives a forced re-analysis (§1.4)"
        );
    }
    assert!(
        reanalysed
            .tags
            .iter()
            .any(|t| t.name == accept && t.state == SuggestionState::Confirmed),
        "confirmed tag survives re-analysis"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A directory of only unanalysable-by-decode assets still completes the pass fail-soft, and
/// `find_similar` on an un-embedded asset returns empty rather than erroring (§1.3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn similar_on_unembedded_is_empty() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    // A structurally-broken "png" — decode fails, so no embedding is produced (fail-soft).
    std::fs::write(src.join("broken.png"), b"\x89PNG\r\n\x1a\nnot-a-real-png").unwrap();

    let lib =
        EmbeddedLibrary::open_with(&tmp.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap();
    let ctx = AuthContext::embedded();
    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: src.to_string_lossy().into_owned(),
                name: None,
                options: Default::default(),
            },
        )
        .await
        .unwrap();
    scan(&lib, &ctx, sid).await;
    let job = lib
        .submit_analyze(&ctx, AnalyzeRequest::default())
        .await
        .unwrap();
    wait_job(&lib, &ctx, &job).await;
    // The analyze job finished (fail-soft) even though the one asset couldn't be embedded.
    let js = lib.get_job(&ctx, &job).await.unwrap();
    assert_eq!(js.state, JobState::Done);

    let id = *ids_by_name(&lib, &ctx).await.get("broken.png").unwrap();
    let sim = lib
        .find_similar(
            &ctx,
            SimilarRequest {
                asset: id,
                k: 8,
                filters: Vec::new(),
                local_only: false,
            },
        )
        .await
        .unwrap();
    assert!(sim.items.is_empty(), "un-embedded asset has no neighbours");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Storage maintenance (tech-spec 10 §5): `clear_analysis` drops suggestions + embeddings and marks
/// assets due again while keeping user-confirmed tags; `wipe_catalog` empties the whole catalog
/// (files in sources are never touched — only catalog rows).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maintenance_clear_analysis_and_wipe() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let a = gradient(64, 64);
    a.save(src.join("a.png")).unwrap();
    let mut b = a.clone();
    b.put_pixel(0, 0, image::Rgba([200, 10, 10, 255]));
    b.save(src.join("b.png")).unwrap();

    let lib =
        EmbeddedLibrary::open_with(&tmp.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap();
    let ctx = AuthContext::embedded();
    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: src.to_string_lossy().into_owned(),
                name: Some("fixtures".into()),
                options: Default::default(),
            },
        )
        .await
        .unwrap();
    scan(&lib, &ctx, sid).await;
    let job = lib
        .submit_analyze(&ctx, AnalyzeRequest::default())
        .await
        .unwrap();
    wait_job(&lib, &ctx, &job).await;

    let ids = ids_by_name(&lib, &ctx).await;
    let id_a = ids["a.png"];

    // Confirm one suggested tag — the user-owned state clear_analysis must preserve.
    let asset_a = lib.get_asset(&ctx, &id_a).await.unwrap();
    let confirmed_tag = asset_a
        .tags
        .iter()
        .find(|t| t.state == SuggestionState::Pending)
        .map(|t| t.name.clone())
        .expect("an auto-suggested tag to confirm");
    lib.review_suggestion(
        &ctx,
        SuggestionReview {
            asset: id_a,
            tag: confirmed_tag.clone(),
            action: ReviewAction::Accept,
        },
    )
    .await
    .unwrap();

    // Embeddings exist before the clear (a has a neighbour), and analysis is up to date.
    let before = lib
        .find_similar(
            &ctx,
            SimilarRequest {
                asset: id_a,
                k: 4,
                filters: Vec::new(),
                local_only: false,
            },
        )
        .await
        .unwrap();
    assert!(!before.items.is_empty(), "embeddings present before clear");
    assert_eq!(lib.library_stats(&ctx, None).await.unwrap().unanalyzed, 0);

    // ── clear_analysis ───────────────────────────────────────────────────────
    let report = lib.clear_analysis().await.unwrap();
    assert!(report.embeddings_removed >= 2, "both embeddings dropped");
    assert!(report.suggestions_removed >= 1, "suggestions dropped");

    // Embeddings gone → no neighbours; every asset is due for re-analysis again.
    let after = lib
        .find_similar(
            &ctx,
            SimilarRequest {
                asset: id_a,
                k: 4,
                filters: Vec::new(),
                local_only: false,
            },
        )
        .await
        .unwrap();
    assert!(after.items.is_empty(), "embeddings cleared → no neighbours");
    let stats = lib.library_stats(&ctx, None).await.unwrap();
    assert_eq!(stats.unanalyzed, stats.total, "all assets marked due again");

    // The confirmed tag survived; the suggestions did not.
    let asset_a = lib.get_asset(&ctx, &id_a).await.unwrap();
    assert!(
        asset_a
            .tags
            .iter()
            .any(|t| t.name == confirmed_tag && t.state == SuggestionState::Confirmed),
        "confirmed tag survives clear_analysis: {:?}",
        asset_a.tags
    );
    assert!(
        !asset_a
            .tags
            .iter()
            .any(|t| t.state == SuggestionState::Pending),
        "no suggestions remain"
    );

    // ── wipe_catalog ─────────────────────────────────────────────────────────
    let wipe = lib.wipe_catalog().await.unwrap();
    assert_eq!(wipe.assets_removed, 2);
    assert_eq!(wipe.sources_removed, 1);
    let stats = lib.library_stats(&ctx, None).await.unwrap();
    assert_eq!(stats.total, 0, "catalog emptied");
    assert_eq!(stats.sources, 0, "sources emptied");
    // The source files themselves are untouched (non-destructive invariant).
    assert!(src.join("a.png").exists(), "source files are never deleted");

    let _ = std::fs::remove_dir_all(&tmp);
}

// ── the bounded analysis write stage (issue #138) ────────────────────────────
//
// Analysis used to persist one image through ~28 write transactions (image analysis, embedding, a
// `suggest_tag` round trip per tag, the version stamp). It now accumulates an `AnalysisWrite` per
// asset and commits a bounded batch of them in one transaction. Three properties of that change
// are worth pinning down end-to-end: events follow the commit, a failed item stays replannable,
// and the payload bound — not just the row bound — decides when a batch closes.

/// Open a library on `dir` and register `src` as a local source, scanned.
async fn library_over(dir: &Path, src: &Path) -> (EmbeddedLibrary, AuthContext, SourceId) {
    let lib =
        EmbeddedLibrary::open_with(&dir.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap();
    let ctx = AuthContext::embedded();
    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: src.to_string_lossy().into_owned(),
                name: Some("fixtures".into()),
                options: Default::default(),
            },
        )
        .await
        .unwrap();
    scan(&lib, &ctx, sid).await;
    (lib, ctx, sid)
}

/// Is this asset still *due* for analysis? Read through the planner itself, which is the only
/// public surface that reports on `asset.analysis_version`: a submission with no due target is
/// rejected rather than queued, so `Err(BadRequest)` means "already at `PIPELINE_VERSION`".
async fn is_due(lib: &EmbeddedLibrary, ctx: &AuthContext, assets: Vec<AssetId>) -> bool {
    match lib
        .submit_analyze(
            ctx,
            AnalyzeRequest {
                assets,
                force: false,
            },
        )
        .await
    {
        Ok(job) => {
            wait_job(lib, ctx, &job).await;
            true
        }
        Err(dam_api::LibError::BadRequest(_)) => false,
        Err(other) => panic!("unexpected planner error: {other}"),
    }
}

/// An `AssetChanged { Reanalyzed }` is a promise that the asset's whole derivation is durable, so
/// it may only be emitted on the far side of the batch commit that stamped its analysis version.
///
/// The test reads each asset back the instant its event arrives — while the job is still running,
/// with later assets still uncommitted — and requires the planner to consider it done. Publishing
/// from the compute path (as the pre-#138 pass did) would surface an id whose version stamp was
/// still sitting in an accumulator, and this would catch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reanalyzed_events_follow_the_committed_batch() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    const TILES: u32 = 6;
    for n in 0..TILES {
        image::RgbaImage::from_fn(48, 48, |x, y| {
            image::Rgba([((x * 5 + n * 7) % 256) as u8, (y * 5 % 256) as u8, 96, 255])
        })
        .save(src.join(format!("tile{n}.png")))
        .unwrap();
    }
    let (lib, ctx, _) = library_over(&tmp, &src).await;

    // Subscribe before submitting so no completion can be missed.
    let mut events = lib
        .subscribe(&ctx, SubscribeRequest::default())
        .await
        .unwrap();
    let job = lib
        .submit_analyze(&ctx, AnalyzeRequest::default())
        .await
        .unwrap();

    let mut announced: Vec<AssetId> = Vec::new();
    let mut finished = false;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(60), events.next()).await {
        match event {
            LibraryEvent::AssetChanged {
                id,
                kind: ChangeKind::Reanalyzed,
                ..
            } => {
                let asset = lib.get_asset(&ctx, &id).await.unwrap();
                assert!(
                    asset.timestamps.analyzed.is_some(),
                    "{} was announced as reanalysed with no analysed_at",
                    asset.summary.name
                );
                assert!(
                    !is_due(&lib, &ctx, vec![id]).await,
                    "{} was announced as reanalysed while still due — the event outran its commit",
                    asset.summary.name
                );
                announced.push(id);
            }
            LibraryEvent::JobProgress(status)
                if status.id == job && status.state == JobState::Done =>
            {
                finished = true;
                break;
            }
            _ => {}
        }
    }

    assert!(finished, "the analyse job should reach Done");
    assert_eq!(
        announced.len(),
        TILES as usize,
        "every analysed asset should be announced exactly once"
    );
    announced.sort();
    announced.dedup();
    assert_eq!(announced.len(), TILES as usize, "duplicate announcements");
    assert_eq!(lib.library_stats(&ctx, None).await.unwrap().unanalyzed, 0);

    let _ = std::fs::remove_dir_all(&tmp);
}

/// One asset that cannot be decoded must leave *itself* — and only itself — due for the next pass.
///
/// This is the batching invariant that is easiest to get wrong in both directions: a per-item
/// failure that aborted the transaction would drag its batch-mates back into the plan, and a
/// failure that was merely logged would let the version stamp land anyway and quietly retire an
/// asset nothing was ever derived for. The failure here happens in the decode, before any write
/// exists, which is exactly the pre-#138 path — what changed is that its four batch-mates commit
/// together around it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_analysis_item_leaves_the_asset_replannable() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for n in 0..4u32 {
        image::RgbaImage::from_fn(32, 32, |x, y| {
            image::Rgba([(x * 8 % 256) as u8, ((y * 8 + n) % 256) as u8, 32, 255])
        })
        .save(src.join(format!("mate{n}.png")))
        .unwrap();
    }
    // A structurally broken PNG: `detect` still catalogues it as an image, and the analyse pass
    // fails on the decode — no `AnalysisWrite` is ever produced for it.
    std::fs::write(src.join("broken.png"), b"\x89PNG\r\n\x1a\nnot-a-real-png").unwrap();

    let (lib, ctx, _) = library_over(&tmp, &src).await;
    let job = lib
        .submit_analyze(&ctx, AnalyzeRequest::default())
        .await
        .unwrap();
    wait_job(&lib, &ctx, &job).await;

    let status = lib.get_job(&ctx, &job).await.unwrap();
    assert_eq!(status.state, JobState::Done, "the pass is fail-soft");
    assert_eq!(
        status.warnings.len(),
        1,
        "exactly one asset should be reported as skipped: {:?}",
        status.warnings
    );

    let ids = ids_by_name(&lib, &ctx).await;
    let broken = ids["broken.png"];
    let mates: Vec<AssetId> = ids
        .iter()
        .filter(|(name, _)| name.as_str() != "broken.png")
        .map(|(_, id)| *id)
        .collect();

    assert_eq!(
        lib.library_stats(&ctx, None).await.unwrap().unanalyzed,
        1,
        "only the undecodable asset should still be due"
    );
    assert!(
        !is_due(&lib, &ctx, mates).await,
        "the failed item's batch-mates were dragged back into the plan"
    );
    assert!(
        is_due(&lib, &ctx, vec![broken]).await,
        "the failed item was retired without a derivation"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Documents are the reason the accumulator bounds payload bytes and not just rows.
///
/// Each of these twelve `.txt` files carries more than the 1 MiB body-text cap, so the pass holds
/// ~12 MiB of pending `document_text` against an 8 MiB payload bound and a 64-row bound. The row
/// bound cannot fire — twelve is nowhere near sixty-four — so a batch that closes mid-run closed on
/// bytes, and the progress counters (written inside each batch's own transaction) are where that
/// becomes observable. The age bound can only ever make a batch *smaller*, so both assertions are
/// one-sided and cannot be flaked by a slow machine; what they exclude is the row-only accumulator,
/// under which all twelve documents would legally have been held resident for one commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_batches_respect_the_payload_bound() {
    const DOCS: u64 = 12;
    /// 8 MiB / ~1 MiB of text apiece, plus a little slack for the row and tag overhead the cost
    /// estimate also charges.
    const MAX_DOCS_PER_BATCH: u64 = 9;

    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let mut body = String::with_capacity(1_300_000);
    while body.len() < 1_200_000 {
        body.push_str("lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod ");
    }
    for n in 0..DOCS {
        // A unique leading token per document, so the FTS `text` column written by the batch can be
        // proved to hold each body and not just the last one's. The trailing `zz` matters: search
        // matches the last term as a prefix, so a bare `sentinel1` would also hit `sentinel10`.
        std::fs::write(
            src.join(format!("doc{n}.txt")),
            format!("sentinel{n}zz {body}"),
        )
        .unwrap();
    }

    let (lib, ctx, _) = library_over(&tmp, &src).await;
    let mut events = lib
        .subscribe(&ctx, SubscribeRequest::default())
        .await
        .unwrap();
    let job = lib
        .submit_analyze(&ctx, AnalyzeRequest::default())
        .await
        .unwrap();

    // Every distinct `done` a batch committed, in order. One entry per flush.
    let mut commits: Vec<u64> = Vec::new();
    let mut finished = false;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(120), events.next()).await
    {
        if let LibraryEvent::JobProgress(status) = event {
            if status.id != job {
                continue;
            }
            if status.state == JobState::Running && status.progress.done > 0 {
                if commits.last() != Some(&status.progress.done) {
                    commits.push(status.progress.done);
                }
            } else if matches!(status.state, JobState::Done | JobState::Failed) {
                finished = true;
                break;
            }
        }
    }
    assert!(finished, "the analyse job should reach Done");
    assert_eq!(lib.library_stats(&ctx, None).await.unwrap().unanalyzed, 0);
    assert_eq!(
        lib.get_job(&ctx, &job).await.unwrap().warnings,
        Vec::<String>::new(),
        "every document should have analysed cleanly"
    );

    assert!(
        commits.len() > 1,
        "12 MiB of document text committed in {} batch(es) ({commits:?}) — the payload bound never \
         fired, and the row bound of 64 could not have",
        commits.len()
    );
    let mut previous = 0;
    for done in &commits {
        assert!(
            done - previous <= MAX_DOCS_PER_BATCH,
            "a batch committed {} documents (~{} MiB of pending text) at once: {commits:?}",
            done - previous,
            done - previous
        );
        previous = *done;
    }
    assert_eq!(previous, DOCS, "every document should be accounted for");

    // The text each batch carried really did reach the index, for every document and not just the
    // one that happened to close a batch.
    for n in 0..DOCS {
        let hits = lib
            .query(
                &ctx,
                QueryRequest {
                    text: Some(format!("sentinel{n}zz")),
                    page: PageParams {
                        after: None,
                        limit: 10,
                    },
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            hits.items.len(),
            1,
            "doc{n}.txt's body text is missing from the index"
        );
        assert_eq!(hits.items[0].name, format!("doc{n}.txt"));
    }

    let _ = std::fs::remove_dir_all(&tmp);
}
