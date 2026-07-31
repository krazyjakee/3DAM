//! Uploading into a source (issue #80, slices 3–4) — the engine half.
//!
//! The whole feature rests on one claim: 3DAM can create a file inside a registered source without
//! ever destroying something already there. These tests are that claim, stated as behaviour —
//! every collision rule, the path-safety guards at the *engine* boundary (the name battery itself
//! is unit-tested in `dam-sources`), and the two cases where a file is written but deliberately not
//! catalogued.
//!
//! The regression that matters most is `an_upload_never_replaces_an_existing_file`: it is the only
//! test that fails if someone ever adds an overwrite arm to make a collision "just work".

use dam_api::dto::*;
use dam_api::id::SourceId;
use dam_api::service::{AuthContext, LibraryService};
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_tmp() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "3dam-upload-{}-{}-{}",
        std::process::id(),
        nanos,
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

fn write_png(path: &Path, w: u32, h: u32) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x * 7 % 256) as u8, (y * 5 % 256) as u8, 90, 255])
    });
    img.save(path).unwrap();
}

/// A library with one registered local source, plus a staged file to upload.
async fn fixture() -> (EmbeddedLibrary, SourceId, PathBuf, PathBuf) {
    let tmp = unique_tmp();
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
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
                name: Some("library".into()),
                options: Default::default(),
            },
        )
        .await
        .unwrap();
    let staged = tmp.join("staged.png");
    write_png(&staged, 16, 16);
    (lib, sid, src, staged)
}

