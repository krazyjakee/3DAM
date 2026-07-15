//! Phase-3 (Automation) end-to-end coverage through the engine (tech-spec 05): the analyze pass
//! derives tileability/perceptual signals + embeddings + auto-tag suggestions; `find_similar` ranks
//! neighbours by embedding cosine; exact + near duplicate grouping surfaces review clusters; and the
//! suggestion accept/reject lifecycle promotes/negates tags and survives re-analysis.

use dam_api::dto::*;
use dam_api::id::{AssetId, SourceId};
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unique_tmp() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-automation-{}-{}", std::process::id(), nanos))
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
            .any(|t| t.state == "suggested" && t.source == "auto"),
        "at least one auto-suggested tag: {:?}",
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
            },
        )
        .await
        .unwrap();
    assert_eq!(exact.len(), 1, "one exact-dup group");
    let g = &exact[0];
    assert_eq!(g.members.len(), 2, "d + d_copy");
    let names: Vec<&str> = g.members.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"d.png") && names.contains(&"d_copy.png"));

    // ── dedup: near groups a with b (high embedding cosine) ──────────────────
    let near = lib
        .list_duplicates(
            &ctx,
            DupRequest {
                kind: DupKind::Near,
                media: Some(MediaType::Image),
                limit: 50,
            },
        )
        .await
        .unwrap();
    assert!(
        near.iter().any(|grp| {
            let m: Vec<AssetId> = grp.members.iter().map(|x| x.id).collect();
            m.contains(&id_a) && m.contains(&id_b)
        }),
        "a and b form a near-dup group: {near:?}"
    );

    // ── review: accept one suggestion, reject another; both persist ──────────
    let suggested: Vec<String> = asset_a
        .tags
        .iter()
        .filter(|t| t.state == "suggested")
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
            .any(|t| t.name == accept && t.state == "confirmed"),
        "accepted tag state is confirmed"
    );
    if reject != accept {
        assert!(
            after
                .tags
                .iter()
                .any(|t| t.name == reject && t.state == "rejected"),
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
                .any(|t| t.name == reject && t.state == "rejected"),
            "reject survives a forced re-analysis (§1.4)"
        );
    }
    assert!(
        reanalysed
            .tags
            .iter()
            .any(|t| t.name == accept && t.state == "confirmed"),
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
        .find(|t| t.state == "suggested")
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
            .any(|t| t.name == confirmed_tag && t.state == "confirmed"),
        "confirmed tag survives clear_analysis: {:?}",
        asset_a.tags
    );
    assert!(
        !asset_a.tags.iter().any(|t| t.state == "suggested"),
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
