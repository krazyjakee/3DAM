//! Analyse and convert reach their bytes through the `FileSource::fetch` seam (issue #48).
//!
//! Before this, both pipelines joined a source root onto a stored relative path — a construction
//! that only ever describes a local filesystem. Analysis compounded it by filtering remote assets
//! out of the plan entirely, so an SFTP/SMB asset was never even considered: no embedding, no
//! derived signals, no auto-tags, and no error to explain it.
//!
//! **What is and isn't covered here.** The plan-side fix (remote assets *are* targets; federated
//! ones still aren't) is pinned by unit tests in `dam-store`, which can build a source of any kind
//! without a server. The tests below cover the runner side: that both pipelines now go through
//! `fetch`, and that a fetch that fails degrades **one item** rather than the job. There is no
//! in-tree SFTP/SMB server, so the remote *happy* path is not asserted end-to-end — it shares the
//! exact `fetch` call the scan path has always used, which the local tests here exercise.

use dam_api::dto::*;
use dam_api::id::{AssetId, SourceId};
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unique_tmp() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "3dam-fetchthrough-{}-{}-{}",
        std::process::id(),
        nanos,
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

fn write_png(path: &Path, w: u32, h: u32) {
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x * 7 % 256) as u8, (y * 5 % 256) as u8, 90, 255])
    });
    img.save(path).unwrap();
}