fn req(source: SourceId, folder: &str, name: &str, collision: UploadCollision) -> UploadRequest {
    UploadRequest {
        source,
        folder: folder.into(),
        name: name.into(),
        collision,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upload_lands_in_the_chosen_folder_and_is_catalogued() {
    let (lib, sid, src, staged) = fixture().await;
    let ctx = AuthContext::embedded();

    let out = lib
        .upload(
            &ctx,
            req(sid, "Textures/brick", "brick.png", UploadCollision::Fail),
            &staged,
        )
        .await
        .unwrap();

    assert_eq!(out.path, "Textures/brick/brick.png");
    assert!(!out.skipped);
    assert!(out.uncatalogued_reason.is_none(), "{out:?}");
    assert!(
        src.join("Textures/brick/brick.png").exists(),
        "the destination folder is created and the bytes land in it"
    );

    // Catalogued means *findable*, not merely inserted: the row has to carry the media type and
    // reach the query path, which is the whole point of uploading into a managed library.
    let asset = out.asset.expect("catalogued");
    let got = lib.get_asset(&ctx, &asset).await.unwrap();
    assert_eq!(got.summary.name, "brick.png");
    assert_eq!(got.summary.media, MediaType::Image);

    let hits = lib.query(&ctx, QueryRequest::default()).await.unwrap();
    assert!(
        hits.items.iter().any(|a| a.id == asset),
        "an uploaded asset is queryable immediately, not only after a rescan"
    );
}

/// The invariant the entire feature is built on (tech-spec 08 §5.1). If this test ever needs
/// changing to make a feature work, the feature is wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upload_never_replaces_an_existing_file() {
    let (lib, sid, src, staged) = fixture().await;
    let ctx = AuthContext::embedded();
    std::fs::write(src.join("brick.png"), b"the original bytes").unwrap();

    let err = lib
        .upload(
            &ctx,
            req(sid, "", "brick.png", UploadCollision::Fail),
            &staged,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, LibError::Conflict(_)), "got {err:?}");
    assert_eq!(
        std::fs::read(src.join("brick.png")).unwrap(),
        b"the original bytes",
        "a refused upload leaves the existing file byte-identical"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suffix_disambiguates_and_skip_leaves_the_original() {
    let (lib, sid, src, staged) = fixture().await;
    let ctx = AuthContext::embedded();
    std::fs::write(src.join("brick.png"), b"original").unwrap();

    let out = lib
        .upload(
            &ctx,
            req(sid, "", "brick.png", UploadCollision::Suffix),
            &staged,
        )
        .await
        .unwrap();
    assert_eq!(out.path, "brick-1.png");
    assert!(out.asset.is_some());
    assert_eq!(
        std::fs::read(src.join("brick.png")).unwrap(),
        b"original",
        "suffixing must not touch the file it stepped around"
    );

    // A second suffixed upload steps past both.
    let out2 = lib
        .upload(
            &ctx,
            req(sid, "", "brick.png", UploadCollision::Suffix),
            &staged,
        )
        .await
        .unwrap();
    assert_eq!(out2.path, "brick-2.png");

    let skipped = lib
        .upload(
            &ctx,
            req(sid, "", "brick.png", UploadCollision::Skip),
            &staged,
        )
        .await
        .unwrap();
    assert!(skipped.skipped);
    assert!(skipped.asset.is_none());
    assert_eq!(
        std::fs::read(src.join("brick.png")).unwrap(),
        b"original",
        "skip writes nothing at all"
    );
}

/// The destination folder the user picked is part of the promise: a name that climbs out of it must
/// be refused even though it would still land inside the source root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hostile_name_or_folder_is_refused_and_writes_nothing() {
    let (lib, sid, src, staged) = fixture().await;
    let ctx = AuthContext::embedded();

    for (folder, name) in [
        ("Textures", "../escaped.png"),
        ("Textures", "sub/nested.png"),
        ("../outside", "brick.png"),
        ("/etc", "brick.png"),
        ("Textures", "CON"),
        ("Textures", "trailing."),
        // Renders as `photo.png` via a bidi override but is really `photo.gnp.exe`.
        ("Textures", "photo\u{202E}gnp.exe"),
    ] {
        let err = lib
            .upload(&ctx, req(sid, folder, name, UploadCollision::Fail), &staged)
            .await
            .unwrap_err();
        assert!(
            matches!(err, LibError::BadRequest(_)),
            "{folder}/{name} should be a bad request, got {err:?}"
        );
    }

    assert!(
        !src.join("escaped.png").exists() && !src.join("Textures/sub").exists(),
        "a rejected upload leaves nothing behind, not even a directory"
    );
    // The parent of the source root must be untouched by the `../` attempts.
    let outside = src.parent().unwrap().join("outside");
    assert!(!outside.exists(), "nothing was created outside the root");
}

/// "The user asked to put a file somewhere" — so store it, and say plainly that the catalog does
/// not hold it. Refusing the write, or writing it silently, are both worse answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsupported_file_is_stored_and_reported_as_not_catalogued() {
    let (lib, sid, src, _staged) = fixture().await;
    let ctx = AuthContext::embedded();
    let odd = src.parent().unwrap().join("notes.xyzzy");
    std::fs::write(&odd, b"not a media file").unwrap();

    let out = lib
        .upload(
            &ctx,
            req(sid, "", "notes.xyzzy", UploadCollision::Fail),
            &odd,
        )
        .await
        .unwrap();

    assert!(!out.skipped);
    assert!(out.asset.is_none(), "nothing to catalogue");
    assert!(
        out.uncatalogued_reason
            .as_deref()
            .is_some_and(|r| r.contains("not catalogued")),
        "the client is told why: {:?}",
        out.uncatalogued_reason
    );
    assert!(
        src.join("notes.xyzzy").exists(),
        "the file is still written — the user asked for it"
    );
}

