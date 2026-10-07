//! SFTP backend coverage, against a real SSH+SFTP server running in-process (`tests/support`).
//!
//! Before this, `src/sftp.rs` had no test at all — the only way to exercise it was to point it at a
//! live host. The harness closes that gap: every assertion below is about bytes and directory
//! entries that actually exist on disk in the served root.

#![cfg(feature = "sftp")]

mod support;

use dam_sources::FileSource;
use dam_sources::{ConnOptions, SourceConnection};
use support::TestSftpServer;

/// Collect a source's tree into (rel_path, size) pairs, sorted.
fn walk_all(src: &dyn FileSource) -> Vec<(String, u64)> {
    let mut found = Vec::new();
    src.walk(&mut |entry| {
        match entry {
            Ok(e) => found.push((e.rel_path, e.size)),
            Err(e) => panic!("walk error: {e}"),
        }
        true
    })
    .expect("walk");
    found.sort();
    found
}

#[test]
fn the_sftp_source_walks_and_fetches_real_files() {
    let root = tempfile::tempdir().expect("root");
    let scratch = tempfile::tempdir().expect("scratch");

    std::fs::write(root.path().join("kick.wav"), b"RIFFkick-bytes").unwrap();
    std::fs::create_dir(root.path().join("textures")).unwrap();
    std::fs::write(root.path().join("textures/brick.png"), b"\x89PNGbrick").unwrap();

    let srv = TestSftpServer::start(root.path());
    let src = srv.connect(scratch.path()).expect("connect sftp source");

    assert_eq!(
        walk_all(src.as_ref()),
        vec![
            ("kick.wav".to_string(), 14),
            ("textures/brick.png".to_string(), 9),
        ],
        "walk must recurse into subdirectories and report real sizes"
    );

    let fetched = src.fetch("textures/brick.png").expect("fetch");
    assert_eq!(
        std::fs::read(fetched.path()).unwrap(),
        b"\x89PNGbrick",
        "fetch must materialise the remote bytes verbatim"
    );
    assert!(
        fetched.path().starts_with(scratch.path()),
        "a remote fetch must land in the scratch dir, not the OS temp dir (issue #87)"
    );
}

#[test]
fn listing_delivers_progress_before_the_last_page_and_closes_once() {
    use std::sync::atomic::Ordering;

    let root = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    for index in 0..100 {
        std::fs::write(root.path().join(format!("file-{index}.png")), b"texture").unwrap();
    }
    let server = TestSftpServer::start(root.path());
    let source = server.connect(scratch.path()).unwrap();
    let mut count = 0;
    source
        .walk(&mut |entry| {
            entry.unwrap();
            count += 1;
            if count == 1 {
                assert_eq!(server.listing.reads.load(Ordering::Relaxed), 1);
                assert_eq!(server.listing.closes.load(Ordering::Relaxed), 0);
            }
            true
        })
        .unwrap();
    assert_eq!(count, 100);
    assert_eq!(server.listing.opens.load(Ordering::Relaxed), 1);
    assert_eq!(server.listing.reads.load(Ordering::Relaxed), 5);
    assert_eq!(server.listing.closes.load(Ordering::Relaxed), 1);
}

#[test]
fn listing_cancellation_stops_pages_even_when_every_path_is_filtered() {
    use std::cell::Cell;
    use std::sync::atomic::Ordering;

    let root = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    for index in 0..100 {
        std::fs::write(root.path().join(format!("file-{index}.csv")), b"ignored").unwrap();
    }
    let server = TestSftpServer::start(root.path());
    let source = server.connect(scratch.path()).unwrap();
    let cancelled = Cell::new(false);
    let result = source.walk_filtered(
        &mut |_| {
            cancelled.set(true);
            Ok(false)
        },
        &mut || {
            if cancelled.get() {
                Err(dam_api::LibError::Internal("cancelled fixture".into()))
            } else {
                Ok(())
            }
        },
        &mut |_| panic!("filtered paths must not reach the sink"),
    );
    assert!(result.is_err());
    assert_eq!(server.listing.reads.load(Ordering::Relaxed), 1);
    assert_eq!(server.listing.closes.load(Ordering::Relaxed), 1);
    assert_eq!(
        walk_all(source.as_ref()).len(),
        100,
        "session remains usable"
    );
}

#[test]
fn listing_sink_stop_and_partial_page_errors_close_the_directory() {
    use std::sync::atomic::Ordering;

    let root = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    for index in 0..100 {
        std::fs::write(root.path().join(format!("file-{index}.png")), b"texture").unwrap();
    }
    let server = TestSftpServer::start(root.path());
    let source = server.connect(scratch.path()).unwrap();
    source.walk(&mut |_| false).unwrap();
    assert_eq!(server.listing.reads.load(Ordering::Relaxed), 1);
    assert_eq!(server.listing.closes.load(Ordering::Relaxed), 1);

    server.listing.fail_read.store(3, Ordering::Relaxed);
    let mut successes = 0;
    let mut warnings = 0;
    source
        .walk(&mut |entry| {
            match entry {
                Ok(_) => successes += 1,
                Err(_) => warnings += 1,
            }
            true
        })
        .unwrap();
    assert_eq!(successes, 32);
    assert_eq!(warnings, 1);
    assert_eq!(server.listing.closes.load(Ordering::Relaxed), 2);
}

