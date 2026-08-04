//! Editable rights metadata (issue #106) — the field an extractor is most often wrong about.
//!
//! These cover the issue's acceptance criteria in the order it names them: a correction is
//! immediately visible to browse/filter (the whole vertical, and the reason status is *derived*
//! rather than asserted); a dry run previews the same authorized selection without touching the
//! catalog or emitting events; a mixed selection reports per-target warnings and still applies to
//! the targets it may write; and a peer-owned id is not writable at all.

use dam_api::dto::*;
use dam_api::event::{EventTopic, LibraryEvent, SubscribeRequest};
use dam_api::id::AssetId;
use dam_api::service::{AuthContext, LibraryService};
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Per-test data dir. The counter matters: `SystemTime` alone is not unique enough when tests run
/// in parallel on the same nanosecond-coarse clock (see `scan.rs`, which documents the same trap).
fn unique_tmp() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "3dam-license-{}-{}-{}",
        std::process::id(),
        nanos,
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A 1×1 PNG — enough to be detected, catalogued, and hashed; nothing here decodes pixels.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

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

/// Open a library over `data`, register `src`, and scan it.
async fn scanned(data: &Path, src: &Path) -> (EmbeddedLibrary, dam_api::id::SourceId) {
    let lib = EmbeddedLibrary::open_with(data, dam_core::ResourceOptions::ungoverned())
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
    (lib, sid)
}

async fn find(lib: &EmbeddedLibrary, ctx: &AuthContext, name: &str) -> AssetId {
    lib.query(ctx, QueryRequest::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|a| a.name == name)
        .unwrap_or_else(|| panic!("{name} not catalogued"))
        .id
}

