use crate::credentials;
use dam_api::dto::{
    content_type_for, Asset, AssetContent, AssetContentMetadata, AssetContentStream, ContentRange,
    MAX_MATERIALIZED_CONTENT_BYTES,
};
use dam_api::LibError;
use dam_store::Store;
use std::path::Path;

/// Rebuild the asset's source backend and resolve its bytes to a private local temp path. Local
/// bytes are copied from an already-open root capability; remote bytes are downloaded. Pure/blocking.
pub(super) fn fetch_asset(
    store: &Store,
    secrets: &credentials::SecretVault,
    asset: &Asset,
    scratch: &Path,
) -> Result<dam_sources::Fetched, LibError> {
    store.ensure_content_ready(asset)?;
    let conn = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
    let fs = dam_sources::open_source(&conn, scratch)?;
    let fetched = fs.fetch(&asset.path)?;
    ensure_fetched_revision(store, asset, &fetched)?;
    Ok(fetched)
}

pub(super) fn ensure_fetched_revision(
    store: &Store,
    asset: &Asset,
    fetched: &dam_sources::Fetched,
) -> Result<(), LibError> {
    fetched.verify_unchanged()?;
    if let (Some(expected), Some(actual)) = (asset.hash, fetched.content_hash()) {
        if expected.to_hex() != actual {
            return Err(LibError::Conflict(
                "source bytes differ from the catalog revision".into(),
            ));
        }
    }
    store.ensure_content_ready(asset)
}

/// Read an asset's bytes for a preview, bounded by the content cap. Both the cheap catalog size and
/// a live source stat are checked before any (possibly remote) fetch, so a file which grew since its
/// last scan cannot turn this materialising compatibility path into an unbounded allocation.
/// Pure/blocking — called inside a `spawn_blocking` closure.
pub(super) fn read_asset_content(
    store: &Store,
    secrets: &credentials::SecretVault,
    asset: &Asset,
    scratch: &Path,
) -> Result<AssetContent, LibError> {
    store.ensure_content_ready(asset)?;
    let size = asset.summary.size;
    if size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "asset is {size} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes"
        )));
    }
    let conn = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
    let source = dam_sources::open_source(&conn, scratch)?;
    let live_size = source.content_stat(&asset.path)?.len;
    if live_size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "asset is {live_size} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes"
        )));
    }
    let fetched = source.fetch(&asset.path)?;
    let abs = fetched.path();
    let fetched_size = std::fs::metadata(abs)
        .map_err(|e| LibError::Internal(format!("stat fetched {}: {e}", abs.display())))?
        .len();
    if fetched_size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "asset is {fetched_size} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes"
        )));
    }
    let bytes = std::fs::read(abs)
        .map_err(|e| LibError::Internal(format!("read {}: {e}", abs.display())))?;
    ensure_fetched_revision(store, asset, &fetched)?;
    let media = asset.summary.media;
    let format = asset.summary.format.clone();
    let content_type = content_type_for(media, &format).to_string();
    Ok(AssetContent {
        bytes,
        content_type,
        format,
        media,
    })
}

pub(super) fn content_metadata(
    asset: &Asset,
    stat: dam_sources::ContentStat,
) -> AssetContentMetadata {
    AssetContentMetadata {
        len: stat.len,
        content_type: content_type_for(asset.summary.media, &asset.summary.format).to_string(),
        format: asset.summary.format.clone(),
        media: asset.summary.media,
        etag: asset.hash.map(|hash| format!("\"{hash}\"")),
    }
}

/// Open a bounded producer over one source. The queue holds at most two chunks (plus the producer
/// and HTTP consumer's current chunks): `blocking_send` applies backpressure, and receiver drop
/// tells every source backend to stop its range loop promptly.
pub(super) fn source_content_stream(
    store: std::sync::Arc<Store>,
    asset: Asset,
    source: Box<dyn dam_sources::FileSource>,
    metadata: AssetContentMetadata,
    range: ContentRange,
) -> AssetContentStream {
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    let consumer_store = store.clone();
    let consumer_asset = asset.clone();
    tokio::task::spawn_blocking(move || {
        let mut guard_error = None;
        let outcome = {
            let mut send = |chunk| {
                if let Err(error) = store.ensure_content_ready(&asset) {
                    guard_error = Some(error);
                    return false;
                }
                sender.blocking_send(Ok(chunk)).is_ok()
            };
            source.read_range(&asset.path, range.first(), range.len(), &mut send)
        };
        let outcome = outcome.and_then(|()| match guard_error {
            Some(error) => Err(error),
            None => store.ensure_content_ready(&asset),
        });
        if let Err(error) = outcome {
            let _ = sender.blocking_send(Err(error));
        }
    });
    // A producer can wait behind a slow consumer after checking the revision, and previously
    // approved chunks may already be buffered. Recheck at delivery without doing database work
    // on the async runtime; an error drops the receiver and stops the producer.
    let bytes = futures::stream::try_unfold(receiver, move |mut receiver| {
        let store = consumer_store.clone();
        let asset = consumer_asset.clone();
        async move {
            let Some(chunk) = receiver.recv().await else {
                return Ok(None);
            };
            let chunk = chunk?;
            tokio::task::spawn_blocking(move || store.ensure_content_ready(&asset))
                .await
                .map_err(|error| {
                    LibError::Internal(format!("content revision check failed: {error}"))
                })??;
            Ok::<_, LibError>(Some((chunk, receiver)))
        }
    });
    AssetContentStream {
        metadata,
        range,
        bytes: Box::pin(bytes),
    }
}

