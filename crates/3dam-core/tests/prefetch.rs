//! Prefetch hint (issue #72): `LibraryService::prefetch` warms an asset's thumbnail cache ahead of
//! any client read — so the grid's later HTTP GET is a cache hit. Verified at the engine level:
//! scan, then prefetch, then assert the derivative exists *without ever calling read_thumbnail*.

use dam_api::dto::*;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn unique_tmp() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-prefetch-{}-{}", std::process::id(), nanos))
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
    let lib = EmbeddedLibrary::open(&data_dir).await.unwrap();
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
    let job = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![sid],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    wait_job(&lib, &ctx, &job).await;

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

    // Prefetch at edge 128 — fire-and-forget; the warm happens off-thread.
    lib.prefetch(
        &ctx,
        PrefetchRequest {
            assets: ids,
            edge: Some(128),
        },
    )
    .await
    .unwrap();

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

    let _ = std::fs::remove_dir_all(&tmp);
}
