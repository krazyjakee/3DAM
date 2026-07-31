//! Server-side waveform peaks (issue #73): the analysis pass computes a normalised peak array for
//! each audio asset and it surfaces on the asset metadata DTO — so clients draw the waveform without
//! decoding the audio themselves.

use dam_api::dto::*;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use std::f32::consts::PI;
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
    std::env::temp_dir().join(format!("3dam-peaks-{}-{}-{}", std::process::id(), nanos, n))
}

/// A 1-second 440 Hz sine sweep with a rising envelope — non-trivial amplitude so the peaks vary.
fn write_tone_wav(path: &Path) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    let n = 44_100u32;
    for i in 0..n {
        let t = i as f32 / n as f32;
        let env = t; // ramp 0→1 so later buckets peak higher than earlier ones
        let s = (2.0 * PI * 440.0 * t).sin() * env;
        w.write_sample((s * i16::MAX as f32) as i16).unwrap();
    }
    w.finalize().unwrap();
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
async fn analysis_computes_waveform_peaks_on_the_dto() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    write_tone_wav(&src.join("tone.wav"));

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

    let job = lib
        .submit_analyze(&ctx, AnalyzeRequest::default())
        .await
        .unwrap();
    wait_job(&lib, &ctx, &job).await;

    let page = lib.query(&ctx, QueryRequest::default()).await.unwrap();
    let id = page.items[0].id;
    let asset = lib.get_asset(&ctx, &id).await.unwrap();
    let MediaAttributes::Audio(a) = &asset.attributes else {
        panic!("expected audio attributes");
    };

    let peaks = a
        .peaks
        .as_ref()
        .expect("waveform peaks computed by analysis");
    assert_eq!(peaks.len(), 256, "canonical bucket count");
    assert!(
        peaks.iter().any(|&p| p > 0.0),
        "a tone yields non-zero peaks"
    );
    assert!(
        peaks.iter().all(|&p| (0.0..=1.0).contains(&p)),
        "peaks are normalised to 0..1"
    );
    // The rising envelope means the tail is louder than the head.
    let head: f32 = peaks[..32].iter().copied().fold(0.0, f32::max);
    let tail: f32 = peaks[224..].iter().copied().fold(0.0, f32::max);
    assert!(
        tail > head,
        "rising-envelope tone peaks higher near the end"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