/// Names matching a single-filter query, sorted — the browse/filter surface this feature exists to
/// move rows into and out of.
async fn filtered(
    lib: &EmbeddedLibrary,
    ctx: &AuthContext,
    field: FacetField,
    value: &str,
) -> Vec<String> {
    let mut names: Vec<String> = lib
        .query(
            ctx,
            QueryRequest {
                filters: vec![Filter {
                    field,
                    op: FilterOp::Eq,
                    value: FilterValue::Str(value.into()),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|a| a.name)
        .collect();
    names.sort();
    names
}

/// CC0: a named licence with all four rights explicitly known and no attribution condition — the
/// only shape that derives `permissive` (ADR 0009 §1).
fn cc0() -> LicenseInput {
    LicenseInput {
        id: Some(Some("CC0-1.0".into())),
        commercial: Some(Some(true)),
        modify: Some(Some(true)),
        redistribute: Some(Some(true)),
        attribution: Some(Some(false)),
        ..Default::default()
    }
}

fn explicit(assets: Vec<AssetId>, license: LicenseInput) -> SetLicenseRequest {
    SetLicenseRequest {
        assets,
        license,
        ..Default::default()
    }
}

/// Acceptance criterion #1: correcting an unknown licence must immediately move the asset in the
/// browse/filter results. This is the whole vertical — the store derives the status, the query path
/// reads the same column, and nothing caches a stale badge in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correcting_an_unknown_license_moves_the_asset_in_filtered_browse() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();
    std::fs::write(src.join("plaster.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let brick = find(&lib, &ctx, "brick.png").await;

    // A scan cannot know a licence, so both files start unknown and neither is commercially usable.
    assert_eq!(
        filtered(&lib, &ctx, FacetField::License, "unknown").await,
        ["brick.png", "plaster.png"]
    );
    assert!(
        filtered(&lib, &ctx, FacetField::License, "permissive")
            .await
            .is_empty(),
        "nothing is permissive until somebody says so"
    );
    assert!(filtered(&lib, &ctx, FacetField::UsageRight, "commercial")
        .await
        .is_empty());

    let result = lib
        .set_license(&ctx, explicit(vec![brick], cc0()))
        .await
        .unwrap();
    assert_eq!(result.matched, 1);
    assert_eq!(result.changed, 1);
    assert!(result.warnings.is_empty());
    let mix: Vec<(LicenseStatus, u64)> = result
        .status
        .iter()
        .map(|entry| (entry.status, entry.count))
        .collect();
    assert_eq!(
        mix,
        [(LicenseStatus::Permissive, 1)],
        "the post-write mix is reported so a client needn't re-query"
    );

    // The filters that excluded it now include it, and the one that included it no longer does.
    assert_eq!(
        filtered(&lib, &ctx, FacetField::License, "permissive").await,
        ["brick.png"]
    );
    assert_eq!(
        filtered(&lib, &ctx, FacetField::UsageRight, "commercial").await,
        ["brick.png"]
    );
    assert_eq!(
        filtered(&lib, &ctx, FacetField::License, "unknown").await,
        ["plaster.png"],
        "the corrected asset left the unknown bucket"
    );

    // The record itself agrees, and the edit is attributed to the user rather than an extractor.
    let license = lib.get_asset(&ctx, &brick).await.unwrap().license;
    assert_eq!(license.id.as_deref(), Some("CC0-1.0"));
    assert_eq!(license.status, LicenseStatus::Permissive);
    assert_eq!(license.commercial, Some(true));
    assert_ne!(license.provenance, "extracted");

    // And the correction survives a restart: it is catalog state, not a session overlay.
    drop(lib);
    let lib =
        EmbeddedLibrary::open_with(&tmp.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap();
    assert_eq!(
        filtered(&lib, &ctx, FacetField::License, "permissive").await,
        ["brick.png"]
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A patch is three-state: absent leaves a column alone, `null` clears it. Stamping a holder across
/// a selection must not disturb licence ids that are already right — the case two-state patches
/// cannot express.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_field_is_left_alone_and_an_explicit_null_clears_it() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let brick = find(&lib, &ctx, "brick.png").await;

    lib.set_license(&ctx, explicit(vec![brick], cc0()))
        .await
        .unwrap();

    // Only `holder` is present, so the licence id and every right survive untouched.
    lib.set_license(
        &ctx,
        explicit(
            vec![brick],
            LicenseInput {
                holder: Some(Some("Kenney".into())),
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap();
    let license = lib.get_asset(&ctx, &brick).await.unwrap().license;
    assert_eq!(license.holder.as_deref(), Some("Kenney"));
    assert_eq!(license.id.as_deref(), Some("CC0-1.0"));
    assert_eq!(license.status, LicenseStatus::Permissive);

    // An explicit null is a retraction — and with no licence named, the status must fall back to
    // unknown rather than keep a green badge nothing supports.
    lib.set_license(
        &ctx,
        explicit(
            vec![brick],
            LicenseInput {
                id: Some(None),
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap();
    let license = lib.get_asset(&ctx, &brick).await.unwrap().license;
    assert_eq!(license.id, None);
    assert_eq!(
        license.status,
        LicenseStatus::Unknown,
        "clearing the licence id must not leave a permissive badge behind"
    );
    assert_eq!(
        license.holder.as_deref(),
        Some("Kenney"),
        "clearing one field must not clear the others"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A preview must cost the caller nothing: same authorized selection, same counts, no catalog write
/// and no event (a client that live-updates on `LicenseSet` would otherwise show a change that
/// never happened).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_previews_the_same_selection_without_touching_anything() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();
    std::fs::write(src.join("plaster.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let ids = vec![
        find(&lib, &ctx, "brick.png").await,
        find(&lib, &ctx, "plaster.png").await,
    ];

    let mut events = lib
        .subscribe(
            &ctx,
            SubscribeRequest {
                topics: vec![EventTopic::Assets],
            },
        )
        .await
        .unwrap();

    let preview = lib
        .set_license(
            &ctx,
            SetLicenseRequest {
                assets: ids.clone(),
                license: cc0(),
                dry_run: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(preview.matched, 2);
    assert_eq!(preview.changed, 2);
    assert!(preview.warnings.is_empty());

    assert_eq!(
        filtered(&lib, &ctx, FacetField::License, "unknown").await,
        ["brick.png", "plaster.png"],
        "a dry run must not move a single row"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(250), events.next())
            .await
            .is_err(),
        "a dry run must not emit an asset event"
    );

    // The real run then reports exactly what the preview promised, and *does* emit.
    let applied = lib
        .set_license(&ctx, explicit(ids.clone(), cc0()))
        .await
        .unwrap();
    assert_eq!((applied.matched, applied.changed), (2, 2));
    let mut changed = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .expect("an applied edit emits")
            .unwrap();
        match event {
            LibraryEvent::AssetChanged { id, kind, .. } => {
                assert_eq!(kind, dam_api::event::ChangeKind::LicenseSet);
                changed.push(id);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    changed.sort();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(changed, expected, "one event per actually-changed asset");

    // Re-applying the identical patch changes nothing — the store diffs, so `changed` is honest and
    // no spurious event tells a client to refetch.
    let repeat = lib.set_license(&ctx, explicit(ids, cc0())).await.unwrap();
    assert_eq!(repeat.matched, 2);
    assert_eq!(repeat.changed, 0, "a no-op patch changes no rows");
    assert!(
        tokio::time::timeout(Duration::from_millis(250), events.next())
            .await
            .is_err(),
        "a no-op edit must not emit"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Partial failure: a bulk edit over a mixed selection must apply to what it may write and *say*
/// what it skipped, distinguishing "you cannot see this" from "you can see it but not edit it".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mixed_selection_applies_to_the_writable_ids_and_warns_about_the_rest() {
    let tmp = unique_tmp();
    let open = tmp.join("open");
    let closed = tmp.join("closed");
    std::fs::create_dir_all(&open).unwrap();
    std::fs::create_dir_all(&closed).unwrap();
    std::fs::write(open.join("writable.png"), PNG).unwrap();
    std::fs::write(closed.join("readonly.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, open_sid) = scanned(&tmp.join("data"), &open).await;
    let closed_sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: closed.to_string_lossy().into_owned(),
                name: Some("closed".into()),
                options: Default::default(),
            },
        )
        .await
        .unwrap();
    let job = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![closed_sid],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    wait_job(&lib, &ctx, &job).await;

    let writable = find(&lib, &ctx, "writable.png").await;
    let readonly = find(&lib, &ctx, "readonly.png").await;
    let ghost = AssetId::new();

    // An editor who may read both sources but only write one.
    let mut scope = dam_api::VisibilityScope::default();
    scope.sources.insert(open_sid);
    scope.sources.insert(closed_sid);
    scope.write_sources.insert(open_sid);
    let editor = AuthContext::connected(
        Some("vera".into()),
        dam_api::Role::Editor.scopes(),
        dam_api::Visibility::Restricted(scope),
    );

    let result = lib
        .set_license(&editor, explicit(vec![writable, readonly, ghost], cc0()))
        .await
        .unwrap();
    assert_eq!(result.matched, 1, "only the writable target was in scope");
    assert_eq!(result.changed, 1);
    let codes: std::collections::BTreeMap<String, String> = result
        .warnings
        .iter()
        .map(|w| (w.subject.clone(), w.code.clone()))
        .collect();
    assert_eq!(
        codes.get(&readonly.to_string()).map(String::as_str),
        Some("target_read_only"),
        "readable-but-not-writable is its own answer, not a 404: {:?}",
        result.warnings
    );
    assert_eq!(
        codes.get(&ghost.to_string()).map(String::as_str),
        Some("target_unavailable"),
        "an id the caller cannot reach (or that does not exist) is unavailable"
    );
    assert!(!codes.contains_key(&writable.to_string()));

    // The one target it could write did change; the one it could not did not.
    assert_eq!(
        filtered(&lib, &ctx, FacetField::License, "permissive").await,
        ["writable.png"]
    );
    assert_eq!(
        lib.get_asset(&ctx, &readonly).await.unwrap().license.status,
        LicenseStatus::Unknown,
        "a warned-about target must be untouched"
    );

    // A caller with no write scope at all is refused outright rather than warned per target.
    let viewer = AuthContext::connected(
        Some("rita".into()),
        dam_api::Role::Viewer.scopes(),
        dam_api::Visibility::Full,
    );
    assert!(matches!(
        lib.set_license(&viewer, explicit(vec![writable], cc0()))
            .await,
        Err(LibError::Forbidden(_))
    ));

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Selector and size guards, and the deliberate no-op. An all-absent patch is *not* an error: the
/// caller still learns what its selector resolved to, which is what a preview UI opens with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selector_and_size_rules_match_the_tag_edit_shape() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let brick = find(&lib, &ctx, "brick.png").await;

    // No selector at all.
    assert!(matches!(
        lib.set_license(&ctx, explicit(Vec::new(), cc0())).await,
        Err(LibError::BadRequest(_))
    ));
    // Two selectors: which one wins is not a question the engine should have to guess at.
    assert!(matches!(
        lib.set_license(
            &ctx,
            SetLicenseRequest {
                assets: vec![brick],
                query: Some(QueryRequest::default()),
                license: cc0(),
                ..Default::default()
            }
        )
        .await,
        Err(LibError::BadRequest(_))
    ));
    // Over the explicit cap.
    let flood = vec![brick; LICENSE_EDIT_EXPLICIT_MAX + 1];
    assert!(matches!(
        lib.set_license(&ctx, explicit(flood, cc0())).await,
        Err(LibError::BadRequest(_))
    ));

    // An empty patch reports its selection and writes nothing.
    let result = lib
        .set_license(&ctx, explicit(vec![brick], LicenseInput::default()))
        .await
        .unwrap();
    assert_eq!((result.matched, result.changed), (1, 0));
    assert_eq!(
        lib.get_asset(&ctx, &brick).await.unwrap().license.status,
        LicenseStatus::Unknown
    );

    // A query selector reaches the same row through the server-resolved path.
    let result = lib
        .set_license(
            &ctx,
            SetLicenseRequest {
                query: Some(QueryRequest::default()),
                license: cc0(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!((result.matched, result.changed), (1, 1));
    assert_eq!(
        filtered(&lib, &ctx, FacetField::License, "permissive").await,
        ["brick.png"]
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
