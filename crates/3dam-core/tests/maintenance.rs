//! Storage & maintenance (tech-spec 10 §5): startup builds one cache inventory off the async
//! runtime; usage reads and explicit maintenance update it without repeated directory walks.

use dam_api::admin::CacheTarget;
use dam_core::EmbeddedLibrary;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_tmp() -> PathBuf {
    // Per-process atomic counter as well as a timestamp, for the reason `scan.rs` documents:
    // these tests run in parallel within one process and `as_nanos()` can coincide for two that
    // start in the same clock tick, silently sharing a data dir. The window is only as narrow as
    // `open` is fast, so it widens whenever engine startup gains work.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-maint-{}-{}-{}", std::process::id(), nanos, n))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_usage_uses_live_inventory_and_clear_is_authoritative() {
    let tmp = unique_tmp();
    let thumbs = tmp.join("cache").join("thumbnails");
    std::fs::create_dir_all(&thumbs).unwrap();
    std::fs::write(thumbs.join("a.webp"), [0u8; 100]).unwrap();
    std::fs::write(thumbs.join("b.webp"), [0u8; 100]).unwrap();

    let lib = EmbeddedLibrary::open(&tmp).await.unwrap();

    let first = lib.storage_usage().await.unwrap();
    assert_eq!(first.thumbnails.files, 2);
    assert_eq!(first.thumbnails.bytes, 200);

    // A file added behind the controller's back is invisible — usage never re-walks the tree.
    std::fs::write(thumbs.join("c.webp"), [0u8; 100]).unwrap();
    let cached = lib.storage_usage().await.unwrap();
    assert_eq!(cached.thumbnails.files, 2, "usage must not rescan the tree");

    // Explicit clear may walk and also removes out-of-band files, then updates live accounting.
    let report = lib.clear_caches(CacheTarget::Thumbnails).await.unwrap();
    assert_eq!(report.files_deleted, 3);
    let fresh = lib.storage_usage().await.unwrap();
    assert_eq!(
        fresh.thumbnails.files, 0,
        "clear must update the live inventory"
    );
    assert_eq!(fresh.thumbnails.bytes, 0);

    std::fs::remove_dir_all(&tmp).ok();
}