#[test]
fn fetch_hash_matches_materialized_bytes_with_empty_files_and_short_remote_reads() {
    use std::sync::atomic::Ordering;

    let root = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let server = TestSftpServer::start(root.path());
    server
        .listing
        .content_read_limit
        .store(7, Ordering::Relaxed);
    let source = server.connect(scratch.path()).unwrap();
    for bytes in [Vec::new(), (0..200).map(|index| index as u8).collect()] {
        std::fs::write(root.path().join("asset.png"), &bytes).unwrap();
        let fetched = source.fetch("asset.png").unwrap();
        let expected = blake3::hash(&bytes).to_hex().to_string();
        assert_eq!(fetched.content_hash(), Some(expected.as_str()));
        assert_eq!(fetched.source_stat().unwrap().len, bytes.len() as u64);
        assert_eq!(std::fs::read(fetched.path()).unwrap(), bytes);
        drop(fetched);
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }
}

#[test]
fn cancelled_remote_hash_fetch_removes_its_partial_materialization() {
    let root = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("asset.png"), vec![0x5a; 1024 * 1024]).unwrap();
    let server = TestSftpServer::start(root.path());
    let source = server.connect(scratch.path()).unwrap();
    let mut calls = 0;
    let outcome = source.fetch_paced("asset.png", &mut |_| {
        calls += 1;
        if calls == 2 {
            Err(dam_api::LibError::Internal("cancelled fixture".into()))
        } else {
            Ok(())
        }
    });
    assert!(outcome.is_err());
    assert_eq!(calls, 2);
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    let fetched = source.fetch("asset.png").unwrap();
    assert_eq!(fetched.source_stat().unwrap().len, 1024 * 1024);
}

#[test]
fn an_ipv6_sftp_url_connects_when_ipv6_loopback_is_available() {
    let root = tempfile::tempdir().expect("root");
    let scratch = tempfile::tempdir().expect("scratch");
    std::fs::write(root.path().join("ipv6.txt"), b"over IPv6").unwrap();

    let Some(srv) = TestSftpServer::start_ipv6(root.path()) else {
        return;
    };
    let uri = format!(
        "sftp://{}:{}@[{}]:{}/",
        srv.username(),
        srv.password(),
        srv.host(),
        srv.port()
    );
    let connection = SourceConnection::parse("sftp", &uri, &ConnOptions::default())
        .expect("parse bracketed IPv6 SFTP URL");
    let source = dam_sources::open_source(&connection, scratch.path())
        .expect("connect to IPv6 SFTP fixture");

    assert_eq!(walk_all(source.as_ref()), vec![("ipv6.txt".into(), 9)]);
}

// ── write side (issue #80 slice 7) ──────────────────────────────────────────
//
// The point of these is that they run over the real SFTP protocol. Create-only for a remote source
// is carried by `CREATE|EXCLUDE` and by a rename the server refuses when the target exists — two
// server-side behaviours that no amount of client-side reasoning can confirm. Asserting them
// against an actual server is the only way to know the invariant survives the wire.

/// A helper source rooted at a fresh server, plus the directory it serves.
fn writable_server() -> (tempfile::TempDir, tempfile::TempDir, TestSftpServer) {
    let root = tempfile::tempdir().expect("root");
    let scratch = tempfile::tempdir().expect("scratch");
    let srv = TestSftpServer::start(root.path());
    (root, scratch, srv)
}

#[test]
fn put_creates_remote_files_and_parent_directories() {
    let (root, scratch, srv) = writable_server();
    let src = srv.connect(scratch.path()).expect("connect");

    assert!(src.writable(), "an SFTP source is a valid destination");

    src.mkdir("Textures/Brick").expect("mkdir");
    assert!(
        srv.root().join("Textures/Brick").is_dir(),
        "mkdir must create the whole chain, not just the leaf"
    );
    // Idempotent, exactly like the local backend: an existing directory is success.
    src.mkdir("Textures/Brick").expect("mkdir again");

    src.put("Textures/Brick/wall.png", &mut &b"\x89PNGwall"[..])
        .expect("put");
    assert_eq!(
        std::fs::read(srv.root().join("Textures/Brick/wall.png")).unwrap(),
        b"\x89PNGwall",
        "the bytes must land at the destination verbatim"
    );

    // `put` creates missing parents itself, so an upload into a new folder is one call.
    src.put("Fresh/deep/note.txt", &mut &b"hello"[..])
        .expect("put into a new tree");
    assert_eq!(
        std::fs::read(srv.root().join("Fresh/deep/note.txt")).unwrap(),
        b"hello"
    );

    // Nothing may be left behind by a successful upload.
    let strays: Vec<_> = walkdir::WalkDir::new(root.path())
        .into_iter()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("3dam-part"))
        .collect();
    assert!(strays.is_empty(), "temp files survived a successful put");
}

