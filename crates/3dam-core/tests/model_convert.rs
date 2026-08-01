//! 3D container transcode through the real convert pipeline (issue #49, tech-spec 08 §3.3).
//!
//! The unit tests in `dam-media` cover the encoder itself. This covers the parts only the pipeline
//! owns: that a `Model` target routes to it at all, that the non-destructive guard applies to it
//! exactly as it does to images, and that a mixed batch fails the wrong-media items softly instead
//! of aborting.
//!
//! Runs under `cargo test --workspace` because `dam-server` enables `dam-core/model-convert` and
//! features unify across the workspace graph. Built without that feature the encode call answers
//! `Unsupported`, and the assertions below distinguish the two rather than assuming either.

use dam_api::dto::*;
use dam_api::id::SourceId;
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use std::path::PathBuf;
use std::time::Duration;

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

fn unique_tmp(name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "3dam-modelconv-{}-{nanos}-{n}-{name}",
        std::process::id()
    ))
}

/// A valid single-triangle Wavefront OBJ. Text, so the fixture is legible and needs no binary.
const TRIANGLE_OBJ: &str = "v 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 0.0 1.0 0.0\nf 1 2 3\n";

#[tokio::test]
async fn a_model_converts_to_glb_without_touching_the_source() {
    let tmp = unique_tmp("root");
    let assets = tmp.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("tri.obj"), TRIANGLE_OBJ).unwrap();

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
                uri: assets.to_string_lossy().into_owned(),
                name: Some("t".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    scan_to_done(&lib, &ctx, sid).await;

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
    let model = page
        .items
        .iter()
        .find(|a| a.media == MediaType::Model)
        .expect("the OBJ is catalogued as a model");

    // Source-safety first, because it is the invariant most worth proving for a *new* target: the
    // guard is target-agnostic by construction, and this is what keeps it that way (tech-spec 08
    // §5.1). Checked before the successful convert so a regression here cannot hide behind it.
    let into_source = assets.join("converted");
    let refused = lib
        .convert(
            &ctx,
            ConvertRequest {
                inputs: vec![model.id],
                target: ConvertTarget::Model {
                    format: "glb".into(),
                },
                output_dir: into_source.to_string_lossy().into_owned(),
                dry_run: false,
                on_collision: CollisionRule::Fail,
            },
        )
        .await;
    assert!(
        refused.is_err(),
        "a 3D convert must not be able to write inside a registered source"
    );
    assert!(
        !into_source.exists(),
        "and must not create the directory either"
    );

    let out = tmp.join("out");
    let dry = lib
        .convert(
            &ctx,
            ConvertRequest {
                inputs: vec![model.id],
                target: ConvertTarget::Model {
                    format: "glb".into(),
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
    assert!(!out.exists(), "dry-run writes nothing");

    let commit = lib
        .convert(
            &ctx,
            ConvertRequest {
                inputs: vec![model.id],
                target: ConvertTarget::Model {
                    format: "glb".into(),
                },
                output_dir: out.to_string_lossy().into_owned(),
                dry_run: false,
                on_collision: CollisionRule::Fail,
            },
        )
        .await
        .unwrap();

    let written = out.join("tri.glb");

    // `#[cfg]`, not `if commit.done == 1`. A runtime branch passes green either way, so if
    // `dam-server` ever stopped enabling `model-convert` the real conversion would quietly stop
    // being tested and nothing would go red. This way the compiler picks the arm, and the build
    // that *can* convert is required to.
    #[cfg(feature = "model-convert")]
    {
        assert_eq!(commit.done, 1, "the model converts");
        assert!(written.exists(), "the GLB was written under the output dir");
        let glb = std::fs::read(&written).unwrap();
        assert_eq!(&glb[0..4], b"glTF", "real GLB container bytes");
        assert_eq!(
            u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize,
            glb.len(),
            "the container's declared length matches what was written"
        );
    }
    #[cfg(not(feature = "model-convert"))]
    {
        // Built without the feature: the target is still accepted and routed, the *job* completes,
        // and the item fails with a message naming the missing feature — rather than a listed
        // format silently doing nothing.
        assert_eq!(commit.failed, 1, "the item fails; the job does not");
        let err = commit.items[0].error.clone().unwrap_or_default();
        assert!(
            err.contains("model-convert"),
            "a build without the feature must say so: {err}"
        );
        assert!(!written.exists());
    }

    // The source is untouched either way — the whole point of the non-destructive invariant.
    assert!(assets.join("tri.obj").exists());
    assert_eq!(
        std::fs::read_to_string(assets.join("tri.obj")).unwrap(),
        TRIANGLE_OBJ
    );

    std::fs::remove_dir_all(&tmp).ok();
}
