//! End-to-end 3D model thumbnail render through the engine (Phase: 3D grid previews).
//!
//! Gated on the `render` feature — the whole file compiles out when 3D rendering is disabled (the
//! CLI/GUI default), and runs under `cargo test --workspace` where `dam-server` turns it on. It
//! scans a real glTF cube, reads its thumbnail through `LibraryService::read_thumbnail` (the same
//! call the web grid makes), and asserts a cached PNG plus fail-soft behaviour on a corrupt model.
#![cfg(feature = "render")]

use dam_api::dto::*;
use dam_api::id::SourceId;
use dam_api::service::{AuthContext, LibraryService};
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A minimal but valid glTF cube with an embedded base64 buffer (8 verts, 12 triangles). Kept in
/// lockstep with `crates/3dam-render/tests/fixtures/cube.gltf`.
const CUBE_BUFFER_B64: &str = "AACAvwAAgL8AAIC/AACAPwAAgL8AAIC/AACAPwAAgD8AAIC/AACAvwAAgD8AAIC/AACAvwAAgL8AAIA/AACAPwAAgL8AAIA/AACAPwAAgD8AAIA/AACAvwAAgD8AAIA/AAABAAIAAAACAAMABAAGAAUABAAHAAYAAAAEAAUAAAAFAAEAAwACAAYAAwAGAAcAAAADAAcAAAAHAAQAAQAFAAYAAQAGAAIA";

fn cube_gltf() -> String {
    format!(
        r#"{{
  "asset": {{"version": "2.0"}},
  "scene": 0,
  "scenes": [{{"nodes": [0]}}],
  "nodes": [{{"mesh": 0}}],
  "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0}}, "indices": 1, "mode": 4}}]}}],
  "accessors": [
    {{"bufferView": 0, "componentType": 5126, "count": 8, "type": "VEC3", "min": [-1,-1,-1], "max": [1,1,1]}},
    {{"bufferView": 1, "componentType": 5123, "count": 36, "type": "SCALAR"}}
  ],
  "bufferViews": [
    {{"buffer": 0, "byteOffset": 0, "byteLength": 96, "target": 34962}},
    {{"buffer": 0, "byteOffset": 96, "byteLength": 72, "target": 34963}}
  ],
  "buffers": [{{"byteLength": 168, "uri": "data:application/octet-stream;base64,{CUBE_BUFFER_B64}"}}]
}}"#
    )
}

fn unique_tmp() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-model-thumb-{}-{}", std::process::id(), nanos))
}

async fn scan_to_done(lib: &EmbeddedLibrary, ctx: &AuthContext, sid: SourceId) {
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
    loop {
        let j = lib.get_job(ctx, &job).await.unwrap();
        if matches!(j.state, JobState::Done | JobState::Failed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn asset_by_format(lib: &EmbeddedLibrary, ctx: &AuthContext, format: &str) -> AssetSummary {
    let page = lib.query(ctx, QueryRequest::default()).await.unwrap();
    page.items
        .into_iter()
        .find(|a| a.format == format)
        .unwrap_or_else(|| panic!("scanned asset with format {format} present"))
}

#[tokio::test]
async fn model_thumbnail_renders_caches_and_fails_soft() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("cube.gltf"), cube_gltf()).unwrap();
    // A structurally broken model to prove per-asset fail-soft (never poisons the batch).
    std::fs::write(
        assets.join("broken.glb"),
        b"glTF\x00\x00\x00\x00not-a-real-glb",
    )
    .unwrap();

    let lib = EmbeddedLibrary::open(&tmp.join("data")).await.unwrap();
    let ctx = AuthContext::embedded();
    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: assets.to_string_lossy().into_owned(),
                name: Some("m".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    scan_to_done(&lib, &ctx, sid).await;

    let cube = asset_by_format(&lib, &ctx, "gltf").await;

    let thumb = match lib.read_thumbnail(&ctx, &cube.id, 96).await {
        Ok(t) => t,
        Err(LibError::Unsupported(_)) => {
            eprintln!("skipping: no GPU/software adapter for 3D render in this environment");
            std::fs::remove_dir_all(&tmp).ok();
            return;
        }
        Err(e) => panic!("unexpected thumbnail error: {e:?}"),
    };
    assert_eq!(thumb.content_type, "image/png");
    assert!(thumb.bytes.starts_with(b"\x89PNG"), "real PNG bytes");

    // Decodes to the requested square and isn't a single flat colour (the cube drew).
    let img = image::load_from_memory(&thumb.bytes).expect("valid PNG");
    assert_eq!((img.width(), img.height()), (96, 96));

    // Second call hits the on-disk cache and returns identical bytes.
    let thumb2 = lib.read_thumbnail(&ctx, &cube.id, 96).await.unwrap();
    assert_eq!(thumb.bytes, thumb2.bytes);

    // The cached file carries the renderer-version suffix so a shader bump invalidates it.
    let cache_dir = tmp.join("data").join("cache").join("thumbnails");
    let has_versioned = std::fs::read_dir(&cache_dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .contains(&format!("-r{}", dam_render::RENDER_VERSION))
        });
    assert!(
        has_versioned,
        "model thumbnail cached with -r<version> suffix"
    );

    // Fail-soft: the broken GLB yields a typed error, not a panic or a poisoned scan.
    let broken = asset_by_format(&lib, &ctx, "glb").await;
    assert!(
        lib.read_thumbnail(&ctx, &broken.id, 96).await.is_err(),
        "a corrupt model degrades to a typed error (fail-soft)"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

#[tokio::test]
async fn model_preview_serves_dmsh_blob_and_caches() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("cube.gltf"), cube_gltf()).unwrap();
    std::fs::write(
        assets.join("broken.glb"),
        b"glTF\x00\x00\x00\x00not-a-real-glb",
    )
    .unwrap();

    let lib = EmbeddedLibrary::open(&tmp.join("data")).await.unwrap();
    let ctx = AuthContext::embedded();
    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: assets.to_string_lossy().into_owned(),
                name: Some("m".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    scan_to_done(&lib, &ctx, sid).await;

    // The interactive preview is a CPU decode (no GPU), so unlike the thumbnail it always resolves.
    let cube = asset_by_format(&lib, &ctx, "gltf").await;
    let preview = lib
        .read_model_preview(&ctx, &cube.id)
        .await
        .expect("model preview blob");
    assert_eq!(preview.content_type, "model/x-dam-preview");
    assert!(
        preview.bytes.starts_with(b"DMSH"),
        "self-contained DMSH mesh blob"
    );

    // Second call hits the on-disk cache and returns identical bytes.
    let preview2 = lib.read_model_preview(&ctx, &cube.id).await.unwrap();
    assert_eq!(preview.bytes, preview2.bytes);

    // Cached under the versioned preview slice so a serializer bump invalidates only it.
    let cache_dir = tmp.join("data").join("cache").join("previews");
    let has_versioned = std::fs::read_dir(&cache_dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .contains(&format!("-p{}", dam_render::PREVIEW_VERSION))
        });
    assert!(has_versioned, "preview cached with -p<version> suffix");

    // Fail-soft: a corrupt model degrades to a typed error, not a panic.
    let broken = asset_by_format(&lib, &ctx, "glb").await;
    assert!(
        lib.read_model_preview(&ctx, &broken.id).await.is_err(),
        "a corrupt model degrades to a typed error (fail-soft)"
    );

    std::fs::remove_dir_all(&tmp).ok();
}
