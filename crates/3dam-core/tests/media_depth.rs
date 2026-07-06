//! Phase-2 (Media depth) end-to-end coverage through the engine: cheap metadata is extracted at
//! scan and surfaces on `get_asset`/grid rows; image thumbnails render + cache; and the convert
//! pipeline plans (dry-run), commits atomically, and refuses to write into a source (§5.1).

use dam_api::dto::*;
use dam_api::id::SourceId;
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
    std::env::temp_dir().join(format!("3dam-media-depth-{}-{}", std::process::id(), nanos))
}

fn write_png(path: &std::path::Path, w: u32, h: u32) {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([120, 90, 60, 255]));
    img.save_with_format(path, image::ImageFormat::Png).unwrap();
}

fn write_wav(path: &std::path::Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for _ in 0..44_100 {
        w.write_sample(0i16).unwrap();
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
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

#[tokio::test]
async fn metadata_thumbnail_and_convert() {
    let tmp = unique_tmp();
    let assets = tmp.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    write_png(&assets.join("tex.png"), 128, 64);
    write_wav(&assets.join("tone.wav"));
    std::fs::write(
        assets.join("tri.gltf"),
        br#"{"meshes":[{"primitives":[{"attributes":{"POSITION":0},"indices":1}]}],"accessors":[{"count":3},{"count":3}]}"#,
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
                name: Some("t".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    scan_to_done(&lib, &ctx, sid).await;

    // ── cheap metadata surfaces on the grid + inspector ──────────────────────
    let page = lib
        .query(
            &ctx,
            QueryRequest {
                page: PageParams {
                    after: None,
                    limit: 50,
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3);

    let img = page
        .items
        .iter()
        .find(|a| a.media == MediaType::Image)
        .unwrap();
    assert_eq!(
        img.key_attrs.get("dimensions").map(String::as_str),
        Some("128×64"),
        "image grid row carries dimensions"
    );

    let full = lib.get_asset(&ctx, &img.id).await.unwrap();
    let MediaAttributes::Image(attrs) = &full.attributes else {
        panic!(
            "image asset should have image attributes, got {:?}",
            full.attributes
        )
    };
    assert_eq!(attrs.width, Some(128));
    assert_eq!(attrs.height, Some(64));

    let audio = page
        .items
        .iter()
        .find(|a| a.media == MediaType::Audio)
        .unwrap();
    let audio_full = lib.get_asset(&ctx, &audio.id).await.unwrap();
    let MediaAttributes::Audio(a) = &audio_full.attributes else {
        panic!("audio attributes missing")
    };
    assert_eq!(a.sample_rate, Some(44_100));
    assert_eq!(a.channels, Some(2));
    assert_eq!(a.duration_ms, Some(1000));
    assert_eq!(a.codec.as_deref(), Some("pcm_s16le"));
    assert_eq!(a.container.as_deref(), Some("wav"));

    let model = page
        .items
        .iter()
        .find(|a| a.media == MediaType::Model)
        .unwrap();
    let model_full = lib.get_asset(&ctx, &model.id).await.unwrap();
    let MediaAttributes::Model(m) = &model_full.attributes else {
        panic!("model attributes missing")
    };
    assert_eq!(m.vertex_count, Some(3));
    assert_eq!(m.triangle_count, Some(1));

    // ── thumbnails: image renders + caches; audio/3D refuse (island previews) ─
    let thumb = lib.read_thumbnail(&ctx, &img.id, 64).await.unwrap();
    assert_eq!(thumb.content_type, "image/png");
    assert!(thumb.bytes.starts_with(b"\x89PNG"));
    // Second call must hit the on-disk cache and return identical bytes.
    let thumb2 = lib.read_thumbnail(&ctx, &img.id, 64).await.unwrap();
    assert_eq!(thumb.bytes, thumb2.bytes);
    let cache_dir = tmp.join("data").join("cache").join("thumbnails");
    assert!(
        std::fs::read_dir(&cache_dir)
            .map(|d| d.count() > 0)
            .unwrap_or(false),
        "a thumbnail file is cached under data/cache/thumbnails"
    );
    assert!(
        lib.read_thumbnail(&ctx, &audio.id, 64).await.is_err(),
        "audio has no server thumbnail (waveform is a WASM island)"
    );

    // ── convert: dry-run plans, commit writes, source-safety holds ───────────
    let out = tmp.join("out");
    let dry = lib
        .convert(
            &ctx,
            ConvertRequest {
                inputs: vec![img.id],
                target: ConvertTarget::Image {
                    format: "jpg".into(),
                    max_edge: Some(32),
                    quality: Some(80),
                },
                output_dir: out.to_string_lossy().into_owned(),
                dry_run: true,
                on_collision: CollisionRule::Fail,
            },
        )
        .await
        .unwrap();
    assert!(dry.dry_run);
    assert_eq!(dry.items.len(), 1);
    assert_eq!(dry.items[0].disposition, Disposition::Write);
    assert!(!out.exists(), "dry-run writes nothing");

    let commit = lib
        .convert(
            &ctx,
            ConvertRequest {
                inputs: vec![img.id, audio.id],
                target: ConvertTarget::Image {
                    format: "jpg".into(),
                    max_edge: Some(32),
                    quality: Some(80),
                },
                output_dir: out.to_string_lossy().into_owned(),
                dry_run: false,
                on_collision: CollisionRule::Fail,
            },
        )
        .await
        .unwrap();
    assert_eq!(commit.done, 1, "the image converts");
    assert_eq!(
        commit.unsupported, 1,
        "the audio input is wrong media for an image target"
    );
    let written = out.join("tex.jpg");
    assert!(
        written.exists(),
        "the JPEG was written under the output dir"
    );
    let jpg = std::fs::read(&written).unwrap();
    assert!(jpg.starts_with(&[0xFF, 0xD8]), "real JPEG bytes");

    // Source-safety: an output dir inside the registered source is rejected outright (§5.1).
    let into_source = assets.join("converted");
    let err = lib
        .convert(
            &ctx,
            ConvertRequest {
                inputs: vec![img.id],
                target: ConvertTarget::Image {
                    format: "png".into(),
                    max_edge: None,
                    quality: None,
                },
                output_dir: into_source.to_string_lossy().into_owned(),
                dry_run: false,
                on_collision: CollisionRule::Fail,
            },
        )
        .await;
    assert!(err.is_err(), "refuses to write into a source tree");

    std::fs::remove_dir_all(&tmp).ok();
}
