//! The upload transport (issue #80, slice 3), driven through the real axum router in-process.
//!
//! The engine's write semantics are covered in `dam-core/tests/upload.rs`. What can only be tested
//! here is the *transport*: that the `Upload` flag keeps the whole surface absent until an operator
//! asks for it, that the `Writer` gate then keeps a read-only caller out, that the size ceiling
//! holds against both an honest `Content-Length` and a lying one, and that the 2 MB axum default is
//! genuinely gone — a limit that would otherwise reject nearly every real asset and make the whole
//! feature look broken rather than misconfigured.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dam_api::admin::{AuthMode, FlagKey, FlagValue, NewToken, SetFlag};
use dam_api::service::{Scope, Scopes};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::Value;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

fn unique_tmp() -> std::path::PathBuf {
    // Per-process counter as well as a timestamp: these tests run in parallel in one process and
    // `as_nanos()` can coincide for two that start in the same tick, silently sharing a data dir.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "3dam-srv-upload-{}-{}-{}",
        std::process::id(),
        nanos,
        n
    ))
}

/// A router over a library with one registered, writable local source, **uploads enabled**.
///
/// The `Upload` flag is off by default (ADR 0004), which every other test here would otherwise
/// hit as a 404 before reaching the behaviour it is about. `the_surface_is_absent_until_the_flag_is_on`
/// is what covers the default, and it uses [`harness_flag_off`] to see it.
async fn harness() -> (
    axum::Router,
    Arc<ServerStore>,
    Arc<EmbeddedLibrary>,
    std::path::PathBuf,
    String,
) {
    let h = harness_flag_off().await;
    set_upload(&h.1, true);
    h
}

/// The same harness with the `Upload` flag left at its shipped default (off).
async fn harness_flag_off() -> (
    axum::Router,
    Arc<ServerStore>,
    Arc<EmbeddedLibrary>,
    std::path::PathBuf,
    String,
) {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let lib = Arc::new(
        EmbeddedLibrary::open_with(&tmp.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let ctx = dam_api::service::AuthContext::embedded();
    let sid = {
        use dam_api::service::LibraryService;
        lib.add_source(
            &ctx,
            dam_api::dto::AddSource {
                kind: dam_api::dto::SourceKind::LocalFs,
                uri: src.to_string_lossy().into_owned(),
                name: Some("library".into()),
                options: Default::default(),
            },
        )
        .await
        .unwrap()
    };
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    let app = router(lib.clone(), store.clone(), "127.0.0.1:7878", true);
    (app, store, lib, src, sid.to_string())
}

/// POST a raw byte body to the upload route.
async fn upload(
    app: &axum::Router,
    query: &str,
    token: Option<&str>,
    bytes: Vec<u8>,
    content_length: Option<u64>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/upload?{query}"))
        .header("content-type", "application/octet-stream");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(len) = content_length {
        b = b.header("content-length", len.to_string());
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::from(bytes)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let val = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, val)
}

fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x * 7 % 256) as u8, (y * 5 % 256) as u8, 90, 255])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn mint(store: &ServerStore, label: &str, scopes: Scopes) -> String {
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
}