/// The invariant, over the wire. If this ever needs changing to make something work, the something
/// is wrong.
#[test]
fn a_remote_put_never_replaces_an_existing_file() {
    let (_root, scratch, srv) = writable_server();
    let src = srv.connect(scratch.path()).expect("connect");
    std::fs::write(srv.root().join("brick.png"), b"the original bytes").unwrap();

    let err = src
        .put("brick.png", &mut &b"replacement"[..])
        .expect_err("a taken name must be refused");
    assert!(
        matches!(err, dam_api::LibError::Conflict(_)),
        "a collision must be a Conflict so Suffix/Skip can recover from it: {err:?}"
    );
    assert_eq!(
        std::fs::read(srv.root().join("brick.png")).unwrap(),
        b"the original bytes",
        "a refused upload must leave the existing file byte-identical"
    );
}

/// The case the pre-check cannot catch: the destination appears *while* the bytes are in flight.
///
/// Only the rename can refuse this, so this is the test that distinguishes "we looked first" from
/// "the protocol refused". It is also why the harness implements SSH_FXP_RENAME as fail-if-exists
/// rather than `fs::rename` — with POSIX semantics this test would pass by overwriting the file.
#[test]
fn a_file_appearing_mid_transfer_is_not_overwritten() {
    let (_root, scratch, srv) = writable_server();
    let src = srv.connect(scratch.path()).expect("connect");
    let target = srv.root().join("contested.bin");

    /// Creates the destination after the first chunk — i.e. after `put`'s `try_exists` has passed.
    struct RacyWriter {
        target: std::path::PathBuf,
        done: bool,
    }
    impl std::io::Read for RacyWriter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.done {
                return Ok(0);
            }
            self.done = true;
            std::fs::write(&self.target, b"SOMEONE ELSES FILE").unwrap();
            buf[..3].copy_from_slice(b"new");
            Ok(3)
        }
    }

    let err = src
        .put(
            "contested.bin",
            &mut RacyWriter {
                target: target.clone(),
                done: false,
            },
        )
        .expect_err("the rename must refuse a destination that appeared mid-transfer");
    assert!(
        matches!(err, dam_api::LibError::Conflict(_)),
        "a lost race is a collision, so Suffix can still recover: {err:?}"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"SOMEONE ELSES FILE",
        "the file that won the race must be untouched"
    );
}

/// A failed transfer must not leave its `.part` behind — the name is `EXCLUDE`-reserved, so a
/// leftover would make every retry of the same upload fail too.
#[test]
fn a_failed_transfer_leaves_no_part_file() {
    let (root, scratch, srv) = writable_server();
    let src = srv.connect(scratch.path()).expect("connect");

    /// Yields a little, then fails — a stand-in for a disconnect mid-upload.
    struct Flaky(usize);
    impl std::io::Read for Flaky {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                self.0 += 1;
                buf[..4].copy_from_slice(b"pear");
                return Ok(4);
            }
            Err(std::io::Error::other("link went down"))
        }
    }

    let err = src.put("half.bin", &mut Flaky(0)).expect_err("must fail");
    assert!(matches!(err, dam_api::LibError::Internal(_)), "{err:?}");

    assert!(
        !srv.root().join("half.bin").exists(),
        "a failed upload must not leave a truncated file at the real name"
    );

    // Nor the directories it created on the way in. A typo'd destination that then fails to
    // transfer would otherwise litter the user's source with empty folders they never made.
    let err = src
        .put("Fresh/deep/half.bin", &mut Flaky(0))
        .expect_err("must fail");
    assert!(matches!(err, dam_api::LibError::Internal(_)), "{err:?}");
    assert!(
        !srv.root().join("Fresh").exists(),
        "a failed upload rolled back neither its file nor its directories"
    );
    let strays: Vec<_> = walkdir::WalkDir::new(root.path())
        .into_iter()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("3dam-part"))
        .map(|e| e.path().to_path_buf())
        .collect();
    assert!(strays.is_empty(), "left a .part behind: {strays:?}");
}

/// Path safety is the same battery the local backend gets — it lives in `safe_name`, and the point
/// here is that the *remote* backend routes through it rather than trusting the caller.
#[test]
fn a_remote_put_rejects_hostile_names() {
    let (root, scratch, srv) = writable_server();
    let src = srv.connect(scratch.path()).expect("connect");

    // Not in this list: `sub/`. `check_rel_path` documents a trailing slash as tolerated and
    // normalised away, so it is a well-formed path, not an attack.
    for bad in [
        "../escape.png",
        "/etc/passwd",
        "a/../../b.png",
        "CON",
        "a\\b.png",
    ] {
        assert!(
            src.put(bad, &mut &b"x"[..]).is_err(),
            "{bad:?} must be refused"
        );
    }
    let outside = root.path().parent().unwrap().join("escape.png");
    assert!(!outside.exists(), "a rejected name escaped the served root");
}
