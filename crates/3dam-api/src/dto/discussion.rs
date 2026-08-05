//! Per-asset discussion (issue #82): the comment thread attached to a single asset.

use crate::id::{AssetId, CommentId};
use serde::{Deserialize, Serialize};

//
// The deliberate counterpart to [`Note`]. A note is **one durable editable annotation** answering
// "what should I know about this asset?"; a discussion is **append-only authored history** answering
// "what did we decide about it?". Both can exist on one asset, but only ever in the multi-user
// posture: discussion is gated on user accounts and is simply absent otherwise, so the single-user
// local library — the common case — never sees two text boxes and has to guess which is which.

/// Who wrote a message.
///
/// `id` is the account id as stored in `library.db`; `display` is resolved at *read* time against
/// `server.db`, across the database boundary. Deleting an account therefore leaves its messages
/// intact and merely unresolved (`display: None` → "deleted user") rather than cascade-deleting
/// them, which would silently rewrite a project's history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommentAuthor {
    pub id: String,
    /// `None` when the account no longer exists, or when nothing resolved it (the embedded engine
    /// has no account store to ask).
    #[serde(default)]
    pub display: Option<String>,
}

/// One message in an asset's discussion thread.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub asset: AssetId,
    pub author: CommentAuthor,
    /// Empty once deleted — the row survives as a tombstone so replies keep their parent.
    pub body: String,
    pub created_at: i64,
    /// Non-`None` ⇒ the client shows an "edited" marker.
    #[serde(default)]
    pub edited_at: Option<i64>,
    /// Non-`None` ⇒ a tombstone: the message is gone but the thread stays coherent.
    #[serde(default)]
    pub deleted_at: Option<i64>,
    /// The message this replies to, if any. One level of quoting, not arbitrary nesting — deep
    /// trees are a lot of UI for little value on what is usually a three-message exchange. The
    /// column costs nothing to carry and keeps the option open.
    #[serde(default)]
    pub reply_to: Option<CommentId>,
}

/// Post a message to an asset's thread.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewComment {
    pub body: String,
    #[serde(default)]
    pub reply_to: Option<CommentId>,
}

/// Replace a message's text. Author-only; sets `edited_at`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditComment {
    pub body: String,
}
