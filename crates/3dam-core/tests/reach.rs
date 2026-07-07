//! Phase-4 (Reach) end-to-end coverage through the engine (tech-spec 07, plus smart folders and
//! export/manifests): delta re-scan skips unchanged files and marks vanished ones absent; the local
//! watcher auto-rescans on a filesystem change; smart folders resolve a saved query live and manual
//! collections hold an explicit set; and export emits JSON/CSV/sidecar manifests over a selector.
//!
//! SFTP/SMB backends ride the same seam but need a live server, so they're covered by the
//! `dam-sources` unit tests (URI parsing / secret hygiene) rather than here.

use dam_api::dto::*;
use dam_api::id::{AssetId, JobId};
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unique_tmp() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-reach-{}-{}", std::process::id(), nanos))
}

fn png(path: &std::path::Path, seed: u8) {
    image::RgbaImage::from_fn(16, 16, |x, y| {
        image::Rgba([seed.wrapping_add(x as u8), y as u8, 128, 255])
    })
    .save(path)
    .unwrap();
}

async fn wait_job(lib: &EmbeddedLibrary, ctx: &AuthContext, job: &JobId) -> JobStatus {
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

async fn scan(lib: &EmbeddedLibrary, ctx: &AuthContext, mode: ScanMode) -> JobStatus {
    let job = lib
        .submit_scan(
            ctx,
            ScanRequest {
                sources: Vec::new(),
                mode,
            },
        )
        .await
        .unwrap();
    wait_job(lib, ctx, &job).await
}

async fn all_names(lib: &EmbeddedLibrary, ctx: &AuthContext) -> Vec<String> {
    let page = lib
        .query(
            ctx,
            QueryRequest {
                page: PageParams {
                    after: None,
                    limit: 200,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut names: Vec<String> = page.items.into_iter().map(|a| a.name).collect();
    names.sort();
    names
}

async fn open_with_source(
    tmp: &std::path::Path,
    watch: bool,
) -> (EmbeddedLibrary, AuthContext, PathBuf) {
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let lib = EmbeddedLibrary::open(&tmp.join("data")).await.unwrap();
    let ctx = AuthContext::embedded();
    lib.add_source(
        &ctx,
        AddSource {
            kind: SourceKind::LocalFs,
            uri: src.to_string_lossy().into_owned(),
            name: Some("fixtures".into()),
            options: SourceOptions {
                watch,
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    (lib, ctx, src)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delta_scan_skips_unchanged_and_marks_missing() {
    let tmp = unique_tmp();
    let (lib, ctx, src) = open_with_source(&tmp, false).await;

    png(&src.join("a.png"), 1);
    let full = scan(&lib, &ctx, ScanMode::Full).await;
    assert_eq!(full.state, JobState::Done);
    assert_eq!(all_names(&lib, &ctx).await, vec!["a.png"]);

    // A second file arrives; delta picks it up and reports `a.png` as unchanged (not re-opened).
    png(&src.join("b.png"), 2);
    let delta = scan(&lib, &ctx, ScanMode::Delta).await;
    assert_eq!(all_names(&lib, &ctx).await, vec!["a.png", "b.png"]);
    assert!(
        delta.error.as_deref().unwrap_or("").contains("unchanged"),
        "delta note should mention unchanged files: {:?}",
        delta.error
    );

    // Remove a file; a delta re-scan marks it missing (non-destructive — the row persists).
    std::fs::remove_file(src.join("a.png")).unwrap();
    let delta2 = scan(&lib, &ctx, ScanMode::Delta).await;
    assert!(
        delta2.error.as_deref().unwrap_or("").contains("missing"),
        "delta note should mention missing files: {:?}",
        delta2.error
    );
    // Grouping only — the catalog row is kept (still listed), never deleted (§2.2).
    assert_eq!(all_names(&lib, &ctx).await, vec!["a.png", "b.png"]);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_watch_auto_rescans_on_change() {
    let tmp = unique_tmp();
    let (lib, ctx, src) = open_with_source(&tmp, true).await;

    // No manual scan: drop a file in and the watcher should pick it up (debounced delta re-scan).
    png(&src.join("watched.png"), 3);

    let mut found = false;
    for _ in 0..80 {
        // up to ~8s
        if all_names(&lib, &ctx)
            .await
            .contains(&"watched.png".to_string())
        {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found,
        "watcher should auto-scan the new file within the timeout"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_and_smart_collections() {
    let tmp = unique_tmp();
    let (lib, ctx, src) = open_with_source(&tmp, false).await;
    png(&src.join("a.png"), 1);
    png(&src.join("b.png"), 2);
    png(&src.join("c.png"), 3);
    scan(&lib, &ctx, ScanMode::Full).await;
    let a = id_of(&lib, &ctx, "a.png").await;
    let b = id_of(&lib, &ctx, "b.png").await;

    // ── manual collection: explicit members ──────────────────────────────────
    let manual = lib
        .create_collection(
            &ctx,
            NewCollection {
                name: "hero".into(),
                kind: CollectionKind::Manual,
                query: None,
            },
        )
        .await
        .unwrap();
    lib.modify_collection_members(
        &ctx,
        &manual,
        CollectionMembers {
            add: vec![a, b],
            remove: Vec::new(),
        },
    )
    .await
    .unwrap();
    let page = lib
        .collection_assets(
            &ctx,
            &manual,
            PageParams {
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2, "manual collection has both members");
    // Membership surfaces on the inspector record.
    assert!(lib
        .get_asset(&ctx, &a)
        .await
        .unwrap()
        .collections
        .contains(&manual));
    // Exact member count on the record.
    assert_eq!(
        lib.get_collection(&ctx, &manual).await.unwrap().count,
        Some(2)
    );

    // Removing a member is reflected live.
    lib.modify_collection_members(
        &ctx,
        &manual,
        CollectionMembers {
            add: Vec::new(),
            remove: vec![a],
        },
    )
    .await
    .unwrap();
    let page = lib
        .collection_assets(
            &ctx,
            &manual,
            PageParams {
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1, "member removed");

    // ── smart folder: a live saved query ─────────────────────────────────────
    let query = QueryRequest {
        filters: vec![Filter {
            field: FacetField::MediaType,
            op: FilterOp::Eq,
            value: FilterValue::Str("image".into()),
        }],
        ..Default::default()
    };
    let smart = lib
        .create_collection(
            &ctx,
            NewCollection {
                name: "all-images".into(),
                kind: CollectionKind::Smart,
                query: Some(query),
            },
        )
        .await
        .unwrap();
    let page = lib
        .collection_assets(
            &ctx,
            &smart,
            PageParams {
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        3,
        "smart folder resolves live to all three images"
    );
    assert_eq!(
        lib.get_collection(&ctx, &smart).await.unwrap().count,
        Some(3),
        "smart folder reports its live match count"
    );
    // A smart folder's membership is query-driven — direct edits are rejected.
    assert!(lib
        .modify_collection_members(
            &ctx,
            &smart,
            CollectionMembers {
                add: vec![a],
                remove: Vec::new()
            },
        )
        .await
        .is_err());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_manifests_json_csv_sidecar() {
    let tmp = unique_tmp();
    let (lib, ctx, src) = open_with_source(&tmp, false).await;
    png(&src.join("one.png"), 1);
    png(&src.join("two.png"), 2);
    scan(&lib, &ctx, ScanMode::Full).await;

    // JSON manifest over the whole library.
    let json_out = tmp.join("manifest.json");
    let rep = lib
        .export(
            &ctx,
            ExportRequest {
                format: ExportFormat::Json,
                output: json_out.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(rep.assets, 2);
    assert_eq!(rep.files_written, 1);
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json_out).unwrap()).unwrap();
    assert_eq!(
        doc["assets"].as_array().unwrap().len(),
        2,
        "json manifest lists both assets"
    );
    assert!(
        doc["assets"][0]["hash"].is_string(),
        "manifest row carries the content hash"
    );

    // CSV manifest — one header line + one row per asset.
    let csv_out = tmp.join("manifest.csv");
    lib.export(
        &ctx,
        ExportRequest {
            format: ExportFormat::Csv,
            output: csv_out.to_string_lossy().into_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let csv = std::fs::read_to_string(&csv_out).unwrap();
    assert_eq!(csv.lines().count(), 3, "csv has header + 2 rows: {csv}");

    // Sidecar — one JSON file per asset under the output dir.
    let side_dir = tmp.join("sidecars");
    let rep = lib
        .export(
            &ctx,
            ExportRequest {
                format: ExportFormat::Sidecar,
                output: side_dir.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(rep.files_written, 2);
    let sidecars: Vec<_> = std::fs::read_dir(&side_dir).unwrap().collect();
    assert_eq!(sidecars.len(), 2, "one sidecar json per asset");

    let _ = std::fs::remove_dir_all(&tmp);
}

async fn id_of(lib: &EmbeddedLibrary, ctx: &AuthContext, name: &str) -> AssetId {
    lib.query(
        ctx,
        QueryRequest {
            page: PageParams {
                after: None,
                limit: 200,
            },
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .items
    .into_iter()
    .find(|a| a.name == name)
    .unwrap_or_else(|| panic!("asset {name} not found"))
    .id
}