/// Resolve `rel` against the *directory* of `base` (a source-relative path), normalising `.`/`..`
/// and rejecting anything absolute or that escapes the source root. Returns a clean source-relative
/// path. This is the loose-glTF sibling resolver (#56); the source's own `fetch` is separately
/// traversal-guarded as defence-in-depth.
pub(super) fn resolve_sibling(base: &str, rel: &str) -> Result<String, LibError> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err(LibError::BadRequest("empty related path".into()));
    }
    // Absolute paths (POSIX or Windows-drive) and URLs are never source-relative siblings.
    let bytes = rel.as_bytes();
    let windows_drive =
        bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic();
    let uri_scheme = rel.split_once(':').is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme.chars().enumerate().all(|(i, c)| {
                c.is_ascii_alphabetic() || (i > 0 && (c.is_ascii_digit() || "+-.".contains(c)))
            })
    });
    if rel.starts_with('/') || rel.starts_with('\\') || windows_drive || uri_scheme {
        return Err(LibError::BadRequest(
            "related path must be source-relative".into(),
        ));
    }
    // Start from the base file's directory (drop its final component).
    let mut parts: Vec<&str> = base.split('/').collect();
    parts.pop();
    for seg in rel.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(LibError::BadRequest(
                        "related path escapes the source".into(),
                    ));
                }
            }
            s => parts.push(s),
        }
    }
    Ok(parts.join("/"))
}

