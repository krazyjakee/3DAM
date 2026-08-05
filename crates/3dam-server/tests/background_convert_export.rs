//! Issue #114: the served surface accepts convert/export work immediately and exposes durable,
//! structured terminal reports through the ordinary job endpoint.

mod support;

use axum::http::StatusCode;
use dam_api::dto::{JobResult, JobState};
use dam_api::AssetId;
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use support::call_token;

async fn wait_job(app: &axum::Router, id: &str) -> dam_api::dto::JobStatus {
    for _ in 0..100 {
        let (status, value) =
            call_token(app, "GET", &format!("/api/v1/jobs/{id}"), None, None).await;
        assert_eq!(status, StatusCode::OK);
        let job: dam_api::dto::JobStatus = serde_json::from_value(value).unwrap();
        if matches!(
            job.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        ) {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} did not finish");
}

async fn wait_running(app: &axum::Router, id: &str) {
    for _ in 0..500 {
        let (status, value) =
            call_token(app, "GET", &format!("/api/v1/jobs/{id}"), None, None).await;
        assert_eq!(status, StatusCode::OK);
        let job: dam_api::dto::JobStatus = serde_json::from_value(value).unwrap();
        if job.state == JobState::Running {
            return;
        }
        assert_eq!(
            job.state,
            JobState::Queued,
            "job finished before cancellation"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("job {id} never entered running state");
}

#[tokio::test]
async fn convert_and_export_are_accepted_and_reports_reopen() {
    let root = tempfile::tempdir().unwrap();
    let library = Arc::new(
        EmbeddedLibrary::open_with(
            &root.path().join("data"),
            dam_core::ResourceOptions::ungoverned(),
        )
        .await
        .unwrap(),
    );
    let app = router(
        library,
        Arc::new(ServerStore::open_in_memory().unwrap()),
        "127.0.0.1:7878",
        true,
    );

    let export_path = root.path().join("manifest.json");
    let (status, accepted) = call_token(
        &app,
        "POST",
        "/api/v1/jobs/export",
        None,
        Some(json!({
            "format": "json",
            "output": export_path,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let export = wait_job(&app, accepted["job_id"].as_str().unwrap()).await;
    assert_eq!(export.state, JobState::Done);
    assert!(matches!(
        export.result.as_deref(),
        Some(JobResult::Export(_))
    ));
    assert!(export_path.exists());

    let (status, accepted) = call_token(
        &app,
        "POST",
        "/api/v1/jobs/convert",
        None,
        Some(json!({
            "inputs": [],
            "target": { "media": "image", "format": "png" },
            "output_dir": root.path().join("converted"),
            "dry_run": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let convert = wait_job(&app, accepted["job_id"].as_str().unwrap()).await;
    assert_eq!(convert.state, JobState::Done);
    assert!(matches!(
        convert.result.as_deref(),
        Some(JobResult::Convert(_))
    ));
}

#[tokio::test]
async fn in_flight_convert_and_export_cancellation_remains_terminal() {
    let root = tempfile::tempdir().unwrap();
    let library = Arc::new(
        EmbeddedLibrary::open_with(
            &root.path().join("data"),
            dam_core::ResourceOptions::ungoverned(),
        )
        .await
        .unwrap(),
    );
    let app = router(
        library,
        Arc::new(ServerStore::open_in_memory().unwrap()),
        "127.0.0.1:7878",
        true,
    );
    let ids = (0..20_000)
        .map(|_| AssetId::new().to_string())
        .collect::<Vec<_>>();

    let requests = [
        (
            "/api/v1/jobs/convert",
            json!({
                "inputs": ids,
                "target": { "media": "image", "format": "png" },
                "output_dir": root.path().join("converted"),
                "dry_run": true,
            }),
        ),
        (
            "/api/v1/jobs/export",
            json!({
                "assets": ids,
                "format": "json",
                "output": root.path().join("cancelled-manifest.json"),
            }),
        ),
    ];

    for (path, request) in requests {
        let (status, accepted) = tokio::time::timeout(
            Duration::from_secs(2),
            call_token(&app, "POST", path, None, Some(request)),
        )
        .await
        .expect("submission must stay bounded and return before the large job finishes");
        assert_eq!(status, StatusCode::ACCEPTED);
        let id = accepted["job_id"].as_str().unwrap();
        wait_running(&app, id).await;
        let (status, _) = call_token(
            &app,
            "POST",
            &format!("/api/v1/jobs/{id}/cancel"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(wait_job(&app, id).await.state, JobState::Cancelled);

        // Let the worker observe the flag and attempt its final write. The conditional terminal
        // update must not turn a cancelled history row back into Done or Failed.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (status, value) =
            call_token(&app, "GET", &format!("/api/v1/jobs/{id}"), None, None).await;
        assert_eq!(status, StatusCode::OK);
        let job: dam_api::dto::JobStatus = serde_json::from_value(value).unwrap();
        assert_eq!(job.state, JobState::Cancelled);
    }
}