/// Upload must not become a way to reinstate bytes the user removed *and blocked* (issue #21).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_content_is_stored_but_stays_out_of_the_catalog() {
    let (lib, sid, src, staged) = fixture().await;
    let ctx = AuthContext::embedded();

    let first = lib
        .upload(
            &ctx,
            req(sid, "", "brick.png", UploadCollision::Fail),
            &staged,
        )
        .await
        .unwrap();
    let asset = first.asset.unwrap();

    // Remove *and block* — the user's "never show me these bytes again".
    lib.remove_asset(&ctx, &asset, RemoveAsset { block: true })
        .await
        .unwrap();

    let again = lib
        .upload(
            &ctx,
            req(sid, "", "brick-copy.png", UploadCollision::Fail),
            &staged,
        )
        .await
        .unwrap();
    assert!(
        again.asset.is_none(),
        "blocked bytes must not re-enter the catalog through upload"
    );
    assert!(again
        .uncatalogued_reason
        .as_deref()
        .is_some_and(|r| r.contains("blocklist")));
    assert!(
        src.join("brick-copy.png").exists(),
        "the write itself still happens; only the catalog row is refused"
    );
}

/// Metadata must be read from the file *where it landed*, not from the scratch copy.
///
/// Extraction resolves a file's siblings relative to its own directory, so cataloguing the staged
/// copy would look for them in the scratch dir and find nothing. `size` on the emitted asset is the
/// observable consequence: for a model it is the file plus its external dependencies, so a wrong
/// directory silently under-reports the asset by its whole texture/buffer set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_is_read_from_the_destination_not_the_scratch_copy() {
    let (lib, sid, src, _staged) = fixture().await;
    let ctx = AuthContext::embedded();

    // A glTF whose buffer sits beside it — the sibling only exists in the destination folder.
    let tmp = src.parent().unwrap();
    let gltf_src = tmp.join("staged-scene.gltf");
    std::fs::write(
        &gltf_src,
        br#"{"asset":{"version":"2.0"},"buffers":[{"uri":"scene.bin","byteLength":2048}]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(src.join("Models")).unwrap();
    std::fs::write(src.join("Models/scene.bin"), vec![0u8; 2048]).unwrap();

    let out = lib
        .upload(
            &ctx,
            req(sid, "Models", "scene.gltf", UploadCollision::Fail),
            &gltf_src,
        )
        .await
        .unwrap();

    let asset = out.asset.expect("a gltf is catalogued");
    let got = lib.get_asset(&ctx, &asset).await.unwrap();
    let MediaAttributes::Model(m) = &got.attributes else {
        panic!("expected model attributes, got {:?}", got.attributes);
    };
    assert_eq!(
        m.dependency_bytes,
        Some(2048),
        "the sibling .bin must be found next to the *written* file, not in scratch"
    );
}

/// A source that cannot actually be written to must say so *before* the user picks files, and must
/// refuse the write if they get there anyway.
///
/// Registering a federated peer needs a reachable peer (`add_source` probes it), so the
/// peer-is-never-writable half is a unit test beside `run_upload` in `dam-core/src/upload.rs`,
/// where a source row of any kind can be built without a server. What is covered here is the case
/// a unit test cannot reach and an operator actually hits: a real local directory the process is
/// not allowed to write into.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn a_read_only_source_is_not_a_destination() {
    use std::os::unix::fs::PermissionsExt;
    let (lib, sid, src, staged) = fixture().await;
    let ctx = AuthContext::embedded();

    // Writable to begin with — the probe reports capability, not a guess from the source kind.
    assert!(lib.get_source(&ctx, &sid).await.unwrap().writable);

    let restore = std::fs::metadata(&src).unwrap().permissions();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o555)).unwrap();

    let listed = lib.list_sources(&ctx).await.unwrap();
    let local = listed.iter().find(|s| s.id == sid).unwrap();
    assert!(
        !local.writable,
        "a read-only mount must be reported before the user chooses files"
    );
    // The listing and the single-source read cannot disagree about whether a drop will work.
    assert!(!lib.get_source(&ctx, &sid).await.unwrap().writable);

    let err = lib
        .upload(
            &ctx,
            req(sid, "", "brick.png", UploadCollision::Fail),
            &staged,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, LibError::Forbidden(_)), "got {err:?}");

    std::fs::set_permissions(&src, restore).unwrap();
}
