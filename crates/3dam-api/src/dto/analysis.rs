//! Analysis and automation (tech-spec 05, phase 3): embeddings and prefetch, similarity, duplicate
//! detection and review, tag suggestion/editing, favourites, and per-asset notes.

use super::asset::AssetSummary;
use super::media::MediaType;
use super::query::{Filter, QueryRequest};
use crate::id::{AssetId, CollectionId};
use serde::{Deserialize, Serialize};

/// Submit an analysis pass. With no `assets`, the runner plans every asset that is *due* — behind
/// the current extractor versions (§1.2, §7.2) — so a re-run is incremental, not a full re-sweep.
/// `force` re-analyses even up-to-date assets (e.g. after tuning thresholds). Background job.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AnalyzeRequest {
    #[serde(default)]
    pub assets: Vec<AssetId>,
    #[serde(default)]
    pub force: bool,
}

/// Force the derived preview cache to be rebuilt for specific assets: drop each asset's cached
/// thumbnail PNG(s) (all edges + renderer variants) and its 3D preview blob, so the next read
/// re-renders from source. Content-keyed and non-destructive — only regenerable derivatives are
/// removed; the source bytes are never touched (PRODUCT_SPEC §8). Unlike the whole-tier admin
/// clear-cache, this is a per-asset front-end action (e.g. after a source file was edited in place).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThumbnailRegenRequest {
    pub assets: Vec<AssetId>,
}

/// What a thumbnail-regen pass dropped: how many assets were visited and cache files removed.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ThumbnailRegenReport {
    pub assets: u64,
    pub files_deleted: u64,
}

/// A prefetch hint (hosted mode, issue #72): the assets a client is about to render, so the server
/// warms their thumbnails (at `edge`) and — for models — preview meshes ahead of the HTTP fetch.
/// Fire-and-forget; the bytes still travel on HTTP/2 (ADR 0012), this only moves generation earlier.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PrefetchRequest {
    pub assets: Vec<AssetId>,
    /// Thumbnail long-edge to warm. `None` → the server's default grid edge.
    #[serde(default)]
    pub edge: Option<u32>,
    /// Internal federation hop marker. A receiving peer warms its local matches but does not relay
    /// the hint again, preventing cycles between mutually registered libraries.
    #[serde(default, skip_serializing_if = "is_false")]
    pub relay: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A "find similar" query. By-asset-id in v1 ("more like this"); the upload-a-reference entry point
/// (tech-spec 05 §3.2) lands with the MCP/web upload path. Scoped to the query asset's media space.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimilarRequest {
    pub asset: AssetId,
    /// How many neighbours to return (post-filter, post-self-drop).
    #[serde(default = "default_k")]
    pub k: u32,
    /// Compose with the same faceted filters as text search (§3.3).
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Answer from the local index only — no federated fan-out or peer forwarding (phase 6). Set
    /// on every peer-bound call so federation stays one hop, never transitive (ADR 0009 §5).
    #[serde(default)]
    pub local_only: bool,
}

fn default_k() -> u32 {
    24
}

/// One similarity hit: the neighbour plus its cosine score and the space it was ranked in
/// (the explanation, DESIGN_GUIDELINES §1.2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimilarHit {
    pub asset: AssetSummary,
    /// Cosine similarity in [0,1] (1 = identical direction).
    pub score: f32,
    /// The `EmbeddingSpace` id the ranking happened in.
    pub space: String,
}

/// Which duplicate tier to surface for the review view (tech-spec 05 §4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DupKind {
    /// Byte-identical (content-hash groups) — free, exact.
    #[default]
    Exact,
    /// Perceptually close but not identical (pHash / embedding cosine).
    Near,
}

/// Request the duplicate groups for review. Optionally scoped to one media type.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DupRequest {
    #[serde(default)]
    pub kind: DupKind,
    #[serde(default)]
    pub media: Option<MediaType>,
    #[serde(default = "default_dup_limit")]
    pub limit: u32,
    /// Opaque continuation returned by the previous duplicate-review page.
    #[serde(default)]
    pub after: Option<crate::Cursor>,
    /// Which durable review queue to show. Pending is the default working queue; reviewed groups
    /// remain reachable so a decision can be inspected or reopened after refresh.
    #[serde(default)]
    pub review: DupReviewFilter,
}

fn default_dup_limit() -> u32 {
    24
}

/// Hard response and work bounds for duplicate reads. Callers may request less, never more.
pub const DUP_GROUP_PAGE_MAX: u32 = 100;
pub const DUP_GROUP_MEMBER_MAX: usize = 100;
pub const DUP_MEMBERSHIP_ASSET_MAX: usize = 600;

/// Lightweight exact-duplicate lookup for a bounded set of browse rows.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DupMembershipRequest {
    pub assets: Vec<AssetId>,
}

/// Exact-duplicate membership without any member summaries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupMembership {
    pub asset: AssetId,
    /// Stable within this library: the lower-case content hash identifying the group.
    pub group: String,
    /// Visible members in the complete group, including `asset`.
    pub count: u32,
}

