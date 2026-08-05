//! Query inputs: the search text, filters, facets, and sort that browse and search submit.

use crate::page::PageParams;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueryRequest {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default)]
    pub page: PageParams,
    #[serde(default)]
    pub include_facets: bool,
    /// Request an exact filtered total even on a continuation page. First pages include the exact
    /// total automatically when omitted; `false` lets internal fan-out/refetch callers suppress
    /// even that count, and `true` explicitly requests it on any page.
    #[serde(default)]
    pub include_total: Option<bool>,
    /// How the `text` query is matched (semantic-search M5). Defaults to `Lexical` so existing
    /// callers and stored queries are unchanged; `Hybrid`/`Semantic` widen results with embedding
    /// neighbours of the lexical hits.
    #[serde(default)]
    pub mode: SearchMode,
    /// Answer from the local catalog only — no federated fan-out. Set on every peer-bound call so
    /// a peer never re-fans-out to *its* peers (federation is one hop, never transitive — ADR 0009
    /// §5). Old peers that predate the field simply ignore it, which is the same thing.
    #[serde(default)]
    pub local_only: bool,
}

/// Text-search strategy (semantic-search M5). `Lexical` is the FTS/synonym path (M1–M3). `Hybrid`
/// keeps every lexical hit and *adds* embedding-nearest neighbours of those hits, reciprocal-rank
/// fused — so "ak47" also surfaces visually/geometrically similar props. `Semantic` ranks purely by
/// that embedding neighbourhood (lexical hits seed it). With no embeddings present both degrade to
/// `Lexical`. When the model-backed spaces land (M4) the same fusion simply gets better vectors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    #[default]
    Lexical,
    Hybrid,
    Semantic,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Filter {
    pub field: FacetField,
    pub op: FilterOp,
    pub value: FilterValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FacetField {
    MediaType,
    Format,
    Source,
    Tag,
    SizeBytes,
    License,
    UsageRight,
    // ── media-specific structured attributes ────────────────────────────────
    // Each maps to a typed column in a per-media attr table (`audio_attr`/`image_attr`/`model_attr`).
    // Numeric fields take Eq/Lt/Lte/Gt/Gte/Range; string fields Eq/In; boolean fields Eq(bool). These
    // back the Advanced Search dropdown/range controls — the tags-vs-attributes split (bounded,
    // extracted metadata belongs here, not in the open-vocabulary tag set).
    // Image (image_attr):
    Width,
    Height,
    ColorDepth,
    HasAlpha,
    ColorSpace,
    ImageClass,
    Tileability,
    TileClass,
    // Audio (audio_attr):
    Bpm,
    Duration,
    SampleRate,
    BitDepth,
    Channels,
    MusicalKey,
    Loudness,
    Brightness,
    Harmonicity,
    AudioClass,
    Codec,
    Container,
    // Model (model_attr):
    TriCount,
    VertexCount,
    MeshCount,
    MaterialCount,
    TextureCount,
    DependencyBytes,
    HasRig,
    HasAnimation,
    HasUv,
    ModelClass,
    // Video (video_attr). `Width`/`Height`/`Duration` above also match video — the columns mean
    // the same thing there, so those facets span both tables rather than being duplicated.
    Fps,
    Bitrate,
    HasAudio,
    VideoClass,
    // Document (document_attr):
    PageCount,
    WordCount,
    Author,
    DocumentClass,
    /// User-flagged favourite (issue #63). Presence of the filter means "favourites only".
    Favorite,
    /// Source-relative path prefix (issue #66) — scopes the browse to a folder subtree, **including
    /// everything below it**. The value is the prefix (e.g. `Environment/Rock/`); an empty prefix
    /// matches everything.
    Path,
    /// Source-relative folder, matched **exactly** (issue #66): assets whose immediate parent is
    /// this folder, excluding anything in a deeper subfolder. The value is the same prefix form
    /// [`Path`](Self::Path) takes (`Environment/Rock/`); an empty value means the source root, so
    /// `folder = ""` is the loose files at the top level rather than "everything".
    ///
    /// A separate field rather than an op on `Path` because both readings are legitimate scopes a
    /// user saves into a smart folder, and overloading `Eq` would silently re-scope every folder
    /// filter already stored.
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
    Range,
    Contains,
    Exists,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterValue {
    Str(String),
    Num(f64),
    Bool(bool),
    Range(f64, f64),
    List(Vec<FilterValue>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sort {
    pub field: SortField,
    pub dir: SortDir,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            field: SortField::Name,
            dir: SortDir::Asc,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortField {
    Name,
    Size,
    Scanned,
    Relevance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    Asc,
    Desc,
}
