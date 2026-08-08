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
    let conn = secrets.resolve(store.get_source_connection(&asset.source_id)?)?;
    let fs = dam_sources::open_source(&conn, scratch)?;
    fs.fetch(&asset.path)
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
    source: Box<dyn dam_sources::FileSource>,
    path: String,
    metadata: AssetContentMetadata,
    range: ContentRange,
) -> AssetContentStream {
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    tokio::task::spawn_blocking(move || {
        let mut send = |chunk| sender.blocking_send(Ok(chunk)).is_ok();
        if let Err(error) = source.read_range(&path, range.first(), range.len(), &mut send) {
            let _ = sender.blocking_send(Err(error));
        }
    });
    AssetContentStream {
        metadata,
        range,
        bytes: Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver)),
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
    let target = resolve_sibling(&asset.path, rel)?;
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
    Ok(AssetContent {
        bytes,
        content_type: "application/octet-stream".to_string(),
        format: String::new(),
        media: asset.summary.media,
    })
}
