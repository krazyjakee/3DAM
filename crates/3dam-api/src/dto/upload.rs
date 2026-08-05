//! Upload (issue #80, tech-spec 08 §5.1): ingesting client-supplied bytes into a writable source.

use crate::id::{AssetId, SourceId};
use serde::{Deserialize, Serialize};

/// How an upload resolves a name that is already taken.
///
/// Deliberately **not** [`CollisionRule`]: this enum has no `Overwrite` arm, and that absence is
/// the feature. Upload is the one sanctioned path that writes inside a source tree, and it stays
/// non-destructive because clobbering an existing file is not expressible in the API at all — no
/// flag, no admin toggle, no request field can produce it (tech-spec 08 §5.1). Sharing
/// `CollisionRule` would have put an `Overwrite` variant one typo away from a destroyed original.
///
/// [`CollisionRule`]: super::convert::CollisionRule
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadCollision {
    /// The name is taken → the upload fails and writes nothing. The safe default.
    #[default]
    Fail,
    /// Disambiguate: `brick.png` → `brick-1.png`, `brick-2.png`, …
    Suffix,
    /// Leave the existing file alone and report the upload as skipped.
    Skip,
}

/// Write one file into a registered source at an explicit user request.
///
/// The bytes travel out-of-band (an HTTP body, or a staged file for an in-process caller) rather
/// than in this struct, so a multi-gigabyte asset is never held in memory or serialised through a
/// DTO.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadRequest {
    /// Destination source. Must be writable; a federated peer is always rejected.
    pub source: SourceId,
    /// Source-relative destination directory. Empty means the source root. Created if missing.
    #[serde(default)]
    pub folder: String,
    /// The bare filename to create. Validated against the path-safety battery; rejected, never
    /// silently sanitised, so the user always gets the name they asked for or a clear error.
    pub name: String,
    #[serde(default)]
    pub collision: UploadCollision,
}

/// What became of one uploaded file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadOutcome {
    /// The source-relative path actually written — differs from the requested name under
    /// [`UploadCollision::Suffix`], which is why the client is told rather than left to guess.
    pub path: String,
    /// True when [`UploadCollision::Skip`] left an existing file in place. Nothing was written.
    pub skipped: bool,
    pub size: u64,
    /// The catalogued asset, when the file was one 3DAM understands.
    #[serde(default)]
    pub asset: Option<AssetId>,
    /// Why the file was stored but not catalogued (an unsupported format). The file is on disk
    /// either way: the user asked to put it somewhere, so refusing the write would be the wrong
    /// answer — but silently omitting it from the catalog would be a worse one.
    #[serde(default)]
    pub uncatalogued_reason: Option<String>,
}