async fn wait_job(lib: &EmbeddedLibrary, ctx: &AuthContext, job: &dam_api::id::JobId) -> JobStatus {
    loop {
        let j = lib.get_job(ctx, job).await.unwrap();
        if matches!(
            j.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        ) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Open a library over `src`, register it, scan it.
async fn scanned(data: &Path, src: &Path) -> (EmbeddedLibrary, SourceId) {
    let lib = EmbeddedLibrary::open_with(data, dam_core::ResourceOptions::ungoverned())
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
    (lib, sid)
}

async fn id_of(lib: &EmbeddedLibrary, ctx: &AuthContext, name: &str) -> AssetId {
    lib.query(ctx, QueryRequest::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|a| a.name == name)
        .unwrap_or_else(|| panic!("{name} not catalogued"))
        .id
}

/// A fetch that fails is one asset's problem. The job still completes, every other asset is still
/// analysed, and the failure is counted rather than swallowed — golden rule 6, now enforced at the
/// point where remote I/O can actually fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unfetchable_asset_degrades_one_item_not_the_job() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for n in ["keep_a.png", "vanishes.png", "keep_b.png"] {
        write_png(&src.join(n), 16, 16);
    }

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let gone = id_of(&lib, &ctx, "vanishes.png").await;
    let kept = id_of(&lib, &ctx, "keep_a.png").await;

    // Remove the bytes after cataloguing — the catalog row survives, the fetch cannot.
    std::fs::remove_file(src.join("vanishes.png")).unwrap();

    let job = lib
        .submit_analyze(
            &ctx,
            AnalyzeRequest {
                assets: vec![],
                force: true,
            },
        )
        .await
        .unwrap();
    let status = wait_job(&lib, &ctx, &job).await;

    assert!(
        matches!(status.state, JobState::Done),
        "one unreadable asset must not fail the job: {:?} {:?}",
        status.state,
        status.error
    );
    assert!(
        lib.get_asset(&ctx, &kept)
            .await
            .unwrap()
            .timestamps
            .analyzed
            .is_some(),
        "the readable assets are still analysed"
    );
    assert!(
        lib.get_asset(&ctx, &gone)
            .await
            .unwrap()
            .timestamps
            .analyzed
            .is_none(),
        "the unfetchable asset is skipped, not marked analysed"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Convert now materialises its input through `fetch` too. One input that cannot be fetched fails
/// its own item and the batch carries on (§1.1) — and the report still names the asset's *logical*
/// location, not the scratch path its bytes were read from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn convert_fails_one_unfetchable_input_and_finishes_the_batch() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    let out = tmp.join("out");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    write_png(&src.join("good.png"), 16, 16);
    write_png(&src.join("missing.png"), 16, 16);

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let good = id_of(&lib, &ctx, "good.png").await;
    let missing = id_of(&lib, &ctx, "missing.png").await;
    std::fs::remove_file(src.join("missing.png")).unwrap();

    let report = lib
        .convert(
            &ctx,
            ConvertRequest {
                inputs: vec![missing, good],
                target: ConvertTarget::Image {
                    format: "jpeg".into(),
                    max_edge: None,
                    quality: None,
                },
                output_dir: out.to_string_lossy().into_owned(),
                dry_run: false,
                on_collision: CollisionRule::Suffix,
            },
        )
        .await
        .unwrap();

    assert_eq!(report.items.len(), 2, "the batch ran both inputs");
    let bad = report.items.iter().find(|i| i.input == missing).unwrap();
    let ok = report.items.iter().find(|i| i.input == good).unwrap();
    assert!(
        matches!(bad.disposition, Disposition::Failed),
        "the unfetchable input fails its own item: {bad:?}"
    );
    assert!(
        matches!(ok.disposition, Disposition::Done),
        "the readable input still converts: {ok:?}"
    );
    assert!(
        bad.input_path.ends_with("missing.png"),
        "the report names the asset's logical location, not a temp file: {}",
        bad.input_path
    );
    assert!(
        out.join("good.jpg").exists(),
        "the successful output landed in the output dir"
    );
    // Non-destructive: convert never writes into the source (golden rule 3).
    assert!(
        !src.join("good.jpg").exists(),
        "convert must not write into the source tree"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A remote source that cannot be reached is *addable* (reachability is a scan-time question) and
/// its failure is recorded on the source rather than raised — so a mixed library keeps working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_remote_source_records_an_error_without_aborting() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    write_png(&src.join("local.png"), 16, 16);

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;

    // Port 1 on loopback: nothing listens, so the connect fails fast and deterministically.
    let remote = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::Sftp,
                uri: "sftp://user@127.0.0.1:1/assets".into(),
                name: Some("dead".into()),
                options: SourceOptions {
                    password: Some("x".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("a remote source may be added while offline");

    let job = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![remote],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    let status = wait_job(&lib, &ctx, &job).await;
    assert!(
        matches!(status.state, JobState::Done),
        "an unreachable source degrades, it does not fail the job: {:?}",
        status.state
    );

    let info = lib
        .list_sources(&ctx)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.id == remote)
        .unwrap();
    assert!(
        matches!(info.state, SourceState::Error { .. }),
        "the unreachable source surfaces its error: {:?}",
        info.state
    );

    // And the local half of the library still analyses cleanly.
    let job = lib
        .submit_analyze(
            &ctx,
            AnalyzeRequest {
                assets: vec![],
                force: true,
            },
        )
        .await
        .unwrap();
    let status = wait_job(&lib, &ctx, &job).await;
    assert!(matches!(status.state, JobState::Done));
    let local = id_of(&lib, &ctx, "local.png").await;
    assert!(lib
        .get_asset(&ctx, &local)
        .await
        .unwrap()
        .timestamps
        .analyzed
        .is_some());

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Convert's source-safety guard (tech-spec 08 §5.1) is **absolute** and must stay that way.
///
/// Issue #80 adds an upload path that deliberately *does* write inside a source tree. The whole
/// argument for why that is safe rests on the two paths sharing no code and this guard never
/// growing an "unless…" clause — so it is worth a test that fails loudly if someone ever relaxes it
/// to make upload easier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn convert_still_refuses_to_write_inside_a_source() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    write_png(&src.join("input.png"), 16, 16);

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let id = id_of(&lib, &ctx, "input.png").await;

    for dest in [src.clone(), src.join("nested/deeper")] {
        let err = lib
            .convert(
                &ctx,
                ConvertRequest {
                    inputs: vec![id],
                    target: ConvertTarget::Image {
                        format: "jpeg".into(),
                        max_edge: None,
                        quality: None,
                    },
                    output_dir: dest.to_string_lossy().into_owned(),
                    dry_run: false,
                    on_collision: CollisionRule::Suffix,
                },
            )
            .await
            .expect_err("convert must refuse an output dir inside a registered source");
        assert!(
            matches!(err, dam_api::LibError::BadRequest(_)),
            "expected a refusal, got {err:?}"
        );
    }
    // Not even a stray temp file: the guard fires before any item is planned.
    assert_eq!(
        std::fs::read_dir(&src).unwrap().count(),
        1,
        "the source tree is untouched"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
