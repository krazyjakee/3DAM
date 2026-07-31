//! Per-asset free-text notes (issue #81) — the one field the automation will never infer.
//!
//! These cover the acceptance criteria the issue names, in the order it names them: a note survives
//! a restart and a rescan; a word that appears *only* in a note finds the asset; clearing removes it
//! from the index (a stale FTS entry is the real bug here, since the text has no other home); notes
//! reach export output; and a read-only caller can read one but not write it.

use dam_api::dto::*;
use dam_api::id::AssetId;
use dam_api::service::{AuthContext, LibraryService};
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
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
        "3dam-notes-{}-{}-{}",
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

/// Open a library over `src`, scan it, and return the engine plus the source id.
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

async fn rescan(lib: &EmbeddedLibrary, ctx: &AuthContext, sid: dam_api::id::SourceId) {
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
    wait_job(lib, ctx, &job).await;
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

async fn search_names(lib: &EmbeddedLibrary, ctx: &AuthContext, text: &str) -> Vec<String> {
    lib.query(
        ctx,
        QueryRequest {
            text: Some(text.into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .items
    .into_iter()
    .map(|a| a.name)
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn note_survives_restart_and_rescan() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    let data = tmp.join("data");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, sid) = scanned(&data, &src).await;
    let id = find(&lib, &ctx, "brick.png").await;

    lib.set_note(
        &ctx,
        &id,
        NoteRequest {
            body: "  Client rejected this variant.  ".into(),
        },
    )
    .await
    .unwrap();

    // Stored trimmed, and readable straight back off the asset record.
    let note = lib.get_asset(&ctx, &id).await.unwrap().note.unwrap();
    assert_eq!(note.body, "Client rejected this variant.");
    assert!(note.updated_at > 0, "note carries an edit time");

    // A rescan re-walks the same file and reconciles the same row — authored state must not be
    // collateral damage of the upsert path.
    rescan(&lib, &ctx, sid).await;
    assert_eq!(
        lib.get_note(&ctx, &id).await.unwrap().unwrap().body,
        "Client rejected this variant.",
        "note lost to a rescan"
    );

    // Restart: drop the engine and reopen the same data dir.
    drop(lib);
    let lib = EmbeddedLibrary::open_with(&data, dam_core::ResourceOptions::ungoverned())
        .await
        .unwrap();
    assert_eq!(
        lib.get_note(&ctx, &id).await.unwrap().unwrap().body,
        "Client rejected this variant.",
        "note lost to a restart"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn note_text_is_searchable_and_clearing_unindexes_it() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();
    std::fs::write(src.join("plaster.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let id = find(&lib, &ctx, "brick.png").await;

    // "chimney" appears in no filename, no token, and no tag — only in the note.
    assert!(
        search_names(&lib, &ctx, "chimney").await.is_empty(),
        "nothing should match before the note exists"
    );

    lib.set_note(
        &ctx,
        &id,
        NoteRequest {
            body: "Photographed off a chimney; the mortar is wrong for interiors.".into(),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        search_names(&lib, &ctx, "chimney").await,
        vec!["brick.png".to_string()],
        "a word that lives only in a note must find the asset"
    );

    // Clearing removes the row *and* the index entry. The note text has no home outside the FTS
    // index, so a stale entry here would keep matching with nothing left to explain it.
    lib.set_note(&ctx, &id, NoteRequest { body: "   ".into() })
        .await
        .unwrap();
    assert!(
        lib.get_note(&ctx, &id).await.unwrap().is_none(),
        "a blank body clears the note"
    );
    assert!(
        lib.get_asset(&ctx, &id).await.unwrap().note.is_none(),
        "the cleared note is gone from the record too"
    );
    assert!(
        search_names(&lib, &ctx, "chimney").await.is_empty(),
        "cleared note left a stale FTS entry"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A note is authored text, so it joins filename/tokens/tags on the near side of the ranking tier —
/// but it must not outrank the file the user actually named.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filename_match_still_outranks_a_note_match() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("chimney.png"), PNG).unwrap();
    std::fs::write(src.join("plaster.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let noted = find(&lib, &ctx, "plaster.png").await;
    lib.set_note(
        &ctx,
        &noted,
        NoteRequest {
            body: "chimney chimney chimney chimney chimney".into(),
        },
    )
    .await
    .unwrap();

    let names = search_names(&lib, &ctx, "chimney").await;
    assert_eq!(
        names,
        vec!["chimney.png".to_string(), "plaster.png".to_string()],
        "the file named after the query must rank above the one merely noted about it"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notes_reach_export_output() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    let out = tmp.join("out");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, _sid) = scanned(&tmp.join("data"), &src).await;
    let id = find(&lib, &ctx, "brick.png").await;
    lib.set_note(
        &ctx,
        &id,
        NoteRequest {
            body: "Tiles vertically only.".into(),
        },
    )
    .await
    .unwrap();

    let dest = out.join("manifest.json");
    lib.export(
        &ctx,
        ExportRequest {
            assets: vec![id],
            collection: None,
            query: None,
            format: ExportFormat::Json,
            output: dest.to_string_lossy().into_owned(),
            attribution_only: false,
        },
    )
    .await
    .unwrap();

    let manifest = std::fs::read_to_string(&dest).unwrap();
    assert!(
        manifest.contains("Tiles vertically only."),
        "the user's own prose must survive an export: {manifest}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_caller_can_read_but_not_write_a_note() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, sid) = scanned(&tmp.join("data"), &src).await;
    let id = find(&lib, &ctx, "brick.png").await;
    lib.set_note(
        &ctx,
        &id,
        NoteRequest {
            body: "Needs a high-pass before use.".into(),
        },
    )
    .await
    .unwrap();

    // Reach the source, but with no write share on it.
    let mut scope = dam_api::VisibilityScope::default();
    scope.sources.insert(sid);
    let viewer = AuthContext::connected(
        Some("vera".into()),
        dam_api::Role::Viewer.scopes(),
        dam_api::Visibility::Restricted(scope),
    );

    assert_eq!(
        lib.get_note(&viewer, &id).await.unwrap().unwrap().body,
        "Needs a high-pass before use.",
        "a reader may read the note"
    );
    assert!(
        matches!(
            lib.set_note(
                &viewer,
                &id,
                NoteRequest {
                    body: "nope".into()
                }
            )
            .await,
            Err(LibError::Forbidden(_))
        ),
        "a reader must not be able to write the note"
    );
    assert_eq!(
        lib.get_note(&ctx, &id).await.unwrap().unwrap().body,
        "Needs a high-pass before use.",
        "the rejected write left nothing behind"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Attribution: the identity behind the edit is recorded so a shared library can say who wrote what
/// (`updated_by` stays a loose string until accounts land — issue #42).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_note_records_who_wrote_it() {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("brick.png"), PNG).unwrap();

    let ctx = AuthContext::embedded();
    let (lib, sid) = scanned(&tmp.join("data"), &src).await;
    let id = find(&lib, &ctx, "brick.png").await;

    // Embedded/local: nobody to attribute, and saying so is the honest answer.
    lib.set_note(
        &ctx,
        &id,
        NoteRequest {
            body: "local".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        lib.get_note(&ctx, &id).await.unwrap().unwrap().updated_by,
        None
    );

    let mut scope = dam_api::VisibilityScope::default();
    scope.sources.insert(sid);
    scope.write_sources.insert(sid);
    let editor = AuthContext::connected(
        Some("vera".into()),
        dam_api::Role::Editor.scopes(),
        dam_api::Visibility::Restricted(scope),
    );
    lib.set_note(
        &editor,
        &id,
        NoteRequest {
            body: "reviewed".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        lib.get_note(&ctx, &id)
            .await
            .unwrap()
            .unwrap()
            .updated_by
            .as_deref(),
        Some("vera")
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
