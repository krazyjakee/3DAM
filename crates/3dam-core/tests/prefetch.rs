//! Prefetch hint (issue #72): `LibraryService::prefetch` warms an asset's thumbnail cache ahead of
//! any client read — so the grid's later HTTP GET is a cache hit. Verified at the engine level:
//! seed the catalog, then prefetch, then assert the derivative exists *without ever calling
//! read_thumbnail*.

use dam_api::dto::*;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        "3dam-prefetch-{}-{}-{}",
        std::process::id(),
        nanos,
        n
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetch_warms_the_thumbnail_cache() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    image::RgbaImage::from_fn(48, 48, |x, y| {
        image::Rgba([(x * 5 % 256) as u8, (y * 5 % 256) as u8, 200, 255])
    })
    .save(src.join("tile.png"))
    .unwrap();

    let data_dir = tmp.join("data");
    let lib = EmbeddedLibrary::open_with(&data_dir, dam_core::ResourceOptions::ungoverned())
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
    // Seed the one catalog row directly. This test exercises prefetch bounds, not scan pacing;
    // bypassing scan keeps it deterministic when the host's load governor is intentionally active.
    let store = dam_store::Store::open(&data_dir).unwrap();
    store
        .upsert_asset(&dam_store::NewAsset {
            source_id: sid,
            path: "tile.png".into(),
            filename: "tile.png".into(),
            content_hash: None,
            size_bytes: Some(std::fs::metadata(src.join("tile.png")).unwrap().len() as i64),
            source_modified_at: None,
            scanned_at: dam_store::now_ms(),
            media_type: MediaType::Image,
            format: "png".into(),
        })
        .unwrap();
    drop(store);

    let ids: Vec<_> = lib
        .query(&ctx, QueryRequest::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert!(!ids.is_empty());

    // The cache is cold — no thumbnail has been read yet.
    let thumb_dir = data_dir.join("cache").join("thumbnails");
    assert!(
        !thumb_dir.exists() || std::fs::read_dir(&thumb_dir).unwrap().next().is_none(),
        "no thumbnail should exist before prefetch"
    );

    // A burst of duplicate-heavy hints must return promptly and collapse to one bounded worker +
    // one derivative. This exercises input/queue/task bounds without relying on timing CPU work.
    let id = ids[0];
    let mut synthetic = Vec::with_capacity(120_001);
    synthetic.push(id);
    let synthetic_id = dam_api::id::AssetId::new();
    synthetic.extend(std::iter::repeat_n(synthetic_id, 120_000));
    let started = Instant::now();
    lib.prefetch(
        &ctx,
        PrefetchRequest {
            assets: synthetic,
            edge: Some(128),
            relay: false,
        },
    )
    .await
    .unwrap();
    let calls = (0..64).map(|_| {
        lib.prefetch(
            &ctx,
            PrefetchRequest {
                assets: vec![id; 1_000],
                edge: Some(128),
                relay: false,
            },
        )
    });
    for result in futures::future::join_all(calls).await {
        result.unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "prefetch hints must enqueue rather than perform decode work inline"
    );
    let (max_pending, max_workers) = lib.prefetch_bound_diagnostics();
    assert!(max_pending <= 1_024, "pending queue exceeded its hard cap");
    assert_eq!(max_workers, 1, "all hints must share one local worker task");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut warmed = false;
    while Instant::now() < deadline {
        if thumb_dir.exists()
            && std::fs::read_dir(&thumb_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .any(|e| e.path().to_string_lossy().contains("-128"))
                })
                .unwrap_or(false)
        {
            warmed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        warmed,
        "prefetch warmed the 128px thumbnail before any read"
    );
    assert_eq!(
        std::fs::read_dir(&thumb_dir).unwrap().flatten().count(),
        1,
        "duplicate bursts publish exactly one derivative"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
