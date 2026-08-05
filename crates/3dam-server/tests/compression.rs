//! Response-compression integration coverage (issue #149). Requests traverse the real application
//! router, so these assertions cover Axum body conversion, negotiation, the compression predicate,
//! and the final wire bytes together.

mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use dam_api::dto::{
    AddSource, JobState, QueryRequest, ScanMode, ScanRequest, SourceKind, SourceOptions,
};
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use support::unique_tmp;
use tower::ServiceExt;

/// The shared router; these tests only ever talk to it as the local owner.
async fn harness() -> axum::Router {
    support::harness(true).await.0
}

async fn get(app: &axum::Router, path: &str, accept_encoding: Option<&str>) -> Response {
    let mut request = Request::builder().uri(path);
    if let Some(value) = accept_encoding {
        request = request.header(header::ACCEPT_ENCODING, value);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn response_bytes(response: Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn decode_brotli(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    brotli::Decompressor::new(input, 4096)
        .read_to_end(&mut output)
        .unwrap();
    output
}

fn decode_gzip(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    flate2::read::GzDecoder::new(input)
        .read_to_end(&mut output)
        .unwrap();
    output
}

fn varies_on_accept_encoding(response: &Response) -> bool {
    response
        .headers()
        .get_all(header::VARY)
        .iter()
        .any(|value| {
            value
                .to_str()
                .unwrap_or_default()
                .split(',')
                .any(|name| name.trim().eq_ignore_ascii_case("accept-encoding"))
        })
}

#[tokio::test]
async fn api_json_negotiates_brotli_gzip_and_identity() {
    let app = harness().await;

    let identity = get(&app, "/api/version", None).await;
    assert_eq!(identity.status(), StatusCode::OK);
    assert!(identity.headers().get(header::CONTENT_ENCODING).is_none());
    let expected = response_bytes(identity).await;
    assert!(expected.len() >= 256, "fixture must exercise the size gate");

    let brotli = get(&app, "/api/version", Some("gzip, br")).await;
    assert_eq!(
        brotli.headers().get(header::CONTENT_ENCODING).unwrap(),
        "br"
    );
    assert!(varies_on_accept_encoding(&brotli));
    assert_eq!(decode_brotli(&response_bytes(brotli).await), expected);

    let gzip = get(&app, "/api/version", Some("br;q=0.4, gzip;q=1")).await;
    assert_eq!(
        gzip.headers().get(header::CONTENT_ENCODING).unwrap(),
        "gzip"
    );
    assert!(varies_on_accept_encoding(&gzip));
    assert_eq!(decode_gzip(&response_bytes(gzip).await), expected);

    let explicitly_identity = get(&app, "/api/version", Some("br;q=0, gzip;q=0")).await;
    assert!(explicitly_identity
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_none());
    assert!(varies_on_accept_encoding(&explicitly_identity));
    assert_eq!(response_bytes(explicitly_identity).await, expected);
}

#[tokio::test]
async fn tiny_probe_is_not_compressed() {
    let app = harness().await;
    let response = get(&app, "/healthz", Some("br, gzip")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    assert!(!varies_on_accept_encoding(&response));
    assert_eq!(response_bytes(response).await, b"ok");
}

#[tokio::test]
async fn range_body_is_never_recompressed() {
    let root = unique_tmp("compression");
    let source = root.join("source");
    std::fs::create_dir_all(&source).unwrap();
    let original = b"0123456789".repeat(100);
    std::fs::write(source.join("large.txt"), &original).unwrap();

    let library = Arc::new(
        EmbeddedLibrary::open_with(&root.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let context = AuthContext::embedded();
    let source_id = library
        .add_source(
            &context,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: source.to_string_lossy().into_owned(),
                name: Some("compression fixture".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    let job_id = library
        .submit_scan(
            &context,
            ScanRequest {
                sources: vec![source_id],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    loop {
        let job = library.get_job(&context, &job_id).await.unwrap();
        if matches!(job.state, JobState::Done) {
            break;
        }
        assert!(
            !matches!(job.state, JobState::Failed | JobState::Cancelled),
            "fixture scan did not complete: {:?}",
            job.error
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let asset = library
        .query(&context, QueryRequest::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .next()
        .unwrap();
    let app = router(
        library,
        Arc::new(ServerStore::open_in_memory().unwrap()),
        "127.0.0.1:7878",
        true,
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/assets/{}/content", asset.id))
                .header(header::RANGE, "bytes=10-109")
                .header(header::ACCEPT_ENCODING, "br, gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 10-109/1000"
    );
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    assert!(!varies_on_accept_encoding(&response));
    assert_eq!(response_bytes(response).await, original[10..110]);
}