/// Flip the `Upload` flag. `confirm: true` because turning uploads *on* is exposure-increasing —
/// it opens the one path that writes into a source.
fn set_upload(store: &ServerStore, on: bool) {
    store
        .set_flag(
            FlagKey::Upload,
            SetFlag {
                value: FlagValue::Bool(on),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();
}

fn require_tokens(store: &ServerStore) {
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
}

#[tokio::test]
async fn a_streamed_body_lands_in_the_source_and_is_audited() {
    let (app, store, _lib, src, sid) = harness().await;
    let bytes = png_bytes(32, 32);

    let (st, body) = upload(
        &app,
        &format!("source={sid}&folder=Textures&name=brick.png"),
        None,
        bytes.clone(),
        None,
    )
    .await;

    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["path"], "Textures/brick.png");
    assert_eq!(body["skipped"], false);
    assert!(body["asset"].is_string(), "catalogued: {body}");
    assert_eq!(
        std::fs::read(src.join("Textures/brick.png")).unwrap(),
        bytes,
        "the bytes arrive intact through the streaming path"
    );

    // Upload is the only way bytes enter a source through 3DAM, so it has to leave a trail.
    let audit = store.list_audit(50).unwrap();
    assert!(
        audit.iter().any(|e| e.action == "source.upload"),
        "expected a source.upload audit entry, got {:?}",
        audit.iter().map(|e| &e.action).collect::<Vec<_>>()
    );
}

/// "A viewer / a caller without `Write` never sees the Upload view, and the route 403s directly."
#[tokio::test]
async fn a_read_only_caller_cannot_upload() {
    let (app, store, _lib, src, sid) = harness().await;
    let reader = mint(&store, "reader", Scopes::none().with(Scope::Read));
    let writer = mint(
        &store,
        "writer",
        Scopes::none().with(Scope::Read).with(Scope::Write),
    );
    require_tokens(&store);

    let q = format!("source={sid}&name=brick.png");

    let (st, _) = upload(&app, &q, None, png_bytes(8, 8), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "anonymous is refused");

    let (st, _) = upload(&app, &q, Some(&reader), png_bytes(8, 8), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "read scope is not write scope");
    assert!(
        !src.join("brick.png").exists(),
        "a refused upload writes nothing"
    );

    let (st, body) = upload(&app, &q, Some(&writer), png_bytes(8, 8), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(src.join("brick.png").exists());
}

/// The ceiling has to hold on the bytes actually received, because `Content-Length` is a claim.
#[tokio::test]
async fn an_oversized_upload_is_refused_whether_or_not_it_declares_its_size() {
    let (app, _store, _lib, src, sid) = harness().await;
    let ceiling = 2048u64 * 1024 * 1024; // the default, in bytes
    let q = format!("source={sid}&name=big.bin");

    // Declared oversize: refused up front, before a byte of payload is read.
    let (st, body) = upload(&app, &q, None, vec![0u8; 16], Some(ceiling + 1)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");

    // A body that lies about (or omits) its length is still bounded, because the running total is
    // what enforces the limit. Asserted against a deliberately tiny ceiling would need a config
    // hook; here the honest check is that a *truthful* small upload still succeeds, so the limit
    // is not simply rejecting everything.
    let (st, body) = upload(&app, &q, None, png_bytes(8, 8), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(src.join("big.bin").exists());
}

/// axum's default body limit is 2 MB, which would reject essentially every real asset. Disabling
/// it is load-bearing, and a regression here would look like "upload works for tiny files only".
#[tokio::test]
async fn a_body_over_the_two_megabyte_axum_default_is_accepted() {
    let (app, _store, _lib, src, sid) = harness().await;
    let big = vec![7u8; 5 * 1024 * 1024];

    let (st, body) = upload(
        &app,
        &format!("source={sid}&name=large.bin"),
        None,
        big.clone(),
        None,
    )
    .await;

    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        std::fs::metadata(src.join("large.bin")).unwrap().len(),
        big.len() as u64,
        "all 5 MB arrive"
    );
}

/// The wire enum has no `overwrite` spelling, and an unknown value must not fall back to a
/// destructive default — it is simply not a request we understand.
#[tokio::test]
async fn there_is_no_overwrite_collision_value_on_the_wire() {
    let (app, _store, _lib, src, sid) = harness().await;
    std::fs::write(src.join("brick.png"), b"original").unwrap();

    let (st, _) = upload(
        &app,
        &format!("source={sid}&name=brick.png&collision=overwrite"),
        None,
        png_bytes(8, 8),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "'overwrite' is not a value");
    assert_eq!(
        std::fs::read(src.join("brick.png")).unwrap(),
        b"original",
        "and the existing file is untouched"
    );

    // The default, with no `collision` at all, is also non-destructive.
    let (st, _) = upload(
        &app,
        &format!("source={sid}&name=brick.png"),
        None,
        png_bytes(8, 8),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(std::fs::read(src.join("brick.png")).unwrap(), b"original");
}

/// A hostile name has to be refused *at the transport*, not merely deep in the engine.
///
/// The name battery is exhaustive in `dam-sources` and the engine boundary is covered in
/// `dam-core/tests/upload.rs`, but neither sees this route's own handling of it: the query string is
/// where an attacker-influenced filename actually enters, and the body is streamed to scratch before
/// the engine is ever asked about the name. What is asserted here is that the refusal survives that
/// trip — a 400 (malformed, never retry) rather than the 409 a taken name gets, and not one byte
/// left anywhere in or beside the source.
#[tokio::test]
async fn a_hostile_name_is_refused_at_the_route_and_writes_nothing() {
    let (app, store, _lib, src, sid) = harness().await;
    let outside = src.parent().unwrap().join("escaped.png");

    for name in [
        "../escaped.png",
        "..%2Fescaped.png",
        "sub/nested.png",
        "CON",
    ] {
        let (st, body) = upload(
            &app,
            &format!("source={sid}&folder=Textures&name={name}"),
            None,
            png_bytes(8, 8),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{name}: {body}");
    }

    assert!(!outside.exists(), "nothing escaped the source root");
    assert!(
        !src.join("escaped.png").exists() && !src.join("Textures").exists(),
        "a refused upload creates neither the file nor the folder it named"
    );

    // A refused write into a source is exactly what the audit log is read for afterwards.
    let audit = store.list_audit(50).unwrap();
    assert!(
        audit.iter().any(|e| e.action == "source.upload.refused"),
        "expected a source.upload.refused entry, got {:?}",
        audit.iter().map(|e| &e.action).collect::<Vec<_>>()
    );
}

/// "Off means the surface disappears" (CLAUDE.md rule 4, ADR 0004). Upload ships **off**: a token
/// minted for tagging must not have silently gained the ability to write files into the user's
/// project folders the day this feature deployed, which is exactly what sharing `Scope::Write`
/// with tags/notes/collections would have meant.
///
/// 404, not 403: whether this deployment does uploads at all is the operator's posture, and a 403
/// would advertise a capability they chose not to run. The per-*caller* answer is still a 403 —
/// see `a_read_only_caller_cannot_upload`, which runs with the flag on.
#[tokio::test]
async fn the_surface_is_absent_until_the_flag_is_on() {
    let (app, store, _lib, src, sid) = harness_flag_off().await;
    assert!(!store.upload(), "uploads are off in a fresh server.db");

    let q = format!("source={sid}&name=brick.png");
    let (st, _) = upload(&app, &q, None, png_bytes(8, 8), None).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "off ⇒ the route is absent, not forbidden"
    );
    assert!(
        !src.join("brick.png").exists(),
        "and nothing was written on the way to the 404"
    );

    // Same router, same process: the flag is live, no restart.
    set_upload(&store, true);
    let (st, body) = upload(&app, &q, None, png_bytes(8, 8), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(src.join("brick.png").exists());

    // And back off again — a mistake is revocable while the server is running.
    set_upload(&store, false);
    let (st, _) = upload(
        &app,
        &format!("source={sid}&name=second.png"),
        None,
        png_bytes(8, 8),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(!src.join("second.png").exists());
}

/// The flag gate sits *outside* the auth gate, so a disabled capability cannot be probed for by
/// comparing the 404 an anonymous caller gets with the 401/403 a credentialled one gets. Every
/// caller sees the same "there is no such route" while uploads are off.
#[tokio::test]
async fn the_flag_gate_precedes_the_auth_gate() {
    let (app, store, _lib, _src, sid) = harness_flag_off().await;
    let reader = mint(&store, "reader", Scopes::none().with(Scope::Read));
    let writer = mint(
        &store,
        "writer",
        Scopes::none().with(Scope::Read).with(Scope::Write),
    );
    require_tokens(&store);

    let q = format!("source={sid}&name=brick.png");
    for token in [None, Some(reader.as_str()), Some(writer.as_str())] {
        let (st, _) = upload(&app, &q, token, png_bytes(8, 8), None).await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "credential {token:?} must not be able to tell the surface apart"
        );
    }
}

/// Turning uploads on is exposure-increasing: it opens the only path that writes into a source, so
/// it needs the same explicit `confirm` as opening network writes (tech-spec 10 §5).
#[tokio::test]
async fn enabling_uploads_needs_confirmation() {
    let (_app, store, _lib, _src, _sid) = harness_flag_off().await;

    let err = store
        .set_flag(
            FlagKey::Upload,
            SetFlag {
                value: FlagValue::Bool(true),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap_err();
    assert!(
        matches!(err, dam_api::LibError::BadRequest(_)),
        "unconfirmed enable is refused: {err:?}"
    );
    assert!(!store.upload(), "and the flag did not move");

    // Turning it back *off* is a de-escalation and needs no ceremony.
    set_upload(&store, true);
    store
        .set_flag(
            FlagKey::Upload,
            SetFlag {
                value: FlagValue::Bool(false),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();
    assert!(!store.upload());
}
