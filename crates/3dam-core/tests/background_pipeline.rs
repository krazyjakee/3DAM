//! Hosted-mode background pipeline (issue #71): a scan alone — with no client submitting analysis or
//! requesting a thumbnail — must leave the catalog with analysis done *and* thumbnails pre-rendered,
//! because the always-on server pipeline drains ingest work in the background.

use dam_api::dto::*;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::{EmbeddedLibrary, PipelinePolicy};
use std::path::PathBuf;
use std::sync::Arc;
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
        "3dam-pipeline-{}-{}-{}",
        std::process::id(),
        nanos,
        n
    ))
}

/// Both toggles on — the default hosted-mode posture.
struct AllOn;
impl PipelinePolicy for AllOn {
    fn auto_thumbnail(&self) -> bool {
        true
    }
    fn auto_analyze(&self) -> bool {
        true
    }
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

/// Have all scanned image assets acquired their derived analysis signals yet?
async fn all_analyzed(lib: &EmbeddedLibrary, ctx: &AuthContext) -> bool {
    let page = match lib.query(ctx, QueryRequest::default()).await {
        Ok(p) => p,
        Err(_) => return false,
    };
    if page.items.is_empty() {
        return false;
    }
    for a in page.items {
        let Ok(asset) = lib.get_asset(ctx, &a.id).await else {
            return false;
        };
        if let MediaAttributes::Image(attrs) = &asset.attributes {
            if attrs.tileability.is_none() {
                return false;
            }
        }
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_alone_populates_analysis_and_thumbnails() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // Two distinct images — enough to exercise the drain over more than one asset.
    image::RgbaImage::from_fn(64, 64, |x, y| {
        image::Rgba([(x * 4 % 256) as u8, (y * 4 % 256) as u8, 128, 255])
    })
    .save(src.join("a.png"))
    .unwrap();
    image::RgbaImage::from_fn(64, 64, |x, _| {
        let v = if (x / 4) % 2 == 0 { 240 } else { 20 };
        image::Rgba([v, v, v, 255])
    })
    .save(src.join("b.png"))
    .unwrap();

    let data_dir = tmp.join("data");
    let lib = Arc::new(
        EmbeddedLibrary::open_with(&data_dir, dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
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

    // Start the always-on pipeline (as `serve` would), then scan — and do *nothing else*. No
    // submit_analyze, no read_thumbnail: the pipeline must do it all off the scan-completed event.
    lib.start_background_pipeline(Arc::new(AllOn));

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

    // Analysis ran with no client interaction: derived image signals landed on the assets.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut analyzed = false;
    while Instant::now() < deadline {
        if all_analyzed(&lib, &ctx).await {
            analyzed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(analyzed, "background pipeline analysed the scanned assets");

    // Thumbnails pre-rendered with no client request: the content-keyed cache dir now holds PNGs.
    let thumb_dir = data_dir.join("cache").join("thumbnails");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut has_thumbs = false;
    while Instant::now() < deadline {
        let found = std::fs::read_dir(&thumb_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .any(|e| e.path().extension().is_some_and(|x| x == "png"))
            })
            .unwrap_or(false);
        if found {
            has_thumbs = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(has_thumbs, "background pipeline pre-rendered thumbnails");

    let _ = std::fs::remove_dir_all(&tmp);
}