/// Continue through the members of one exact duplicate group.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupGroupMembersRequest {
    /// Group key returned on [`DupGroup`].
    pub group: String,
    #[serde(default)]
    pub after: Option<crate::Cursor>,
    #[serde(default = "default_dup_limit")]
    pub limit: u32,
}

/// One duplicate-review member with the comparison context deliberately omitted from ordinary
/// browse summaries. This is still metadata-only: no source bytes are read.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupMember {
    pub asset: AssetSummary,
    pub path: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyzed_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DupReviewState {
    #[default]
    Pending,
    Resolved,
    Dismissed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DupReviewFilter {
    #[default]
    Pending,
    Resolved,
    Dismissed,
    All,
}

/// Persist one review decision. Choosing a keep is non-destructive and leaves the group pending;
/// resolve/dismiss move it out of the default queue, while Pending reopens it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupReviewRequest {
    pub review: String,
    pub state: DupReviewState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<AssetId>,
    /// Catalog-only removals committed in the same transaction as the review state. `block` is
    /// content-addressed and can therefore remove every exact copy; source files remain untouched.
    #[serde(default)]
    pub removals: Vec<DupReviewRemoval>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupReviewRemoval {
    pub asset: AssetId,
    #[serde(default)]
    pub block: bool,
}

/// A cluster of duplicates for the review view (§4.3). Never auto-deleted — 3DAM only groups.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupGroup {
    pub kind: DupKind,
    pub media: MediaType,
    /// Exact-group key used to continue member pagination. `None` for computed near groups.
    #[serde(default)]
    pub group: Option<String>,
    /// Stable identity for the current group, used by the durable review record.
    pub review: String,
    #[serde(default)]
    pub review_state: DupReviewState,
    pub members: Vec<DupMember>,
    /// Visible members in the complete group. `members` is capped for bounded responses.
    pub total_members: u32,
    /// Continue this group's member list without reloading its first summaries.
    #[serde(default)]
    pub members_cursor: Option<crate::Cursor>,
    /// The pairwise signal that linked the group — the explanation (§4.3).
    pub signal: String,
    /// A suggested "keep" (highest resolution / most-permissive / largest); the user disposes.
    pub suggested_keep: AssetId,
    /// Why the automated choice won. Separate from `signal`, which explains why the members were
    /// grouped rather than why one is preferable to keep.
    pub suggested_keep_reason: String,
    /// A user override, persisted independently of the suggestion. `None` means use the suggestion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen_keep: Option<AssetId>,
}

/// Accept, reject, or undo one auto-suggested tag decision (the one-action lifecycle, §1.4).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SuggestionReview {
    pub asset: AssetId,
    pub tag: String,
    pub action: ReviewAction,
}

/// Add/remove user-authored tags over one explicit or server-resolved selection (issue #121).
/// Explicit ids take precedence, followed by collection, then query (matching export selection).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagEditRequest {
    #[serde(default)]
    pub assets: Vec<AssetId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<CollectionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRequest>,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
    /// Calculate the same authorized delta without changing the catalog.
    #[serde(default)]
    pub dry_run: bool,
}

/// Summary-shaped bulk result: bounded warnings rather than one response row per target.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagEditResult {
    pub matched: u64,
    pub changed: u64,
    pub additions: u64,
    pub removals: u64,
    #[serde(default)]
    pub warnings: Vec<crate::ItemWarning>,
}

pub const TAG_EDIT_EXPLICIT_MAX: usize = 1_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagListRequest {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default = "default_tag_list_limit")]
    pub limit: u32,
}

fn default_tag_list_limit() -> u32 {
    20
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub count: u64,
    /// At least one visible assignment was explicitly user-authored.
    pub manual: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAction {
    Accept,
    Reject,
    Undo,
}

/// Flag or unflag one asset as a favourite (issue #63). Reversible; the state lives in the asset
/// `flags` bitset, so it survives re-scans and analysis.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct FavoriteRequest {
    pub asset: AssetId,
    pub favorite: bool,
}

/// A user's free-text annotation on an asset (issue #81) — the "why" that filenames, tags, and
/// extracted metadata cannot carry ("client rejected this variant"; "needs a high-pass before use").
///
/// One editable note per asset, not a thread: it answers *"what should I know about this asset?"*,
/// which is a single durable statement. Time-ordered, authored discussion is a different feature.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    /// Exactly what the user typed. Stored verbatim — a client may *render* markdown, but nothing
    /// on the way in or out is allowed to rewrite it.
    pub body: String,
    /// Unix ms of the last edit.
    pub updated_at: i64,
    /// Who last edited it, as a loose identity string (account username, else the token identity).
    /// `None` for a single-user local library, which is the common case today.
    #[serde(default)]
    pub updated_by: Option<String>,
}

/// Set or clear an asset's note. An empty (or whitespace-only) `body` **clears** it: there is no
/// separate delete verb, because "select all, delete, blur" is how a user expects to remove text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteRequest {
    pub body: String,
}
