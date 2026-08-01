//! Large-content transport regression coverage (issue #125). The fixture is a sparse file larger
//! than the materialised-preview cap: requests therefore prove the HTTP path uses the range stream
//! rather than `LibraryService::read_content` or a whole-representation allocation.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use dam_api::dto::{
    AddSource, JobState, QueryRequest, ScanMode, ScanRequest, SourceKind, SourceOptions,
    MAX_MATERIALIZED_CONTENT_BYTES,
};
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::Value;
use std::io::{Seek, Write};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

struct Fixture {
    _temp: tempfile::TempDir,
    app: axum::Router,
    asset: dam_api::id::AssetId,
    total: u64,
    marker_offset: u64,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let total = MAX_MATERIALIZED_CONTENT_BYTES + 8192;
    let marker_offset = total - 4096;
    let mut file = std::fs::File::create(source.join("large.mp4")).unwrap();
    file.set_len(total).unwrap();
    file.seek(std::io::SeekFrom::Start(marker_offset)).unwrap();
    file.write_all(&vec![0x7b; 4096]).unwrap();
    drop(file);

    let library = Arc::new(
        EmbeddedLibrary::open_with(
            &temp.path().join("data"),
            dam_core::ResourceOptions::ungoverned(),
        )
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
                name: Some("large media".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    let job = library
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
        let status = library.get_job(&context, &job).await.unwrap();
        if status.state == JobState::Done {
            break;
        }
        assert!(
            !matches!(status.state, JobState::Failed | JobState::Cancelled),
            "fixture scan failed: {:?}",
            status.error
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
        .unwrap()
        .id;
    let app = router(
        library,
        Arc::new(ServerStore::open_in_memory().unwrap()),
        "127.0.0.1:7878",
        true,
    );
    Fixture {
        _temp: temp,
        app,
        asset,
        total,
        marker_offset,
    }
}

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn large_content_supports_head_validators_ranges_and_fail_soft_fallbacks() {
    let fixture = fixture().await;
    let uri = format!("/api/v1/assets/{}/content", fixture.asset);

    let head = request(&fixture.app, Method::HEAD, &uri, &[]).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        head.headers().get(header::CONTENT_LENGTH).unwrap(),
        fixture.total.to_string().as_str()
    );
    assert_eq!(head.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");
    let etag = head.headers().get(header::ETAG).unwrap().to_str().unwrap().to_string();
    assert!(
        axum::body::to_bytes(head.into_body(), 1).await.unwrap().is_empty(),
        "HEAD must not open or emit content bytes"
    );

    let ticket_response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/media-ticket")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "target": uri.clone() }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ticket_response.status(), StatusCode::OK);
    let ticket_body = axum::body::to_bytes(ticket_response.into_body(), 4096)
        .await
        .unwrap();
    let ticket: Value = serde_json::from_slice(&ticket_body).unwrap();
    let ticketed_uri = format!("{uri}?ticket={}", ticket["ticket"].as_str().unwrap());
    let ticketed_head = request(&fixture.app, Method::HEAD, &ticketed_uri, &[]).await;
    assert_eq!(ticketed_head.status(), StatusCode::OK);
    assert_eq!(
        ticketed_head.headers().get(header::CONTENT_LENGTH).unwrap(),
        fixture.total.to_string().as_str()
    );

    let range = format!("bytes={}-{}", fixture.marker_offset, fixture.total - 1);
    let partial = request(
        &fixture.app,
        Method::GET,
        &uri,
        &[("range", &range), ("if-range", &etag)],
    )
    .await;
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        partial.headers().get(header::CONTENT_RANGE).unwrap(),
        format!(
            "bytes {}-{}/{}",
            fixture.marker_offset,
            fixture.total - 1,
            fixture.total
        )
        .as_str()
    );
    assert_eq!(partial.headers().get(header::CONTENT_LENGTH).unwrap(), "4096");
    let bytes = axum::body::to_bytes(partial.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), vec![0x7b; 4096]);

    let unsatisfiable = request(
        &fixture.app,
        Method::GET,
        &uri,
        &[("range", "bytes=999999999-")],
    )
    .await;
    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        unsatisfiable.headers().get(header::CONTENT_RANGE).unwrap(),
        format!("bytes */{}", fixture.total).as_str()
    );

    // A stale validator and malformed Range both fall back to a full streaming 200. Drop the body
    // immediately: this also exercises cooperative cancellation without draining 256+ MiB.
    for headers in [
        vec![("range", "bytes=0-31"), ("if-range", "\"stale\"")],
        vec![("range", "bytes=not-a-range")],
    ] {
        let response = request(&fixture.app, Method::GET, &uri, &headers).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).unwrap(),
            fixture.total.to_string().as_str()
        );
        drop(response);
    }

    // Sixteen simultaneous 1 KiB reads over the 256+ MiB representation only expose 16 KiB to
    // consumers. Together with the source/channels' fixed chunk bounds, this guards the old
    // whole-file-per-request regression without making the test itself allocate the large body.
    let requests = (0..16u64).map(|index| {
        let app = fixture.app.clone();
        let uri = uri.clone();
        async move {
            let first = index * 1024;
            let range = format!("bytes={first}-{}", first + 1023);
            let response = request(&app, Method::GET, &uri, &[("range", &range)]).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers().get(header::CONTENT_LENGTH).unwrap(), "1024");
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
        }
    });
    let bodies = futures::future::join_all(requests).await;
    assert!(bodies.iter().all(|body| body.len() == 1024));
}
