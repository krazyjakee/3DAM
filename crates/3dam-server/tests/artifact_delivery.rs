//! Issue #115: hosted output is allocated by the server and retrieved only through an authenticated,
//! visible completed job. The route accepts a job id, never a filesystem path.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use dam_api::admin::{AuthMode, FlagKey, FlagValue, NewToken, SetFlag};
use dam_api::dto::{JobState, JobStatus};
use dam_api::service::{Scope, Scopes};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    data: tempfile::TempDir,
    writer: String,
    reader: String,
}

async fn harness() -> Harness {
    let data = tempfile::tempdir().unwrap();
    let lib = Arc::new(
        EmbeddedLibrary::open_with(
            &data.path().join("data"),
            dam_core::ResourceOptions::ungoverned(),
        )
        .await
        .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    store
        .set_flag(
            FlagKey::Authentication,
            SetFlag {
                value: FlagValue::Auth(AuthMode::Token),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();
    let mint = |label: &str, scopes: Scopes| {
        store
            .create_token(
                NewToken {
                    label: label.into(),
                    scopes,
                    expires: None,
                },
                "test",
            )
            .unwrap()
            .secret
    };
    let writer = mint(
        "artifact writer",
        Scopes::none().with(Scope::Read).with(Scope::Write),
    );
    let reader = mint("artifact reader", Scopes::none().with(Scope::Read));
    Harness {
        app: router(lib, store, "127.0.0.1:7878", true),
        data,
        writer,
        reader,
    }
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    json: Option<Value>,
    range: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    let body = if let Some(json) = json {
        request = request.header(header::CONTENT_TYPE, "application/json");
        Body::from(json.to_string())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

async fn submit(h: &Harness, format: &str) -> String {
    let (status, _, body) = request(
        &h.app,
        "POST",
        "/api/v1/jobs/export-artifact",
        Some(&h.writer),
        Some(json!({ "format": format })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    serde_json::from_slice::<Value>(&body).unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn wait_done(h: &Harness, id: &str) -> JobStatus {
    for _ in 0..200 {
        let (status, _, body) = request(
            &h.app,
            "GET",
            &format!("/api/v1/jobs/{id}"),
            Some(&h.reader),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let job: JobStatus = serde_json::from_slice(&body).unwrap();
        if matches!(job.state, JobState::Done | JobState::Failed) {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job did not finish")
}

#[tokio::test]
async fn manifest_download_is_authenticated_named_range_capable_and_path_redacted() {
    let h = harness().await;
    let (status, _, _) = request(
        &h.app,
        "POST",
        "/api/v1/jobs/export-artifact",
        Some(&h.reader),
        Some(json!({ "format": "json" })),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "read-only callers cannot export"
    );

    let id = submit(&h, "json").await;
    let job = wait_done(&h, &id).await;
    assert_eq!(job.state, JobState::Done);
    let encoded = serde_json::to_value(job).unwrap();
    assert_eq!(
        encoded["result"]["report"]["output"],
        "Server-managed download"
    );
    assert!(encoded["result_artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|artifact| artifact["route"] == format!("/api/v1/jobs/{id}/artifact")));

    let uri = format!("/api/v1/jobs/{id}/artifact");
    let (status, _, _) = request(&h.app, "GET", &uri, None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, headers, body) = request(&h.app, "GET", &uri, Some(&h.reader), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, br#"{"assets":[]}"#);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert!(headers[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .contains(&format!("3dam-manifest-{id}.json")));

    let (status, headers, body) = request(
        &h.app,
        "GET",
        &uri,
        Some(&h.reader),
        None,
        Some("bytes=0-0"),
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, b"{");
    assert_eq!(headers[header::CONTENT_RANGE], "bytes 0-0/13");

    let (status, headers, body) = request(&h.app, "HEAD", &uri, Some(&h.reader), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
}

#[tokio::test]
async fn arbitrary_server_paths_symlinks_and_unfinished_jobs_are_not_artifacts() {
    let h = harness().await;
    let outside = h.data.path().join("outside.json");
    let (status, _, body) = request(
        &h.app,
        "POST",
        "/api/v1/jobs/export",
        Some(&h.writer),
        Some(json!({ "format": "json", "output": outside })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = serde_json::from_slice::<Value>(&body).unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_done(&h, &id).await;
    let (status, _, _) = request(
        &h.app,
        "GET",
        &format!("/api/v1/jobs/{id}/artifact"),
        Some(&h.reader),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    #[cfg(unix)]
    {
        let id = submit(&h, "json").await;
        wait_done(&h, &id).await;
        let export_dir = h.data.path().join("data/artifacts/export");
        let generated = std::fs::read_dir(&export_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "json"))
            .unwrap();
        let secret = h.data.path().join("secret.txt");
        std::fs::write(&secret, b"do not download").unwrap();
        std::fs::remove_file(&generated).unwrap();
        std::os::unix::fs::symlink(&secret, &generated).unwrap();
        let (status, _, body) = request(
            &h.app,
            "GET",
            &format!("/api/v1/jobs/{id}/artifact"),
            Some(&h.reader),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body
            .windows(secret.as_os_str().len())
            .any(|bytes| bytes == secret.as_os_str().as_encoded_bytes()));
    }
}

#[tokio::test]
async fn sidecars_are_one_zip_and_simultaneous_downloads_share_a_valid_package() {
    let h = harness().await;
    let id = submit(&h, "sidecar").await;
    assert_eq!(wait_done(&h, &id).await.state, JobState::Done);
    let uri = format!("/api/v1/jobs/{id}/artifact");
    let left = request(&h.app, "GET", &uri, Some(&h.reader), None, None);
    let right = request(&h.app, "GET", &uri, Some(&h.reader), None, None);
    let ((ls, lh, lb), (rs, rh, rb)) = tokio::join!(left, right);
    assert_eq!((ls, rs), (StatusCode::OK, StatusCode::OK));
    assert_eq!(lh[header::CONTENT_TYPE], "application/zip");
    assert_eq!(rh[header::CONTENT_TYPE], "application/zip");
    assert!(lb.starts_with(b"PK"));
    assert!(rb.starts_with(b"PK"));
}

#[tokio::test]
async fn managed_convert_dry_run_is_labelled_as_a_plan_and_has_no_download() {
    let h = harness().await;
    let (status, _, body) = request(
        &h.app,
        "POST",
        "/api/v1/jobs/convert-artifact",
        Some(&h.writer),
        Some(json!({
            "inputs": [],
            "target": { "media": "image", "format": "png" },
            "dry_run": true
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = serde_json::from_slice::<Value>(&body).unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let job = wait_done(&h, &id).await;
    let encoded = serde_json::to_value(job).unwrap();
    assert_eq!(
        encoded["result"]["report"]["output_dir"],
        "No output (dry run)"
    );
    assert!(!encoded["result_artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|artifact| artifact["route"] == format!("/api/v1/jobs/{id}/artifact")));
    let (status, _, _) = request(
        &h.app,
        "GET",
        &format!("/api/v1/jobs/{id}/artifact"),
        Some(&h.reader),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
