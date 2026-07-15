//! Storage & maintenance (tech-spec 10 §5): `storage_usage` TTL-caches the cache-tier walk — a
//! stat per cached file, minutes on a big cold HDD cache — so repeated Settings loads don't
//! re-pay it, while `clear_caches` invalidates the cached walk so the report goes live again.

use dam_api::admin::CacheTarget;
use dam_core::EmbeddedLibrary;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_tmp() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-maint-{}-{}", std::process::id(), nanos))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_usage_caches_the_walk_and_clear_invalidates() {
    let tmp = unique_tmp();
    let thumbs = tmp.join("cache").join("thumbnails");
    std::fs::create_dir_all(&thumbs).unwrap();
    std::fs::write(thumbs.join("a.webp"), [0u8; 100]).unwrap();
    std::fs::write(thumbs.join("b.webp"), [0u8; 100]).unwrap();

    let lib = EmbeddedLibrary::open(&tmp).await.unwrap();

    let first = lib.storage_usage().await.unwrap();
    assert_eq!(first.thumbnails.files, 2);
    assert_eq!(first.thumbnails.bytes, 200);

    // A file added behind the cache's back is invisible within the TTL — the walk is not re-run.
    std::fs::write(thumbs.join("c.webp"), [0u8; 100]).unwrap();
    let cached = lib.storage_usage().await.unwrap();
    assert_eq!(cached.thumbnails.files, 2, "walk must be TTL-cached");

    // Clearing a tier invalidates the cached walk: the next report is live (and empty).
    let report = lib.clear_caches(CacheTarget::Thumbnails).await.unwrap();
    assert_eq!(report.files_deleted, 3);
    let fresh = lib.storage_usage().await.unwrap();
    assert_eq!(
        fresh.thumbnails.files, 0,
        "clear must invalidate the TTL cache"
    );
    assert_eq!(fresh.thumbnails.bytes, 0);

    std::fs::remove_dir_all(&tmp).ok();
}
