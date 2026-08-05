//! Issue #106: the bulk rights-edit route. A licence is a claim about how an asset may be used, so
//! the route sits behind the same write gate as any other metadata mutation — and a caller holding
//! only `Read` must be refused before the engine ever sees the request.
//!
//! Driven through the real axum router with `ServiceExt::oneshot` (no socket bind), per the house
//! convention in `auth_tokens.rs`.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use dam_api::admin::{AuthMode, FlagKey, FlagValue, NewToken, SetFlag};
use dam_api::dto::*;
use dam_api::service::{AuthContext, LibraryService, Scope, Scopes};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// A 1×1 PNG — enough to be detected, catalogued, and hashed.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

struct Harness {
    app: axum::Router,
    lib: Arc<EmbeddedLibrary>,
    asset: dam_api::id::AssetId,
    writer: String,
    reader: String,
    _dir: tempfile::TempDir,
}

/// Token-mode server over a library holding exactly one scanned, licence-unknown asset.
async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();

    let lib = Arc::new(
        EmbeddedLibrary::open_with(
            &dir.path().join("data"),
            dam_core::ResourceOptions::ungoverned(),
        )
        .await
        .unwrap(),
    );
    let ctx = AuthContext::embedded();
    let source = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: src.to_string_lossy().into_owned(),
                name: Some("fixtures".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    let job = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![source],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    for _ in 0..500 {
        let state = lib.get_job(&ctx, &job).await.unwrap().state;
        if matches!(
            state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let asset = lib
        .query(&ctx, QueryRequest::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .next()
        .expect("the fixture was catalogued")
        .id;

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
        "license writer",
        Scopes::none().with(Scope::Read).with(Scope::Write),
    );
    let reader = mint("license reader", Scopes::none().with(Scope::Read));

    Harness {
        app: router(lib.clone(), store, "127.0.0.1:7878", true),
        lib,
        asset,
        writer,
        reader,
        _dir: dir,
    }
}

async fn post(app: &axum::Router, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_license_route_is_write_scoped_and_round_trips_a_patch() {
    let h = harness().await;
    let ctx = AuthContext::embedded();
    let patch = json!({
        "assets": [h.asset.to_string()],
        "license": {
            "id": "CC-BY-4.0",
            "commercial": true,
            "modify": true,
            "redistribute": true,
            "attribution": true,
            "holder": "Kenney"
        }
    });

    // A read-only token is refused by the extractor, before the engine sees the body.
    let (status, _) = post(&h.app, "/api/v1/assets/license", &h.reader, patch.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        h.lib
            .get_asset(&ctx, &h.asset)
            .await
            .unwrap()
            .license
            .status,
        LicenseStatus::Unknown,
        "the refused request wrote nothing"
    );

    // The same body under a write token applies, and reports the derived status mix back.
    let (status, body) = post(&h.app, "/api/v1/assets/license", &h.writer, patch).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["matched"], 1);
    assert_eq!(body["changed"], 1);
    assert_eq!(body["status"][0]["status"], "attribution");
    assert_eq!(body["status"][0]["count"], 1);

    let license = h.lib.get_asset(&ctx, &h.asset).await.unwrap().license;
    assert_eq!(license.id.as_deref(), Some("CC-BY-4.0"));
    assert_eq!(license.holder.as_deref(), Some("Kenney"));
    assert_eq!(
        license.status,
        LicenseStatus::Attribution,
        "the wire shape derives the same status the engine does"
    );

    // The three-state patch survives the wire: an explicit JSON null clears, an absent key keeps.
    let (status, body) = post(
        &h.app,
        "/api/v1/assets/license",
        &h.writer,
        json!({
            "assets": [h.asset.to_string()],
            "license": { "holder": null }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["changed"], 1);
    let license = h.lib.get_asset(&ctx, &h.asset).await.unwrap().license;
    assert_eq!(license.holder, None, "an explicit null clears the column");
    assert_eq!(
        license.id.as_deref(),
        Some("CC-BY-4.0"),
        "an absent key leaves its column alone"
    );

    // A dry run over the same router previews without writing.
    let (status, body) = post(
        &h.app,
        "/api/v1/assets/license",
        &h.writer,
        json!({
            "assets": [h.asset.to_string()],
            "license": { "id": "Proprietary" },
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["matched"], 1);
    assert_eq!(
        h.lib
            .get_asset(&ctx, &h.asset)
            .await
            .unwrap()
            .license
            .id
            .as_deref(),
        Some("CC-BY-4.0"),
        "a dry run over the wire is still a dry run"
    );
}

/// A client cannot assert `status` — it is derived from the id and rights at write time, so the
/// unknown field is ignored rather than honoured (ADR 0009 §1: a green badge nobody established).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_cannot_assert_a_permissive_status_over_the_wire() {
    let h = harness().await;
    let ctx = AuthContext::embedded();

    let (status, _) = post(
        &h.app,
        "/api/v1/assets/license",
        &h.writer,
        json!({
            "assets": [h.asset.to_string()],
            "license": { "holder": "Kenney", "status": "permissive" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.lib
            .get_asset(&ctx, &h.asset)
            .await
            .unwrap()
            .license
            .status,
        LicenseStatus::Unknown,
        "no licence was named, so nothing may claim the asset is cleared for use"
    );
}