/// Read a file relative to `asset`'s directory within the same source, bounded by the content cap.
/// Pure/blocking — called inside a `spawn_blocking` closure. Powers loose-glTF external buffers (#56).
pub(super) fn read_related_content(
    store: &Store,
    secrets: &credentials::SecretVault,
    asset: &Asset,
    rel: &str,
    scratch: &Path,
) -> Result<AssetContent, LibError> {
    store.ensure_content_ready(asset)?;
    let target = resolve_sibling(&asset.path, rel)?;
    store.ensure_source_path_ready(&asset.source_id, &target)?;
    let conn = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
    let fs = dam_sources::open_source(&conn, scratch)?;
    let size = fs.content_stat(&target)?.len;
    if size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "related file is {} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes",
            size
        )));
    }
    let fetched = fs.fetch(&target)?;
    let abs = fetched.path();
    let fetched_size = std::fs::metadata(abs)
        .map_err(|e| LibError::Internal(format!("stat fetched {}: {e}", abs.display())))?
        .len();
    if fetched_size > MAX_MATERIALIZED_CONTENT_BYTES {
        return Err(LibError::Unsupported(format!(
            "related file is {fetched_size} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes"
        )));
    }
    let bytes = std::fs::read(abs)
        .map_err(|e| LibError::Internal(format!("read {}: {e}", abs.display())))?;
    store.ensure_source_path_ready(&asset.source_id, &target)?;
    store.ensure_content_ready(asset)?;
    Ok(AssetContent {
        bytes,
        content_type: "application/octet-stream".to_string(),
        format: String::new(),
        media: asset.summary.media,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::dto::MediaType;
    use dam_api::id::{ContentHash, SourceId};
    use futures::StreamExt;
    use std::sync::Arc;

    struct RevisionChangingSource {
        store: Arc<Store>,
        source: SourceId,
    }

    impl dam_sources::FileSource for RevisionChangingSource {
        fn walk(
            &self,
            _sink: &mut dyn FnMut(Result<dam_sources::FileEntry, LibError>) -> bool,
        ) -> Result<(), LibError> {
            Ok(())
        }

        fn fetch(&self, _path: &str) -> Result<dam_sources::Fetched, LibError> {
            Err(LibError::Unsupported("fixture only supports ranges".into()))
        }

        fn read_range(
            &self,
            _path: &str,
            _offset: u64,
            _length: u64,
            sink: &mut dyn FnMut(Vec<u8>) -> bool,
        ) -> Result<(), LibError> {
            let generation = self.store.begin_source_scan(&self.source)?;
            self.store.apply_quick_discovery(
                &self.source,
                generation,
                &[dam_store::PendingDiscovery {
                    path: "asset.png".into(),
                    size: 2,
                    modified_ms: Some(2),
                    media: Some(MediaType::Image),
                    format: Some("png".into()),
                }],
            )?;
            assert!(
                !sink(vec![0x5a]),
                "a pending revision must stop the producer"
            );
            Ok(())
        }
    }

    struct BufferedSource {
        admitted: std::sync::mpsc::Sender<()>,
    }

    impl dam_sources::FileSource for BufferedSource {
        fn walk(
            &self,
            _sink: &mut dyn FnMut(Result<dam_sources::FileEntry, LibError>) -> bool,
        ) -> Result<(), LibError> {
            Ok(())
        }

        fn fetch(&self, _path: &str) -> Result<dam_sources::Fetched, LibError> {
            Err(LibError::Unsupported("fixture only supports ranges".into()))
        }

        fn read_range(
            &self,
            _path: &str,
            _offset: u64,
            _length: u64,
            sink: &mut dyn FnMut(Vec<u8>) -> bool,
        ) -> Result<(), LibError> {
            assert!(sink(vec![0x5a]));
            // The chunk has passed the producer guard and is in the bounded channel. The test
            // deliberately does not poll its stream until the pending revision is durable.
            self.admitted.send(()).unwrap();
            Ok(())
        }
    }

    #[tokio::test]
    async fn streaming_rejects_a_previously_buffered_chunk_after_invalidation() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let source = store
            .add_source(
                &dam_sources::SourceConnection::LocalFs {
                    root: "/buffered-revision".into(),
                },
                "test",
                false,
            )
            .unwrap();
        let (id, _) = store
            .upsert_asset(&dam_store::NewAsset {
                source_id: source,
                path: "asset.png".into(),
                filename: "asset.png".into(),
                content_hash: Some(ContentHash([1; 32])),
                size_bytes: Some(1),
                source_modified_at: Some(1),
                scanned_at: 1,
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();
        let asset = store.get_asset(&id).unwrap();
        let metadata = content_metadata(
            &asset,
            dam_sources::ContentStat {
                len: 1,
                modified_ms: Some(1),
            },
        );
        let (admitted, received) = std::sync::mpsc::channel();
        let mut stream = source_content_stream(
            store.clone(),
            asset,
            Box::new(BufferedSource { admitted }),
            metadata,
            ContentRange::new(0, 0).unwrap(),
        );
        tokio::task::spawn_blocking(move || {
            received
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        })
        .await
        .unwrap();
        let invalidate = store.clone();
        tokio::task::spawn_blocking(move || {
            let generation = invalidate.begin_source_scan(&source).unwrap();
            invalidate
                .apply_quick_discovery(
                    &source,
                    generation,
                    &[dam_store::PendingDiscovery {
                        path: "asset.png".into(),
                        size: 2,
                        modified_ms: Some(2),
                        media: Some(MediaType::Image),
                        format: Some("png".into()),
                    }],
                )
                .unwrap();
        })
        .await
        .unwrap();
        assert!(matches!(
            stream.bytes.next().await,
            Some(Err(LibError::Conflict(_)))
        ));
        assert!(stream.bytes.next().await.is_none());
    }

    #[tokio::test]
    async fn streaming_rejects_a_revision_queued_after_the_stream_was_opened() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let source = store
            .add_source(
                &dam_sources::SourceConnection::LocalFs {
                    root: "/stream-revision".into(),
                },
                "test",
                false,
            )
            .unwrap();
        let (id, _) = store
            .upsert_asset(&dam_store::NewAsset {
                source_id: source,
                path: "asset.png".into(),
                filename: "asset.png".into(),
                content_hash: Some(ContentHash([1; 32])),
                size_bytes: Some(1),
                source_modified_at: Some(1),
                scanned_at: 1,
                media_type: MediaType::Image,
                format: "png".into(),
            })
            .unwrap();
        let asset = store.get_asset(&id).unwrap();
        let metadata = content_metadata(
            &asset,
            dam_sources::ContentStat {
                len: 1,
                modified_ms: Some(1),
            },
        );
        let source = Box::new(RevisionChangingSource {
            store: store.clone(),
            source,
        });
        let mut stream = source_content_stream(
            store,
            asset,
            source,
            metadata,
            ContentRange::new(0, 0).unwrap(),
        );
        assert!(matches!(
            stream.bytes.next().await,
            Some(Err(LibError::Conflict(_)))
        ));
        assert!(stream.bytes.next().await.is_none());
    }
}
