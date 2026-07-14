//! The browse / search / inspect workspace (tech-spec 12 §B) — a three-region layout mirroring the
//! web client: a left rail (library stats + sources), a centre asset list, and a right inspector.
//!
//! ## Async bridge
//! `LibraryService` is async; egui's `update` is a synchronous per-frame call. So every service call
//! is `spawn`ed onto the background Tokio runtime, and its result is sent back over an `mpsc` channel
//! that `update` drains at the top of each frame — the frame never blocks on I/O (golden rule 5).
//! The worker calls `Context::request_repaint()` when it posts a result, so the UI wakes exactly when
//! there's something new to show rather than polling.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use eframe::egui;
use url::Url;

use crate::theme::colors;
use crate::ui::{self, icon};
use dam_frontend::{open_backend, Backend};

use dam_api::service::{AuthContext, LibraryService};
use dam_api::{
    AddSource, AnalyzeRequest, Asset, AssetId, AssetSummary, BlockEntry, Collection, CollectionId,
    CollectionKind, CollectionMembers, CollisionRule, ContentHash, ConvertReport, ConvertRequest,
    ConvertTarget, DupGroup, DupKind, DupRequest, EventTopic, ExportFormat, ExportReport,
    ExportRequest, FacetField, Filter, FilterOp, FilterValue, FolderEntry, FolderListing, JobState,
    LibraryEvent, LibraryStats, LicenseStatus, MediaAttributes, MediaType, NewCollection, Origin,
    Page, PageParams, PrefetchRequest, QueryRequest, RemoveAsset, RemoveSource, ReviewAction,
    ScanMode, ScanRequest, SearchMode, SimilarHit, SimilarRequest, Sort, SortDir, SortField,
    SourceId, SourceInfo, SourceKind, SourceOptions, SubscribeRequest, SuggestionReview,
    ThumbnailRegenRequest, UpdateCollection,
};
use futures::StreamExt;

/// A per-asset maintenance action fired from the inspector (mirrors the web context-menu actions).
enum AssetAction {
    /// Force a re-analysis (embeddings, tileability, auto-tags) — background job.
    Reanalyze(AssetId),
    /// Drop + rebuild the cached preview thumbnail from source.
    RegenThumb(AssetId),
    /// Open the convert modal for this image/audio asset.
    Convert(AssetId, MediaType),
    /// Open the export modal scoped to this single asset.
    Export(AssetId),
    /// Remove from the catalog (`bool` = also block its content hash). Carries the name for the
    /// confirmation dialog. Non-destructive: the file in the source is never touched (issue #21).
    Remove(AssetId, String, bool),
}

/// Modifier keys on a grid/list click that change the multi-selection (issue #10/#22).
#[derive(Clone, Copy, Default)]
struct ClickMods {
    /// ctrl/cmd → toggle one in the selection.
    toggle: bool,
    /// shift → extend a range from the anchor.
    range: bool,
}

/// The convert target-format options per media (mirrors the CLI/web); 3D can't transcode.
const CONVERT_FORMATS: &[(MediaType, &[&str])] = &[
    (
        MediaType::Image,
        &["png", "jpg", "webp", "bmp", "tga", "tiff"],
    ),
    (MediaType::Audio, &["wav"]),
];

/// A source-relative folder-tree node key: which source, and the source-relative prefix (trailing
/// slash, or empty for the source root).
type FolderKey = (SourceId, String);

/// Lazy-loaded state of one folder node's immediate children.
enum FolderState {
    Loading,
    Loaded(Vec<FolderEntry>),
    Failed,
}

/// A source-management action fired from the rail.
enum SourceAction {
    /// Add a local-filesystem source at this path (then scan it).
    AddLocal(String),
    /// Add a federated 3DAM peer (issue #39): endpoint + optional bearer token. Peers yield merged
    /// catalog rows, not bytes — no scan follows.
    AddFederated(String, Option<String>),
    Remove(SourceId),
    Rescan(SourceId),
}

/// Actions collected while rendering the (immutably-borrowed) folder tree, applied after the panels.
#[derive(Default)]
struct NavActions {
    /// Folder nodes whose expand/collapse toggle was clicked.
    toggle: Vec<FolderKey>,
    /// Folder nodes whose children need fetching.
    load: Vec<FolderKey>,
    /// A clicked scope target: source + optional path prefix (None = whole source).
    scope: Option<(SourceId, Option<String>)>,
    /// A source add/remove/rescan action.
    source: Option<SourceAction>,
    /// Set/clear the "confirm remove" state for a source (`Some(None)` clears it).
    set_confirm: Option<Option<SourceId>>,
    /// Toggle the add-source input's visibility.
    toggle_add: bool,
}

/// The sort presets offered in the toolbar — label + (field, dir), mirroring the web sort control.
/// "Best match" (relevance) is only meaningful with a text query, so it's filtered in at render.
// Labels stay ASCII — egui's default font has no ↑/↓ glyph (renders as a tofu box).
const SORTS: &[(&str, SortField, SortDir)] = &[
    ("Name A-Z", SortField::Name, SortDir::Asc),
    ("Name Z-A", SortField::Name, SortDir::Desc),
    ("Largest", SortField::Size, SortDir::Desc),
    ("Smallest", SortField::Size, SortDir::Asc),
    ("Newest", SortField::Scanned, SortDir::Desc),
    ("Oldest", SortField::Scanned, SortDir::Asc),
];

/// Index into `SORTS` for "newest first" (scan-time desc) — the "Recently added" shortcut's sort.
const NEWEST_SORT: usize = 4;

/// One Advanced-Search control. Mirrors the web `Control` union (AdvancedSearch.tsx) one-for-one —
/// keep in step with the `FacetField` variants and the analysis-pass classifier string literals.
enum AdvCtl {
    /// Dropdown of string values → one `Eq(Str)` filter. `(field, label, &[(value, label)])`.
    Enum(
        FacetField,
        &'static str,
        &'static [(&'static str, &'static str)],
    ),
    /// Dropdown of numeric values → one `Eq(Num)` filter. `(field, label, &[(value, label)])`.
    NumEnum(FacetField, &'static str, &'static [(f64, &'static str)]),
    /// Yes / No toggle → one `Eq(Bool)` filter. `(field, label)`.
    Bool(FacetField, &'static str),
    /// Min/max numeric range → a `Range` (both), `Gte` (min only), or `Lte` (max only) filter.
    /// `(field, label, unit, scale)` — `scale` maps display units to stored (e.g. s → ms is 1000).
    Range(FacetField, &'static str, &'static str, f64),
}

use AdvCtl::{Bool as B, Enum as E, NumEnum as N, Range as R};

const ADV_AUDIO: &[AdvCtl] = &[
    E(
        FacetField::AudioClass,
        "Type",
        &[
            ("one_shot", "One-shot"),
            ("loop", "Loop"),
            ("music", "Music"),
            ("sfx", "SFX"),
        ],
    ),
    E(
        FacetField::MusicalKey,
        "Key",
        &[
            ("c", "C"),
            ("c#", "C#"),
            ("d", "D"),
            ("d#", "D#"),
            ("e", "E"),
            ("f", "F"),
            ("f#", "F#"),
            ("g", "G"),
            ("g#", "G#"),
            ("a", "A"),
            ("a#", "A#"),
            ("b", "B"),
        ],
    ),
    R(FacetField::Bpm, "BPM", "BPM", 1.0),
    R(FacetField::Duration, "Duration", "s", 1000.0),
    N(
        FacetField::SampleRate,
        "Sample rate",
        &[
            (22050.0, "22.05 kHz"),
            (44100.0, "44.1 kHz"),
            (48000.0, "48 kHz"),
            (96000.0, "96 kHz"),
        ],
    ),
    N(
        FacetField::Channels,
        "Channels",
        &[(1.0, "Mono"), (2.0, "Stereo")],
    ),
    N(
        FacetField::BitDepth,
        "Bit depth",
        &[(16.0, "16-bit"), (24.0, "24-bit"), (32.0, "32-bit")],
    ),
    R(FacetField::Loudness, "Loudness", "LUFS", 1.0),
    R(FacetField::Brightness, "Brightness", "", 1.0),
    R(FacetField::Harmonicity, "Harmonicity", "", 1.0),
];

const ADV_IMAGE: &[AdvCtl] = &[
    E(
        FacetField::ImageClass,
        "Type",
        &[
            ("texture", "Texture"),
            ("sprite", "Sprite"),
            ("photo", "Photo"),
        ],
    ),
    E(
        FacetField::TileClass,
        "Tiling",
        &[
            ("seamless", "Seamless"),
            ("tiled", "Tiled"),
            ("non_tiling", "Non-tiling"),
        ],
    ),
    R(FacetField::Width, "Width", "px", 1.0),
    R(FacetField::Height, "Height", "px", 1.0),
    R(FacetField::Tileability, "Tileability", "", 1.0),
    B(FacetField::HasAlpha, "Alpha channel"),
];

const ADV_MODEL: &[AdvCtl] = &[
    E(
        FacetField::ModelClass,
        "Complexity",
        &[
            ("prop_lowpoly", "Low-poly"),
            ("prop", "Prop"),
            ("prop_highpoly", "High-poly"),
        ],
    ),
    R(FacetField::TriCount, "Triangles", "", 1.0),
    R(FacetField::VertexCount, "Vertices", "", 1.0),
    R(FacetField::MeshCount, "Meshes", "", 1.0),
    R(FacetField::MaterialCount, "Materials", "", 1.0),
    R(FacetField::TextureCount, "Textures", "", 1.0),
    B(FacetField::HasRig, "Rigged"),
    B(FacetField::HasAnimation, "Animated"),
    B(FacetField::HasUv, "UV mapped"),
];

/// The structured-attribute controls for a media type (mirrors the web `CATALOG`).
fn adv_catalog(m: MediaType) -> &'static [AdvCtl] {
    match m {
        MediaType::Audio => ADV_AUDIO,
        MediaType::Image => ADV_IMAGE,
        MediaType::Model => ADV_MODEL,
    }
}

/// The analysis-class quick facet for a media type: the class `FacetField` and its `(value, label)`
/// options (mirrors the web `CLASS_FACET`). Shown only when a single media type is selected.
fn class_facet(m: MediaType) -> (FacetField, &'static [(&'static str, &'static str)]) {
    match m {
        MediaType::Audio => (
            FacetField::AudioClass,
            &[
                ("one_shot", "One-shot"),
                ("loop", "Loop"),
                ("music", "Music"),
                ("sfx", "SFX"),
            ],
        ),
        MediaType::Image => (
            FacetField::ImageClass,
            &[
                ("texture", "Texture"),
                ("sprite", "Sprite"),
                ("photo", "Photo"),
            ],
        ),
        MediaType::Model => (
            FacetField::ModelClass,
            &[
                ("prop_lowpoly", "Low-poly"),
                ("prop", "Prop"),
                ("prop_highpoly", "High-poly"),
            ],
        ),
    }
}

/// Build a `Range`/`Gte`/`Lte` filter from raw min/max display strings (or `None` when both are
/// blank/invalid), applying `scale` to map display units to the stored units. Mirrors the web
/// `rangeFilter`.
fn range_filter(field: FacetField, min: &str, max: &str, scale: f64) -> Option<Filter> {
    let lo = min.trim().parse::<f64>().ok().map(|v| v * scale);
    let hi = max.trim().parse::<f64>().ok().map(|v| v * scale);
    match (lo, hi) {
        (Some(a), Some(b)) => Some(Filter {
            field,
            op: FilterOp::Range,
            value: FilterValue::Range(a, b),
        }),
        (Some(a), None) => Some(Filter {
            field,
            op: FilterOp::Gte,
            value: FilterValue::Num(a),
        }),
        (None, Some(b)) => Some(Filter {
            field,
            op: FilterOp::Lte,
            value: FilterValue::Num(b),
        }),
        (None, None) => None,
    }
}

/// Structural equality for `Filter` (which doesn't derive `PartialEq`; its parts do).
fn filters_eq(a: &Filter, b: &Filter) -> bool {
    a.field == b.field && a.op == b.op && a.value == b.value
}

/// Number of peak buckets in an inspector waveform (the drawn bar count).
const WAVEFORM_BUCKETS: usize = 240;

/// Decode audio bytes and reduce them to `WAVEFORM_BUCKETS` normalised (0..1) peak values — the
/// max absolute mono amplitude in each equal time slice. Mirrors the web waveform island's shape.
fn compute_peaks(bytes: &[u8]) -> Result<Vec<f32>, String> {
    use rodio::Source;
    let dec =
        rodio::Decoder::new(std::io::Cursor::new(bytes.to_vec())).map_err(|e| e.to_string())?;
    let channels = dec.channels().max(1) as usize;
    // Fold interleaved samples to mono absolute amplitude. Cap the sample count so a long track's
    // preview stays cheap (the peaks still span the whole file — this only limits resolution).
    const MAX_SAMPLES: usize = 6_000_000;
    let mut mono: Vec<f32> = Vec::new();
    let mut acc = 0f32;
    let mut c = 0usize;
    for s in dec {
        acc += (s as f32 / i16::MAX as f32).abs();
        c += 1;
        if c == channels {
            mono.push(acc / channels as f32);
            acc = 0.0;
            c = 0;
        }
        if mono.len() >= MAX_SAMPLES {
            break;
        }
    }
    if mono.is_empty() {
        return Err("no samples".into());
    }
    let n = mono.len();
    let mut peaks = vec![0f32; WAVEFORM_BUCKETS];
    for (i, &v) in mono.iter().enumerate() {
        let b = (i * WAVEFORM_BUCKETS / n).min(WAVEFORM_BUCKETS - 1);
        if v > peaks[b] {
            peaks[b] = v;
        }
    }
    // Normalise so the loudest slice fills the height.
    let max = peaks.iter().copied().fold(0f32, f32::max);
    if max > 0.0 {
        for p in &mut peaks {
            *p /= max;
        }
    }
    Ok(peaks)
}

/// The media-specific "detail" column value from `key_attrs` (mirrors the web `detailAttr`):
/// image dimensions, audio duration (+ analysis type), or model triangle count.
fn detail_attr(a: &AssetSummary) -> Option<String> {
    let k = &a.key_attrs;
    match a.media {
        MediaType::Image => k.get("dimensions").cloned(),
        MediaType::Audio => match (k.get("duration"), k.get("type")) {
            (Some(d), Some(t)) => Some(format!("{d} · {t}")),
            (Some(d), None) => Some(d.clone()),
            (None, Some(t)) => Some(t.clone()),
            (None, None) => None,
        },
        MediaType::Model => k.get("tris").map(|t| format!("{t} tris")),
    }
}

/// A short display label for a license status (matches the web sidebar labels).
fn license_label(status: LicenseStatus) -> &'static str {
    match status {
        LicenseStatus::Permissive => "Permissive",
        LicenseStatus::Attribution => "Attribution",
        LicenseStatus::Restricted => "Restricted",
        LicenseStatus::Unknown => "Unknown",
    }
}

/// Paint a mirrored waveform (accent bars around a centre line) from normalised peaks.
fn draw_waveform(ui: &mut egui::Ui, peaks: &[f32]) {
    // The one accent, from the active (web-matched) theme — see theme.rs / issue #68.
    let accent = ui.visuals().selection.stroke.color;
    let width = ui.available_width().min(300.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 56.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, crate::theme::VIEWER_BG); // mode-independent dark trough
    if peaks.is_empty() {
        return;
    }
    let mid = rect.center().y;
    let half = rect.height() / 2.0 - 3.0;
    let slot = rect.width() / peaks.len() as f32;
    let bar = (slot * 0.7).max(1.0);
    for (i, &p) in peaks.iter().enumerate() {
        let x = rect.left() + i as f32 * slot;
        let h = (p * half).max(0.5);
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x, mid - h), egui::pos2(x + bar, mid + h)),
            0.0,
            accent,
        );
    }
}

/// Format a range bound for display: drop the fraction when it's a whole number.
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// The license facet options (matches the web sidebar), as `(LicenseStatus, label)`.
const LICENSES: &[(LicenseStatus, &str)] = &[
    (LicenseStatus::Permissive, "Permissive"),
    (LicenseStatus::Attribution, "Attribution"),
    (LicenseStatus::Restricted, "Restricted"),
    (LicenseStatus::Unknown, "Unknown"),
];

/// The most assets to pull into the list at once. The GUI is a single-page browse for now (infinite
/// scroll / pagination is an owed parity item); a generous cap keeps a typical library one request.
const LIST_LIMIT: u32 = 500;

/// Thumbnail edge (px) requested from the engine — matches the web grid tile.
const THUMB_EDGE: u32 = 128;

/// Grid vs list browse (a subset of the web view toggle).
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Grid,
    List,
}

/// Decoded thumbnail pixels handed back from the worker; uploaded to a GPU texture on the UI thread.
struct ThumbPixels {
    size: [usize; 2],
    rgba: Vec<u8>,
}

/// Per-asset thumbnail state in the UI-thread cache.
enum Thumb {
    Loading,
    Ready(egui::TextureHandle),
    /// No raster thumbnail (audio / a 3D model without the render feature) — show the typed tile.
    None,
}

/// A result posted from the background runtime back to the UI thread. Variants differ in size
/// (a full `Asset` vs a small stats struct), but each is posted at most once per user action, so the
/// enum size is irrelevant here — not worth boxing every payload.
#[allow(clippy::large_enum_variant)]
enum Msg {
    Assets(Result<Page<AssetSummary>, String>),
    Detail(Result<Asset, String>),
    Sources(Result<Vec<SourceInfo>, String>),
    Stats(Result<LibraryStats, String>),
    Thumb(AssetId, Option<ThumbPixels>),
    Folders(FolderKey, Result<Vec<FolderEntry>, String>),
    /// An add-source finished — `Err` carries the engine's BadRequest message (e.g. a federated
    /// peer that failed its add-time handshake) so the rail surfaces it instead of dropping it.
    SourceAdded(Result<(), String>),
    /// A live change from the engine's event stream (scan/analyze/convert, source state, …).
    Event(LibraryEvent),
    Similar(AssetId, Result<Vec<SimilarHit>, String>),
    Collections(Result<Vec<Collection>, String>),
    Export(Result<ExportReport, String>),
    Duplicates(Result<Vec<DupGroup>, String>),
    /// Groups for the dedicated duplicates view (filterable by kind/media; distinct from the
    /// inspector's exact-only `duplicates` cache).
    DupView(Result<Vec<DupGroup>, String>),
    /// An asset removal finished (reload the browse + blocklist on success).
    AssetRemoved(Result<(), String>),
    /// The current blocklist entries.
    Blocklist(Result<Vec<BlockEntry>, String>),
    /// An unblock finished (reload the blocklist).
    Unblocked(Result<(), String>),
    Convert(Result<ConvertReport, String>),
    /// Raw bytes of an audio asset fetched for inspector playback.
    AudioBytes(AssetId, Result<Vec<u8>, String>),
    /// Normalised peak buckets (0..1) for an audio asset's inspector waveform.
    Waveform(AssetId, Result<Vec<f32>, String>),
    /// The DMSH preview-mesh blob for a model asset (interactive 3D viewer).
    ModelMesh(AssetId, Result<Vec<u8>, String>),
    /// A collection create/rename/delete/membership mutation finished (reload on success).
    CollectionMutated(Result<(), String>),
    /// A backend switch (Connect/Disconnect) finished building the new service (issue #70). On `Ok`
    /// the app rebinds `lib`, resets browse state, and re-subscribes to the event stream.
    BackendSwitched(Conn, Result<Arc<dyn LibraryService>, String>),
}

/// A collection CRUD / membership action gathered while rendering, applied after the panels.
enum CollectionAction {
    Create { name: String, smart: bool },
    Rename { id: CollectionId, name: String },
    Delete(CollectionId),
    AddMember { id: CollectionId, asset: AssetId },
    RemoveMember { id: CollectionId, asset: AssetId },
}

/// Which backend the GUI is currently bound to (issue #70, hosted mode). The embedded engine runs
/// in-process; a remote `3dam serve` is reached over HTTP/WS through the same `LibraryService` seam.
/// The endpoint is kept as a string for display and for rebuilding the client on a reconnect.
#[derive(Clone)]
pub struct Conn {
    /// Remote server URL, or `None` for the in-process embedded engine.
    pub endpoint: Option<String>,
    /// Bearer token presented to a token-gated server.
    pub token: Option<String>,
}

impl Conn {
    /// The in-process embedded engine — always local, never "offline".
    pub fn embedded() -> Self {
        Self {
            endpoint: None,
            token: None,
        }
    }

    /// A remote `3dam serve` endpoint (optionally token-gated).
    pub fn remote(endpoint: Url, token: Option<String>) -> Self {
        Self {
            endpoint: Some(endpoint.to_string()),
            token,
        }
    }

    fn is_remote(&self) -> bool {
        self.endpoint.is_some()
    }
}

/// Live reachability of the current backend, surfaced as a toolbar chip. The embedded engine is
/// always `Local`; a remote backend moves Connecting → Online, and flips to `Offline` when calls
/// start failing (the `ApiClient` event subscription reconnects with backoff, so it recovers).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnStatus {
    /// In-process embedded engine.
    Local,
    /// Remote backend just selected; first calls in flight.
    Connecting,
    /// Remote backend answering.
    Online,
    /// Remote backend unreachable (transient) — auto-recovers on the next successful call.
    Offline,
}

/// A compact `host[:port]` label for the connection chip (falls back to the raw string).
fn short_host(endpoint: &str) -> String {
    Url::parse(endpoint)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_else(|| endpoint.to_string())
}

pub struct DamGui {
    rt: Arc<tokio::runtime::Runtime>,
    lib: Arc<dyn LibraryService>,
    auth: AuthContext,
    /// The backend the GUI is bound to (embedded or a remote endpoint), for the status chip and the
    /// Connect dialog. `data_dir` is retained so "Disconnect" can rebuild the embedded engine.
    conn: Conn,
    data_dir: PathBuf,
    status: ConnStatus,
    /// A backend switch (Connect/Disconnect) is building the new service off-thread.
    switching: bool,
    /// The live-event pump task; aborted when the backend switches so the old client's reconnect
    /// loop doesn't linger against the previous server.
    event_task: Option<tokio::task::JoinHandle<()>>,
    /// Connect-dialog state: open flag, URL + token input buffers, and recently-used servers.
    connect_open: bool,
    connect_url: String,
    connect_token: String,
    recent_servers: Vec<String>,
    egui_ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    // ── view state ──
    search: String,
    media_filter: Option<MediaType>,
    /// Analysis-class quick facet (audio/image/model class), contextual to the active media type.
    class: Option<String>,
    license: Option<LicenseStatus>,
    favorites: bool,
    /// Theme preference (dark-first, DESIGN_GUIDELINES §4). `System` follows the OS; egui resolves it.
    theme_pref: egui::ThemePreference,
    /// The dark/light mode our web-matched visuals were last applied for (re-apply when it flips).
    theme_applied: Option<bool>,
    source_filter: Option<SourceId>,
    /// Folder scope (issue #66): a source-relative path prefix, paired with `source_filter`.
    path: Option<String>,
    /// Collection browse mode: when set, the Browser shows this collection's members instead of the
    /// faceted query (mutually exclusive with the facets, mirroring the web).
    collection: Option<CollectionId>,
    collections: Vec<Collection>,
    // ── collection CRUD modals ──
    /// The "new collection" modal: name buffer + whether to save the current search as a smart folder.
    collection_new_open: bool,
    collection_new_name: String,
    collection_new_smart: bool,
    /// When Some, the rename modal is open for this collection with the edited name buffer.
    collection_rename: Option<(CollectionId, String)>,
    /// Exact (byte-identical) duplicate groups, cached for the inspector's per-asset dup section.
    duplicates: Vec<DupGroup>,
    // ── dedicated duplicates view (issue #8) ──
    /// When set, the central panel shows the dedupe review (groups) instead of the browse.
    dup_view: bool,
    dup_kind: DupKind,
    dup_media: Option<MediaType>,
    dup_groups: Vec<DupGroup>,
    dup_loading: bool,
    // ── remove / blocklist (issue #21) ──
    /// Pending remove confirmation: (asset, name, also-block). None when no dialog is open.
    confirm_remove_asset: Option<(AssetId, String, bool)>,
    /// When set, the central panel shows the blocklist management surface.
    blocklist_view: bool,
    blocklist: Vec<BlockEntry>,
    blocklist_loading: bool,
    sort: usize, // index into SORTS
    mode: SearchMode,
    view: View,
    /// Which folder-tree nodes are expanded, and the lazily-fetched children of the open ones.
    expanded: HashSet<FolderKey>,
    folders: HashMap<FolderKey, FolderState>,
    assets: Vec<AssetSummary>,
    total: Option<u64>,
    /// Federation fan-out (issue #39): `Some(dropped peer names)` when the last result page came
    /// back `partial.complete == false` — the browse under-represents the federated library, a
    /// degradation rather than an error. `None` ⇒ the page was complete, no notice.
    dropped_peers: Option<Vec<String>>,
    loading: bool,
    selected: Option<AssetId>,
    /// Multi-selection for batch actions (issue #10) — distinct from the single inspector focus.
    selection: HashSet<AssetId>,
    /// The pivot for a shift-range extend.
    anchor: Option<AssetId>,
    detail: Option<Asset>,
    detail_loading: bool,
    sources: Vec<SourceInfo>,
    stats: Option<LibraryStats>,
    error: Option<String>,
    /// Thumbnail texture cache, keyed by asset. Presence of a key means "already requested", so it
    /// doubles as the de-dupe set for the lazy, visible-only loader.
    thumbs: HashMap<AssetId, Thumb>,
    // ── live updates (coalesced) ──
    /// Pending refreshes flagged by the event stream, flushed at most a few times a second so a big
    /// scan's event burst doesn't re-query per event.
    dirty_assets: bool,
    dirty_stats: bool,
    dirty_sources: bool,
    dirty_detail: bool,
    last_refresh: f64,
    // ── "find similar" (opt-in per asset) ──
    /// The asset the current `similar` results belong to (so they hide when selection changes).
    similar_for: Option<AssetId>,
    similar: Vec<SimilarHit>,
    similar_loading: bool,
    // ── advanced search (structured attr + tag filters, issue: tags→search) ──
    /// Structured attribute + tag filters, contextual to the active media type. Composes with the
    /// sidebar facets and text query in `build_query`. Mirrors the web `adv` `Filter[]`.
    adv: Vec<Filter>,
    adv_open: bool,
    /// Transient min/max text buffers for range controls, keyed by the control's stable buffer key.
    adv_range_bufs: std::collections::HashMap<&'static str, (String, String)>,
    adv_tag_input: String,
    // ── export manifest modal ──
    export_open: bool,
    export_format: ExportFormat,
    export_path: String,
    export_attribution: bool,
    /// When non-empty, the export modal exports this explicit selection instead of the current view.
    export_assets: Vec<AssetId>,
    /// Last export outcome (Ok message / Err message) shown in the modal.
    export_status: Option<Result<String, String>>,
    // ── source management ──
    add_open: bool,
    /// Which kind the add-source form is offering (LocalFs folder or a Federated peer, #39).
    add_kind: SourceKind,
    add_path: String,
    /// Federated-peer inputs: endpoint (`3dam://host:7878` or `http(s)://…`) + optional bearer token.
    add_endpoint: String,
    add_token: String,
    /// The source pending a remove confirmation (two-step to guard against accidental drops).
    confirm_remove: Option<SourceId>,
    // ── convert modal ──
    convert_open: bool,
    convert_asset: Option<(AssetId, MediaType)>,
    convert_format: String,
    convert_output: String,
    convert_status: Option<Result<String, String>>,
    // ── audio preview player ──
    /// The rodio output stream + handle, opened lazily on first play (kept alive so audio keeps
    /// flowing). `!Send`, but the app lives on the main thread.
    audio_stream: Option<rodio::OutputStream>,
    audio_handle: Option<rodio::OutputStreamHandle>,
    audio_sink: Option<rodio::Sink>,
    audio_for: Option<AssetId>,
    /// Cached waveform peaks (0..1) for the selected audio asset's inspector preview.
    waveform: Option<(AssetId, Vec<f32>)>,
    waveform_loading: bool,
    audio_error: Option<String>,
    /// Interactive 3D preview renderer (present only on the wgpu backend). `!Send`; UI-thread only.
    viewer3d: Option<crate::viewer3d::Viewer3d>,
    /// When set, the 3D preview fills the window in a fullscreen overlay (issue #65).
    viewer_fullscreen: bool,
}

impl DamGui {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: Arc<tokio::runtime::Runtime>,
        lib: Arc<dyn LibraryService>,
        auth: AuthContext,
        conn: Conn,
        data_dir: PathBuf,
    ) -> Self {
        // Dark-first preference (DESIGN_GUIDELINES §4); `update` applies the web-matched visuals
        // (issue #68) for whatever mode the preference resolves to.
        cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
        // Register Inter + Phosphor and the web type scale/spacing once, up front, so the shell never
        // renders a frame in egui's stock font at default metrics (issue #68).
        crate::theme::install_fonts(&cc.egui_ctx);
        crate::theme::install_style(&cc.egui_ctx);
        let (tx, rx) = std::sync::mpsc::channel();
        // A remote launch starts "Connecting" and turns Online on the first successful call; embedded
        // is always Local. Seed the recent-servers list with the launch endpoint if any.
        let status = if conn.is_remote() {
            ConnStatus::Connecting
        } else {
            ConnStatus::Local
        };
        let recent_servers = conn.endpoint.iter().cloned().collect();
        let connect_url = conn.endpoint.clone().unwrap_or_default();
        let connect_token = conn.token.clone().unwrap_or_default();
        let mut app = Self {
            rt,
            lib,
            auth,
            conn,
            data_dir,
            status,
            switching: false,
            event_task: None,
            connect_open: false,
            connect_url,
            connect_token,
            recent_servers,
            egui_ctx: cc.egui_ctx.clone(),
            tx,
            rx,
            search: String::new(),
            media_filter: None,
            class: None,
            license: None,
            favorites: false,
            theme_pref: egui::ThemePreference::Dark,
            theme_applied: None,
            source_filter: None,
            path: None,
            collection: None,
            collections: Vec::new(),
            collection_new_open: false,
            collection_new_name: String::new(),
            collection_new_smart: false,
            collection_rename: None,
            duplicates: Vec::new(),
            dup_view: false,
            dup_kind: DupKind::Exact,
            dup_media: None,
            dup_groups: Vec::new(),
            dup_loading: false,
            confirm_remove_asset: None,
            blocklist_view: false,
            blocklist: Vec::new(),
            blocklist_loading: false,
            sort: 0,
            mode: SearchMode::Lexical,
            view: View::Grid,
            expanded: HashSet::new(),
            folders: HashMap::new(),
            assets: Vec::new(),
            total: None,
            dropped_peers: None,
            loading: false,
            selected: None,
            selection: HashSet::new(),
            anchor: None,
            detail: None,
            detail_loading: false,
            sources: Vec::new(),
            stats: None,
            error: None,
            thumbs: HashMap::new(),
            dirty_assets: false,
            dirty_stats: false,
            dirty_sources: false,
            dirty_detail: false,
            last_refresh: 0.0,
            similar_for: None,
            similar: Vec::new(),
            similar_loading: false,
            adv: Vec::new(),
            adv_open: false,
            adv_range_bufs: std::collections::HashMap::new(),
            adv_tag_input: String::new(),
            export_open: false,
            export_format: ExportFormat::Json,
            export_path: default_export_path(),
            export_attribution: false,
            export_assets: Vec::new(),
            export_status: None,
            add_open: false,
            add_kind: SourceKind::LocalFs,
            add_path: String::new(),
            add_endpoint: String::new(),
            add_token: String::new(),
            confirm_remove: None,
            convert_open: false,
            convert_asset: None,
            convert_format: String::new(),
            convert_output: default_convert_dir(),
            convert_status: None,
            audio_stream: None,
            audio_handle: None,
            audio_sink: None,
            audio_for: None,
            waveform: None,
            waveform_loading: false,
            audio_error: None,
            viewer3d: cc
                .wgpu_render_state
                .as_ref()
                .map(crate::viewer3d::Viewer3d::new),
            viewer_fullscreen: false,
        };
        // Kick the initial loads against the freshly opened library.
        let egctx = cc.egui_ctx.clone();
        app.load_assets(&egctx);
        app.load_sources(&egctx);
        app.load_stats(&egctx);
        app.load_collections(&egctx);
        app.load_duplicates(&egctx);
        app.load_blocklist(&egctx);
        app.event_task = Some(app.spawn_events());
        app
    }

    /// Subscribe to the engine's event stream and forward each change to the UI thread (live
    /// updates). The events are coalesced into throttled refreshes in `update` — this task just
    /// pumps them across the channel and wakes the frame loop. The returned handle is retained so a
    /// backend switch can abort the old subscription (otherwise a remote client keeps trying to
    /// reconnect to the previous server).
    fn spawn_events(&self) -> tokio::task::JoinHandle<()> {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let req = SubscribeRequest {
                topics: vec![
                    EventTopic::Assets,
                    EventTopic::Sources,
                    EventTopic::Jobs,
                    EventTopic::Analysis,
                ],
            };
            let Ok(mut stream) = lib.subscribe(&auth, req).await else {
                return;
            };
            while let Some(ev) = stream.next().await {
                if tx.send(Msg::Event(ev)).is_err() {
                    break; // UI gone
                }
                egctx.request_repaint();
            }
        })
    }

    /// Begin switching to a different backend (a remote server or back to embedded). The new service
    /// is built off the UI thread — `open_backend` for a remote endpoint only constructs the HTTP
    /// client (no handshake), so this never blocks — and posts back a [`Msg::BackendSwitched`].
    fn switch_backend(&mut self, conn: Conn) {
        self.switching = true;
        self.status = if conn.is_remote() {
            ConnStatus::Connecting
        } else {
            ConnStatus::Local
        };
        let (tx, egctx, data_dir) = (
            self.tx.clone(),
            self.egui_ctx.clone(),
            self.data_dir.clone(),
        );
        let conn_for_task = conn.clone();
        self.rt.spawn(async move {
            let backend = match &conn_for_task.endpoint {
                Some(url) => match Url::parse(url) {
                    Ok(endpoint) => Backend::Connected {
                        endpoint,
                        token: conn_for_task.token.clone(),
                    },
                    Err(e) => {
                        let _ = tx.send(Msg::BackendSwitched(
                            conn_for_task.clone(),
                            Err(format!("invalid URL: {e}")),
                        ));
                        egctx.request_repaint();
                        return;
                    }
                },
                None => Backend::Embedded { data_dir },
            };
            let res = open_backend(backend)
                .await
                .map(Arc::from)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::BackendSwitched(conn_for_task, res));
            egctx.request_repaint();
        });
    }

    /// Rebind the app to a freshly-built backend: abort the old event pump, swap `lib`, reset all
    /// browse/inspect caches, then re-run the initial loads and re-subscribe (issue #70).
    fn rebind_backend(&mut self, conn: Conn, lib: Arc<dyn LibraryService>) {
        if let Some(task) = self.event_task.take() {
            task.abort();
        }
        self.lib = lib;
        self.status = if conn.is_remote() {
            ConnStatus::Online
        } else {
            ConnStatus::Local
        };
        // Remember the endpoint for quick reconnection (most-recent first, de-duplicated).
        if let Some(ep) = &conn.endpoint {
            self.recent_servers.retain(|s| s != ep);
            self.recent_servers.insert(0, ep.clone());
            self.recent_servers.truncate(8);
        }
        self.conn = conn;
        self.switching = false;
        self.error = None;

        // Drop everything tied to the old backend so nothing stale renders against the new one.
        self.assets.clear();
        self.total = None;
        self.dropped_peers = None;
        self.thumbs.clear();
        self.folders.clear();
        self.expanded.clear();
        self.detail = None;
        self.selected = None;
        self.selection.clear();
        self.anchor = None;
        self.collections.clear();
        self.duplicates.clear();
        self.dup_groups.clear();
        self.blocklist.clear();
        self.similar.clear();
        self.similar_for = None;
        self.waveform = None;
        self.audio_for = None;
        self.stats = None;

        let egctx = self.egui_ctx.clone();
        self.load_assets(&egctx);
        self.load_sources(&egctx);
        self.load_stats(&egctx);
        self.load_collections(&egctx);
        self.load_duplicates(&egctx);
        self.load_blocklist(&egctx);
        self.event_task = Some(self.spawn_events());
    }

    /// Note the outcome of a remote call so the status chip tracks reachability: a success means the
    /// server is answering (Online), an error means it's (transiently) unreachable (Offline). No-op
    /// for the embedded engine or while a switch is mid-flight.
    fn note_conn(&mut self, ok: bool) {
        if !self.conn.is_remote() || self.switching {
            return;
        }
        self.status = if ok {
            ConnStatus::Online
        } else {
            ConnStatus::Offline
        };
    }

    /// Colour + short label for the toolbar connection chip. Colours come from the active theme's
    /// tokens (web `ServerChip`/`ConnectionPill` semantics: local → dim, remote/online → accent,
    /// trouble → warn/danger) so the chip follows dark/light like everything else.
    fn status_chip(&self, dark: bool) -> (egui::Color32, String) {
        let c = colors(dark);
        match self.status {
            ConnStatus::Local => (c.fg_dim, "Local".to_string()),
            ConnStatus::Connecting => (c.warn, "Connecting…".to_string()),
            ConnStatus::Online => (
                c.accent,
                short_host(self.conn.endpoint.as_deref().unwrap_or("server")),
            ),
            ConnStatus::Offline => (c.danger, "Offline".to_string()),
        }
    }

    /// The Connect dialog (issue #70): choose the in-process embedded engine, pick a recent server,
    /// or enter a URL + token. Returns the [`Conn`] to switch to when the user commits — applied by
    /// the caller after the panels so it doesn't fight the panels' mutable self-borrow.
    fn connect_dialog(&mut self, ctx: &egui::Context) -> Option<Conn> {
        if !self.connect_open {
            return None;
        }
        let mut open = true;
        let mut close = false;
        let mut request: Option<Conn> = None;
        egui::Window::new("Connect to server")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label("Bind this client to a remote 3DAM server, or use the local library.");
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    let l = ui.label("Server URL");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.connect_url)
                            .hint_text("http://host:7878")
                            .desired_width(240.0),
                    )
                    .labelled_by(l.id);
                });
                ui.horizontal(|ui| {
                    let l = ui.label("Token");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.connect_token)
                            .password(true)
                            .hint_text("optional — required for token-gated servers")
                            .desired_width(240.0),
                    )
                    .labelled_by(l.id);
                });

                if !self.recent_servers.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Recent").weak());
                    for srv in self.recent_servers.clone() {
                        if ui.selectable_label(false, &srv).clicked() {
                            self.connect_url = srv;
                        }
                    }
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let can_connect = !self.connect_url.trim().is_empty();
                    if ui
                        .add_enabled(can_connect, egui::Button::new("Connect"))
                        .clicked()
                    {
                        let token = self.connect_token.trim();
                        request = Some(Conn {
                            endpoint: Some(self.connect_url.trim().to_string()),
                            token: (!token.is_empty()).then(|| token.to_string()),
                        });
                    }
                    // Only offer "Use local library" when currently remote.
                    if self.conn.is_remote() && ui.button("Use local library").clicked() {
                        request = Some(Conn::embedded());
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
                if self.switching {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Switching backend…");
                    });
                }
            });
        // Commit or Cancel closes the dialog; the window's own X button clears `open`.
        self.connect_open = open && !close && request.is_none();
        request
    }

    fn build_query(&self) -> QueryRequest {
        let mut filters = Vec::new();
        if let Some(media) = self.media_filter {
            filters.push(Filter {
                field: FacetField::MediaType,
                op: FilterOp::Eq,
                value: FilterValue::Str(media_value(media).to_string()),
            });
        }
        if let Some(license) = self.license {
            filters.push(Filter {
                field: FacetField::License,
                op: FilterOp::Eq,
                value: FilterValue::Str(license_value(license).to_string()),
            });
        }
        // Analysis class (contextual to media) — only meaningful with a single media type selected.
        if let (Some(m), Some(c)) = (self.media_filter, self.class.as_deref()) {
            filters.push(Filter {
                field: class_facet(m).0,
                op: FilterOp::Eq,
                value: FilterValue::Str(c.to_string()),
            });
        }
        if self.favorites {
            filters.push(Filter {
                field: FacetField::Favorite,
                op: FilterOp::Eq,
                value: FilterValue::Bool(true),
            });
        }
        if let Some(src) = self.source_filter {
            filters.push(Filter {
                field: FacetField::Source,
                op: FilterOp::Eq,
                value: FilterValue::Str(src.to_string()),
            });
        }
        // Folder scope (issue #66): a source-relative path prefix restricting the browse to a subtree.
        if let Some(p) = self.path.as_deref().filter(|p| !p.is_empty()) {
            filters.push(Filter {
                field: FacetField::Path,
                op: FilterOp::Eq,
                value: FilterValue::Str(p.to_string()),
            });
        }
        // Advanced Search: structured attribute + tag filters, AND-ed onto the query.
        filters.extend(self.adv.iter().cloned());
        let text = self.search.trim();
        let (_, field, dir) = SORTS[self.sort.min(SORTS.len() - 1)];
        QueryRequest {
            text: (!text.is_empty()).then(|| text.to_string()),
            filters,
            sort: Sort { field, dir },
            page: PageParams {
                after: None,
                limit: LIST_LIMIT,
            },
            include_facets: false,
            mode: self.mode,
            local_only: false,
        }
    }

    // ── Advanced Search (structured attr + tag filters) ──────────────────────────────────────────

    /// The "Advanced filters" popover window (mirrors web `AdvancedSearch.tsx`). Contextual to the
    /// active media type: its structured controls appear, plus a free tag filter. Returns true when
    /// the filter set changed this frame so the caller re-queries.
    fn advanced_modal(&mut self, ctx: &egui::Context) -> bool {
        if !self.adv_open {
            return false;
        }
        let mut changed = false;
        let mut open = true;
        egui::Window::new("Advanced filters")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .default_width(320.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(460.0)
                    .show(ui, |ui| {
                        if let Some(m) = self.media_filter {
                            egui::Grid::new("adv-grid")
                                .num_columns(2)
                                .spacing([8.0, 4.0])
                                .show(ui, |ui| {
                                    for ctl in adv_catalog(m) {
                                        changed |= self.adv_control(ui, ctl);
                                    }
                                });
                        } else {
                            ui.label(
                                egui::RichText::new(
                                    "Pick a media type (Audio, Images, 3D) to filter on its \
                                     properties — BPM, key, dimensions, triangle count, and more.",
                                )
                                .weak()
                                .italics(),
                            );
                        }
                        ui.separator();
                        changed |= self.adv_tags(ui);
                        if !self.adv.is_empty() {
                            ui.separator();
                            if ui.button("Clear all").clicked() {
                                self.adv.clear();
                                self.adv_range_bufs.clear();
                                changed = true;
                            }
                        }
                    });
            });
        if !open {
            self.adv_open = false;
        }
        changed
    }

    fn adv_control(&mut self, ui: &mut egui::Ui, ctl: &AdvCtl) -> bool {
        match ctl {
            AdvCtl::Enum(field, label, opts) => self.adv_enum(ui, *field, label, opts),
            AdvCtl::NumEnum(field, label, opts) => self.adv_num_enum(ui, *field, label, opts),
            AdvCtl::Bool(field, label) => self.adv_bool(ui, *field, label),
            AdvCtl::Range(field, label, unit, scale) => {
                self.adv_range(ui, *field, label, unit, *scale)
            }
        }
    }

    /// The current string value of `field`'s filter, if it holds one.
    fn adv_str(&self, field: FacetField) -> Option<String> {
        self.adv.iter().find(|f| f.field == field).and_then(|f| {
            if let FilterValue::Str(s) = &f.value {
                Some(s.clone())
            } else {
                None
            }
        })
    }

    fn adv_num(&self, field: FacetField) -> Option<f64> {
        self.adv.iter().find(|f| f.field == field).and_then(|f| {
            if let FilterValue::Num(n) = &f.value {
                Some(*n)
            } else {
                None
            }
        })
    }

    fn adv_bool_val(&self, field: FacetField) -> Option<bool> {
        self.adv.iter().find(|f| f.field == field).and_then(|f| {
            if let FilterValue::Bool(b) = &f.value {
                Some(*b)
            } else {
                None
            }
        })
    }

    /// Remove the filter targeting `field`; returns true if one was present.
    fn adv_remove(&mut self, field: FacetField) -> bool {
        let before = self.adv.len();
        self.adv.retain(|f| f.field != field);
        self.adv.len() != before
    }

    /// Replace (single) the filter targeting `field`.
    fn adv_replace(&mut self, field: FacetField, f: Filter) -> bool {
        self.adv.retain(|x| x.field != field);
        self.adv.push(f);
        true
    }

    fn adv_enum(
        &mut self,
        ui: &mut egui::Ui,
        field: FacetField,
        label: &str,
        opts: &[(&str, &str)],
    ) -> bool {
        ui.label(label);
        let cur = self.adv_str(field);
        let sel = cur
            .as_deref()
            .and_then(|c| opts.iter().find(|(v, _)| *v == c).map(|(_, l)| *l))
            .unwrap_or("Any");
        let mut pick: Option<Option<String>> = None;
        let combo = egui::ComboBox::from_id_salt(("adve", label))
            .selected_text(sel)
            .width(150.0)
            .show_ui(ui, |ui| {
                if ui.selectable_label(cur.is_none(), "Any").clicked() {
                    pick = Some(None);
                }
                for (v, l) in opts {
                    if ui
                        .selectable_label(cur.as_deref() == Some(*v), *l)
                        .clicked()
                    {
                        pick = Some(Some((*v).to_string()));
                    }
                }
            });
        ui::access_combo(&combo.response, label, sel);
        ui.end_row();
        match pick {
            Some(None) => self.adv_remove(field),
            Some(Some(v)) => self.adv_replace(
                field,
                Filter {
                    field,
                    op: FilterOp::Eq,
                    value: FilterValue::Str(v),
                },
            ),
            None => false,
        }
    }

    fn adv_num_enum(
        &mut self,
        ui: &mut egui::Ui,
        field: FacetField,
        label: &str,
        opts: &[(f64, &str)],
    ) -> bool {
        ui.label(label);
        let cur = self.adv_num(field);
        let sel = cur
            .and_then(|c| opts.iter().find(|(v, _)| *v == c).map(|(_, l)| *l))
            .unwrap_or("Any");
        let mut pick: Option<Option<f64>> = None;
        let combo = egui::ComboBox::from_id_salt(("advn", label))
            .selected_text(sel)
            .width(150.0)
            .show_ui(ui, |ui| {
                if ui.selectable_label(cur.is_none(), "Any").clicked() {
                    pick = Some(None);
                }
                for (v, l) in opts {
                    if ui.selectable_label(cur == Some(*v), *l).clicked() {
                        pick = Some(Some(*v));
                    }
                }
            });
        ui::access_combo(&combo.response, label, sel);
        ui.end_row();
        match pick {
            Some(None) => self.adv_remove(field),
            Some(Some(v)) => self.adv_replace(
                field,
                Filter {
                    field,
                    op: FilterOp::Eq,
                    value: FilterValue::Num(v),
                },
            ),
            None => false,
        }
    }

    fn adv_bool(&mut self, ui: &mut egui::Ui, field: FacetField, label: &str) -> bool {
        ui.label(label);
        let cur = self.adv_bool_val(field);
        let sel = match cur {
            None => "Any",
            Some(true) => "Yes",
            Some(false) => "No",
        };
        let mut pick: Option<Option<bool>> = None;
        let combo = egui::ComboBox::from_id_salt(("advb", label))
            .selected_text(sel)
            .width(150.0)
            .show_ui(ui, |ui| {
                if ui.selectable_label(cur.is_none(), "Any").clicked() {
                    pick = Some(None);
                }
                if ui.selectable_label(cur == Some(true), "Yes").clicked() {
                    pick = Some(Some(true));
                }
                if ui.selectable_label(cur == Some(false), "No").clicked() {
                    pick = Some(Some(false));
                }
            });
        ui::access_combo(&combo.response, label, sel);
        ui.end_row();
        match pick {
            Some(None) => self.adv_remove(field),
            Some(Some(b)) => self.adv_replace(
                field,
                Filter {
                    field,
                    op: FilterOp::Eq,
                    value: FilterValue::Bool(b),
                },
            ),
            None => false,
        }
    }

    fn adv_range(
        &mut self,
        ui: &mut egui::Ui,
        field: FacetField,
        label: &'static str,
        unit: &str,
        scale: f64,
    ) -> bool {
        // Seed the transient min/max buffers from the current filter the first time this control shows.
        if !self.adv_range_bufs.contains_key(label) {
            let seeded = self.adv_range_read(field, scale);
            self.adv_range_bufs.insert(label, seeded);
        }
        let (mut lo, mut hi) = self.adv_range_bufs.get(label).cloned().unwrap_or_default();

        let range_label = ui.label(if unit.is_empty() {
            label.to_string()
        } else {
            format!("{label} ({unit})")
        });
        // Commit when either box loses focus (tab/click away/Enter) — the standard egui pattern.
        let mut commit = false;
        ui.horizontal(|ui| {
            let r1 = ui
                .add(
                    egui::TextEdit::singleline(&mut lo)
                        .desired_width(56.0)
                        .hint_text("min"),
                )
                .labelled_by(range_label.id);
            ui.label("–");
            let r2 = ui
                .add(
                    egui::TextEdit::singleline(&mut hi)
                        .desired_width(56.0)
                        .hint_text("max"),
                )
                .labelled_by(range_label.id);
            commit = r1.lost_focus() || r2.lost_focus();
        });
        ui.end_row();
        self.adv_range_bufs.insert(label, (lo.clone(), hi.clone()));
        if !commit {
            return false;
        }
        // Only re-query when the resulting filter actually differs from the current one — tabbing
        // through the boxes without edits shouldn't trigger a browse.
        let new = range_filter(field, &lo, &hi, scale);
        let cur = self.adv.iter().find(|f| f.field == field).cloned();
        match (new, cur) {
            (Some(n), Some(c)) if filters_eq(&n, &c) => false,
            (None, None) => false,
            (Some(n), _) => self.adv_replace(field, n),
            (None, Some(_)) => self.adv_remove(field),
        }
    }

    /// Read `field`'s current range filter back into display-unit `(min, max)` strings.
    fn adv_range_read(&self, field: FacetField, scale: f64) -> (String, String) {
        match self.adv.iter().find(|f| f.field == field) {
            Some(f) => match (&f.op, &f.value) {
                (FilterOp::Range, FilterValue::Range(a, b)) => {
                    (fmt_num(a / scale), fmt_num(b / scale))
                }
                (FilterOp::Gte, FilterValue::Num(a)) => (fmt_num(a / scale), String::new()),
                (FilterOp::Lte, FilterValue::Num(b)) => (String::new(), fmt_num(b / scale)),
                _ => (String::new(), String::new()),
            },
            None => (String::new(), String::new()),
        }
    }

    /// Free tag filter: each accepted tag AND-s a `Tag` equality onto the query (tags power search
    /// now that they've left the sidebar). Returns true if a tag was added/removed.
    fn adv_tags(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        let tags_label = ui.label(egui::RichText::new("Tags").weak());
        let mut add = false;
        ui.horizontal(|ui| {
            let r = ui
                .add(
                    egui::TextEdit::singleline(&mut self.adv_tag_input)
                        .hint_text("Add a tag filter, then Enter")
                        .desired_width(200.0),
                )
                .labelled_by(tags_label.id);
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                add = true;
            }
            if ui.button("Add").clicked() {
                add = true;
            }
        });
        if add {
            let n = self.adv_tag_input.trim().to_string();
            let dup = self.adv.iter().any(|f| {
                f.field == FacetField::Tag
                    && matches!(&f.value, FilterValue::Str(s) if s.eq_ignore_ascii_case(&n))
            });
            if !n.is_empty() && !dup {
                self.adv.push(Filter {
                    field: FacetField::Tag,
                    op: FilterOp::Eq,
                    value: FilterValue::Str(n),
                });
                changed = true;
            }
            self.adv_tag_input.clear();
        }
        let tags: Vec<String> = self
            .adv
            .iter()
            .filter(|f| f.field == FacetField::Tag)
            .filter_map(|f| {
                if let FilterValue::Str(s) = &f.value {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect();
        let mut remove: Option<String> = None;
        if !tags.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for t in &tags {
                    if ui.button(format!("{t} ×")).clicked() {
                        remove = Some(t.clone());
                    }
                }
            });
        }
        if let Some(t) = remove {
            self.adv.retain(|f| {
                !(f.field == FacetField::Tag && matches!(&f.value, FilterValue::Str(s) if *s == t))
            });
            changed = true;
        }
        changed
    }

    /// Run the current browse against the engine, off-thread: a collection's members when in
    /// collection mode, else the faceted query.
    fn load_assets(&mut self, egctx: &egui::Context) {
        self.loading = true;
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            egctx.clone(),
        );
        let collection = self.collection;
        let req = self.build_query();
        let page = PageParams {
            after: None,
            limit: LIST_LIMIT,
        };
        self.rt.spawn(async move {
            let r = match collection {
                Some(id) => lib.collection_assets(&auth, &id, page).await,
                None => lib.query(&auth, req).await,
            }
            .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Assets(r));
            egctx.request_repaint();
        });
    }

    fn load_collections(&self, egctx: &egui::Context) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            egctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib.list_collections(&auth).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Collections(r));
            egctx.request_repaint();
        });
    }

    /// Apply a collection CRUD / membership action off-thread; success posts `CollectionMutated(Ok)`
    /// which triggers a reload of the list, the browse view, and the inspected asset's memberships.
    fn collection_action(&self, action: CollectionAction) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        // A smart folder saves the current faceted browse as its live query.
        let smart_query = self.build_query();
        self.rt.spawn(async move {
            let r: Result<(), String> = async {
                match action {
                    CollectionAction::Create { name, smart } => {
                        let (kind, query) = if smart {
                            (CollectionKind::Smart, Some(smart_query))
                        } else {
                            (CollectionKind::Manual, None)
                        };
                        lib.create_collection(&auth, NewCollection { name, kind, query })
                            .await
                            .map(|_| ())
                    }
                    CollectionAction::Rename { id, name } => {
                        lib.update_collection(
                            &auth,
                            &id,
                            UpdateCollection {
                                name: Some(name),
                                query: None,
                            },
                        )
                        .await
                    }
                    CollectionAction::Delete(id) => lib.delete_collection(&auth, &id).await,
                    CollectionAction::AddMember { id, asset } => {
                        lib.modify_collection_members(
                            &auth,
                            &id,
                            CollectionMembers {
                                add: vec![asset],
                                remove: vec![],
                            },
                        )
                        .await
                    }
                    CollectionAction::RemoveMember { id, asset } => {
                        lib.modify_collection_members(
                            &auth,
                            &id,
                            CollectionMembers {
                                add: vec![],
                                remove: vec![asset],
                            },
                        )
                        .await
                    }
                }
                .map_err(|e| e.to_string())
            }
            .await;
            let _ = tx.send(Msg::CollectionMutated(r));
            egctx.request_repaint();
        });
    }

    /// The create + rename collection modals. Kept together; each fires a `collection_action`
    /// (create/rename) on confirm and closes.
    fn collection_modals(&mut self, ctx: &egui::Context) {
        // ── new collection ──
        if self.collection_new_open {
            let mut open = true;
            let mut submit = false;
            let mut cancel = false;
            egui::Window::new("New collection")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    let l = ui.label("Name");
                    let resp = ui
                        .add(
                            egui::TextEdit::singleline(&mut self.collection_new_name)
                                .hint_text("Collection name")
                                .desired_width(220.0),
                        )
                        .labelled_by(l.id);
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = true;
                    }
                    ui.checkbox(
                        &mut self.collection_new_smart,
                        "Smart folder (save current search)",
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let named = !self.collection_new_name.trim().is_empty();
                        if ui.add_enabled(named, egui::Button::new("Create")).clicked() {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            if submit && !self.collection_new_name.trim().is_empty() {
                self.collection_action(CollectionAction::Create {
                    name: self.collection_new_name.trim().to_string(),
                    smart: self.collection_new_smart,
                });
                self.collection_new_open = false;
            } else if cancel || !open {
                self.collection_new_open = false;
            }
        }

        // ── rename ── the name buffer is edited *in place* in `collection_rename` (like the create
        // modal's persistent field) — reconstructing it each frame fought egui's text-cursor state.
        let mut open = true;
        let mut submit = false;
        let mut cancel = false;
        if let Some((_id, name)) = &mut self.collection_rename {
            egui::Window::new("Rename collection")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("Collection name")
                            .desired_width(220.0),
                    );
                    ui::access_edit_label(&resp, "Collection name");
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = true;
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let named = !name.trim().is_empty();
                        if ui.add_enabled(named, egui::Button::new("Save")).clicked() {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
        }
        if self.collection_rename.is_some() && (submit || cancel || !open) {
            let (id, name) = self.collection_rename.take().unwrap();
            if submit && !name.trim().is_empty() {
                self.collection_action(CollectionAction::Rename {
                    id,
                    name: name.trim().to_string(),
                });
            }
        }
    }

    /// Load the exact (byte-identical) duplicate groups, cached for the inspector's dup section.
    fn load_duplicates(&self, egctx: &egui::Context) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            egctx.clone(),
        );
        self.rt.spawn(async move {
            let req = DupRequest {
                kind: DupKind::Exact,
                media: None,
                limit: 10_000,
            };
            let r = lib
                .list_duplicates(&auth, req)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Duplicates(r));
            egctx.request_repaint();
        });
    }

    /// Load groups for the dedicated duplicates view, honouring the view's kind/media filters.
    fn load_dup_view(&self, egctx: &egui::Context) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            egctx.clone(),
        );
        let req = DupRequest {
            kind: self.dup_kind,
            media: self.dup_media,
            limit: 10_000,
        };
        self.rt.spawn(async move {
            let r = lib
                .list_duplicates(&auth, req)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::DupView(r));
            egctx.request_repaint();
        });
    }

    /// Remove an asset from the catalog off-thread (optionally blocking its hash). Non-destructive:
    /// the source file is never touched. Posts `AssetRemoved` (reload on success).
    fn remove_asset(&self, id: AssetId, block: bool) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib
                .remove_asset(&auth, &id, RemoveAsset { block })
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::AssetRemoved(r));
            egctx.request_repaint();
        });
    }

    /// Load the blocked-content-hash list for the blocklist view.
    fn load_blocklist(&self, egctx: &egui::Context) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            egctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib.list_blocklist(&auth).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Blocklist(r));
            egctx.request_repaint();
        });
    }

    /// Lift a block so a future scan can re-import the content. Posts `Unblocked` (reload on success).
    fn unblock(&self, hash: ContentHash) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib.unblock(&auth, &hash).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Unblocked(r));
            egctx.request_repaint();
        });
    }

    /// The remove-confirmation dialog (issue #21). Stresses that the file is untouched; the "+ block"
    /// variant also blocklists the content hash. Fires `remove_asset` on confirm.
    fn confirm_remove_modal(&mut self, ctx: &egui::Context) {
        let Some((id, name, block)) = self.confirm_remove_asset.clone() else {
            return;
        };
        let mut open = true;
        let mut confirmed = false;
        let mut cancel = false;
        egui::Window::new("Remove asset")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Remove \"{name}\" from the catalog?"));
                ui.label(
                    egui::RichText::new(
                        "The file in its source is never touched — only the catalog entry is dropped.",
                    )
                    .small()
                    .weak(),
                );
                if block {
                    ui.label(
                        egui::RichText::new(
                            "Its content hash is blocked so a rescan can't re-import it.",
                        )
                        .small()
                        .weak(),
                    );
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let label = if block { "Remove + block" } else { "Remove" };
                    if ui.button(label).clicked() {
                        confirmed = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if confirmed {
            self.remove_asset(id, block);
            self.confirm_remove_asset = None;
        } else if cancel || !open {
            self.confirm_remove_asset = None;
        }
    }

    fn load_detail(&mut self, id: AssetId, egctx: &egui::Context) {
        self.detail = None;
        self.detail_loading = true;
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            egctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib.get_asset(&auth, &id).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Detail(r));
            egctx.request_repaint();
        });
    }

    /// Apply a grid/list click to the multi-selection + inspector focus (issue #10): ctrl/cmd toggles
    /// one, shift extends a range from the anchor within the current order, plain click single-selects.
    /// The clicked asset always becomes the inspector focus.
    fn select_asset(&mut self, id: AssetId, mods: ClickMods, ctx: &egui::Context) {
        if mods.toggle {
            if !self.selection.remove(&id) {
                self.selection.insert(id);
            }
            self.anchor = Some(id);
        } else if mods.range {
            let order: Vec<AssetId> = self.assets.iter().map(|a| a.id).collect();
            let bi = order.iter().position(|x| *x == id);
            let ai = self
                .anchor
                .and_then(|an| order.iter().position(|x| *x == an));
            if let (Some(ai), Some(bi)) = (ai, bi) {
                let (lo, hi) = if ai <= bi { (ai, bi) } else { (bi, ai) };
                self.selection = order[lo..=hi].iter().copied().collect();
            } else {
                self.selection.clear();
                self.selection.insert(id);
                self.anchor = Some(id);
            }
        } else {
            self.selection.clear();
            self.selection.insert(id);
            self.anchor = Some(id);
        }
        self.selected = Some(id);
        self.load_detail(id, ctx);
        let mut fetch = false;
        self.thumbs.entry(id).or_insert_with(|| {
            fetch = true;
            Thumb::Loading
        });
        if fetch {
            self.load_thumb(id);
        }
    }

    /// Re-fetch the selected asset's detail without blanking the current view — used by live updates
    /// so an in-place refresh doesn't flicker the inspector (unlike `load_detail`, which shows a
    /// loading state for an explicit selection).
    fn reload_detail(&self, id: AssetId) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib.get_asset(&auth, &id).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Detail(r));
            egctx.request_repaint();
        });
    }

    /// Export a manifest of the current browse view (a collection when in collection mode, else the
    /// faceted query) — the same non-destructive export the CLI/web offer.
    fn run_export(&self) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        // An explicit selection (batch export) takes precedence over the current view.
        let selection = !self.export_assets.is_empty();
        let req = ExportRequest {
            assets: self.export_assets.clone(),
            collection: if selection { None } else { self.collection },
            query: if selection || self.collection.is_some() {
                None
            } else {
                Some(self.build_query())
            },
            format: self.export_format,
            output: self.export_path.clone(),
            attribution_only: self.export_attribution,
        };
        self.rt.spawn(async move {
            let r = lib.export(&auth, req).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Export(r));
            egctx.request_repaint();
        });
    }

    /// Fetch a model's DMSH preview-mesh blob off-thread for the interactive 3D viewer.
    fn load_model_mesh(&self, id: AssetId) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib
                .read_model_preview(&auth, &id)
                .await
                .map(|c| c.bytes)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::ModelMesh(id, r));
            egctx.request_repaint();
        });
    }

    /// Fetch an audio asset's bytes off-thread for inspector playback.
    fn load_audio(&self, id: AssetId) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib
                .read_content(&auth, &id)
                .await
                .map(|c| c.bytes)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::AudioBytes(id, r));
            egctx.request_repaint();
        });
    }

    /// Fetch + decode an audio asset and reduce it to normalised peak buckets for the inspector
    /// waveform. Decode is CPU work → `spawn_blocking` (golden rule 5); the peaks post over a `Msg`.
    fn load_waveform(&self, id: AssetId) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let r = async {
                let bytes = lib
                    .read_content(&auth, &id)
                    .await
                    .map_err(|e| e.to_string())?
                    .bytes;
                tokio::task::spawn_blocking(move || compute_peaks(&bytes))
                    .await
                    .map_err(|e| e.to_string())?
            }
            .await;
            let _ = tx.send(Msg::Waveform(id, r));
            egctx.request_repaint();
        });
    }

    /// Start playing decoded audio bytes on the (lazily opened) output stream.
    fn play_audio(&mut self, id: AssetId, bytes: Vec<u8>) {
        self.stop_audio();
        if self.audio_handle.is_none() {
            match rodio::OutputStream::try_default() {
                Ok((stream, handle)) => {
                    self.audio_stream = Some(stream);
                    self.audio_handle = Some(handle);
                }
                Err(e) => {
                    self.audio_error = Some(format!("no audio device: {e}"));
                    return;
                }
            }
        }
        let Some(handle) = &self.audio_handle else {
            return;
        };
        match (
            rodio::Sink::try_new(handle),
            rodio::Decoder::new(std::io::Cursor::new(bytes)),
        ) {
            (Ok(sink), Ok(source)) => {
                sink.append(source);
                sink.play();
                self.audio_sink = Some(sink);
                self.audio_for = Some(id);
                self.audio_error = None;
            }
            _ => self.audio_error = Some("couldn't decode this audio".to_string()),
        }
    }

    fn stop_audio(&mut self) {
        if let Some(sink) = self.audio_sink.take() {
            sink.stop();
        }
        self.audio_for = None;
    }

    /// Convert one image/audio asset to another format (non-destructive: writes to `output_dir`,
    /// never into a source; `Suffix` collision policy disambiguates rather than overwrites).
    fn run_convert(&self, id: AssetId, media: MediaType, format: String, output_dir: String) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let target = match media {
                MediaType::Audio => ConvertTarget::Audio { format },
                _ => ConvertTarget::Image {
                    format,
                    max_edge: None,
                    quality: None,
                },
            };
            let req = ConvertRequest {
                inputs: vec![id],
                target,
                output_dir,
                dry_run: false,
                on_collision: CollisionRule::Suffix,
            };
            let r = lib.convert(&auth, req).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Convert(r));
            egctx.request_repaint();
        });
    }

    /// Rank an asset's nearest neighbours by embedding cosine (tech-spec 05 §3) — opt-in from the
    /// inspector. Only analyzed assets have a vector; the caller gates on that.
    fn find_similar_asset(&self, id: AssetId) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let req = SimilarRequest {
                asset: id,
                k: 12,
                filters: Vec::new(),
                local_only: false,
            };
            let r = lib
                .find_similar(&auth, req)
                .await
                .map(|p| p.items)
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Similar(id, r));
            egctx.request_repaint();
        });
    }

    /// Reject / restore an auto-tag (reject-only lifecycle), then re-fetch the asset so the inspector
    /// reflects the new tag state. Tag changes also affect search, but the grid refreshes on its next
    /// query — keeping this to a detail refresh avoids a jarring re-sort under the user.
    fn review_tag(&self, id: AssetId, tag: String, action: ReviewAction) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let req = SuggestionReview {
                asset: id,
                tag,
                action,
            };
            if lib.review_suggestion(&auth, req).await.is_ok() {
                let r = lib.get_asset(&auth, &id).await.map_err(|e| e.to_string());
                let _ = tx.send(Msg::Detail(r));
            }
            egctx.request_repaint();
        });
    }

    /// Submit a forced re-analysis of one asset (fire-and-forget background job). Results surface on
    /// the next detail fetch — until live event updates land (owed parity), re-select to refresh.
    fn submit_analyze_asset(&self, id: AssetId) {
        self.submit_analyze_batch(vec![id], true);
    }

    /// Submit an analysis pass over a set of assets (batch action). `force` re-runs up-to-date ones.
    fn submit_analyze_batch(&self, assets: Vec<AssetId>, force: bool) {
        let (lib, auth) = (self.lib.clone(), self.auth.clone());
        self.rt.spawn(async move {
            let _ = lib
                .submit_analyze(&auth, AnalyzeRequest { assets, force })
                .await;
        });
    }

    /// Drop + rebuild one asset's cached thumbnail, then re-read it so the preview refreshes in place.
    /// Marks the cache entry `Loading` first so the lazy grid loader doesn't race a stale re-request.
    fn regen_thumb(&mut self, id: AssetId) {
        self.thumbs.insert(id, Thumb::Loading);
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let _ = lib
                .regenerate_thumbnails(&auth, ThumbnailRegenRequest { assets: vec![id] })
                .await;
            let pixels = match lib.read_thumbnail(&auth, &id, THUMB_EDGE).await {
                Ok(content) => image::load_from_memory(&content.bytes).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    ThumbPixels {
                        size,
                        rgba: rgba.into_raw(),
                    }
                }),
                Err(_) => None,
            };
            let _ = tx.send(Msg::Thumb(id, pixels));
            egctx.request_repaint();
        });
    }

    fn load_sources(&mut self, egctx: &egui::Context) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            egctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib.list_sources(&auth).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Sources(r));
            egctx.request_repaint();
        });
    }

    fn load_stats(&mut self, egctx: &egui::Context) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            egctx.clone(),
        );
        self.rt.spawn(async move {
            let r = lib.library_stats(&auth).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Stats(r));
            egctx.request_repaint();
        });
    }

    /// Add a local-filesystem source, then kick a full scan so it ingests. Source/asset events from
    /// the scan refresh the rail + grid live. The add outcome is posted back so a rejected path
    /// surfaces in the rail rather than vanishing.
    fn add_local_source(&self, path: String) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let req = AddSource {
                kind: SourceKind::LocalFs,
                uri: path,
                name: None,
                options: Default::default(),
            };
            let r = match lib.add_source(&auth, req).await {
                Ok(id) => {
                    let _ = lib
                        .submit_scan(
                            &auth,
                            ScanRequest {
                                sources: vec![id],
                                mode: ScanMode::Full,
                            },
                        )
                        .await;
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(Msg::SourceAdded(r));
            egctx.request_repaint();
        });
    }

    /// Register a federated 3DAM peer (issue #39). No scan follows — peers contribute merged
    /// catalog rows at query time, not bytes. The engine validates the endpoint at add time
    /// (advertise handshake + protocol check), so a typo'd endpoint or a flag-off peer comes back
    /// as a clear `Err` here and is surfaced in the rail.
    fn add_federated_source(&self, endpoint: String, token: Option<String>) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let req = AddSource {
                kind: SourceKind::Federated,
                uri: endpoint,
                name: None,
                options: SourceOptions {
                    // The peer's bearer token rides in `password`, like SFTP/SMB credentials —
                    // stored in the source's connection blob, never returned to clients.
                    password: token,
                    ..Default::default()
                },
            };
            let r = lib.add_source(&auth, req).await.map(|_| ());
            let _ = tx.send(Msg::SourceAdded(r.map_err(|e| e.to_string())));
            egctx.request_repaint();
        });
    }

    fn remove_source_svc(&self, id: SourceId) {
        let (lib, auth) = (self.lib.clone(), self.auth.clone());
        self.rt.spawn(async move {
            let _ = lib
                .remove_source(
                    &auth,
                    &id,
                    RemoveSource {
                        keep_metadata: false,
                    },
                )
                .await;
        });
    }

    fn rescan_source(&self, id: SourceId) {
        let (lib, auth) = (self.lib.clone(), self.auth.clone());
        self.rt.spawn(async move {
            let _ = lib
                .submit_scan(
                    &auth,
                    ScanRequest {
                        sources: vec![id],
                        mode: ScanMode::Delta,
                    },
                )
                .await;
        });
    }

    /// Fetch the immediate subfolders under one folder-tree node, off-thread (issue #66).
    fn load_folders(&self, key: FolderKey) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        let (source, prefix) = key.clone();
        self.rt.spawn(async move {
            let req = FolderListing { source, prefix };
            let r = lib
                .list_folders(&auth, req)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Folders(key, r));
            egctx.request_repaint();
        });
    }

    /// Fetch + decode one asset's thumbnail off-thread. PNG decode happens on the worker; only the
    /// GPU texture upload (which must be on the UI thread) is deferred to `apply`. Audio and (feature-
    /// off) 3D assets have no raster thumbnail — those resolve to `Thumb::None` → the typed tile.
    /// Fire-and-forget prefetch of the currently-loaded page's thumbnails/previews (issue #72), at
    /// the edge the grid tiles request — so the server warms the cache ahead of the per-tile reads.
    fn prefetch_page(&self) {
        if self.assets.is_empty() {
            return;
        }
        let assets: Vec<AssetId> = self.assets.iter().map(|a| a.id).collect();
        let (lib, auth) = (self.lib.clone(), self.auth.clone());
        self.rt.spawn(async move {
            let _ = lib
                .prefetch(
                    &auth,
                    PrefetchRequest {
                        assets,
                        edge: Some(THUMB_EDGE),
                    },
                )
                .await;
        });
    }

    fn load_thumb(&self, id: AssetId) {
        let (lib, auth, tx, egctx) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            self.egui_ctx.clone(),
        );
        self.rt.spawn(async move {
            let pixels = match lib.read_thumbnail(&auth, &id, THUMB_EDGE).await {
                Ok(content) => image::load_from_memory(&content.bytes).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    ThumbPixels {
                        size,
                        rgba: rgba.into_raw(),
                    }
                }),
                Err(_) => None,
            };
            let _ = tx.send(Msg::Thumb(id, pixels));
            egctx.request_repaint();
        });
    }

    /// Inspect a transport-bearing result to keep the remote reachability chip current (issue #70).
    fn note_conn_for(&mut self, msg: &Msg) {
        match msg {
            Msg::Assets(r) => self.note_conn(r.is_ok()),
            Msg::Stats(r) => self.note_conn(r.is_ok()),
            Msg::Sources(r) => self.note_conn(r.is_ok()),
            Msg::Detail(r) => self.note_conn(r.is_ok()),
            _ => {}
        }
    }

    /// Fold a posted result into view state.
    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::BackendSwitched(conn, Ok(lib)) => self.rebind_backend(conn, lib),
            Msg::BackendSwitched(conn, Err(e)) => {
                self.switching = false;
                self.status = if conn.is_remote() {
                    ConnStatus::Offline
                } else {
                    ConnStatus::Local
                };
                self.error = Some(format!(
                    "Couldn't connect to {}: {e}",
                    conn.endpoint.as_deref().unwrap_or("embedded")
                ));
            }
            Msg::Assets(Ok(page)) => {
                self.total = page.total;
                // Federation fan-out (issue #39): an incomplete page means a peer missed the merge
                // deadline — remember which, so the browser shows a partial-results notice.
                self.dropped_peers = if page.partial.complete {
                    None
                } else {
                    Some(
                        page.partial
                            .warnings
                            .iter()
                            .filter(|w| w.code == "peer_dropped")
                            .map(|w| w.subject.clone())
                            .collect(),
                    )
                };
                self.assets = page.items;
                self.loading = false;
                self.error = None;
                // Prefetch hint (issue #72): warm this page's thumbnails/previews server-side ahead of
                // the per-tile reads. Fire-and-forget; the tile fetch still generates on a miss.
                self.prefetch_page();
            }
            Msg::Assets(Err(e)) => {
                self.loading = false;
                self.dropped_peers = None;
                self.error = Some(format!("Search failed: {e}"));
            }
            Msg::Detail(Ok(asset)) => {
                // Kick the interactive-3D mesh fetch for a previewable model (not .blend/USD; those
                // fall back to the turntable still / typed tile).
                let id = asset.summary.id;
                let previewable_model = asset.summary.media == MediaType::Model
                    && !matches!(
                        asset.summary.format.as_str(),
                        "blend" | "usd" | "usdz" | "usdc" | "usda"
                    );
                let is_audio = asset.summary.media == MediaType::Audio;
                // Server-computed waveform peaks (issue #73): prefer them so the inspector draws the
                // waveform without re-downloading + re-decoding the audio. `None` until analysed.
                let server_peaks = match &asset.attributes {
                    MediaAttributes::Audio(a) => a.peaks.clone(),
                    _ => None,
                };
                self.detail = Some(asset);
                self.detail_loading = false;
                if previewable_model
                    && self
                        .viewer3d
                        .as_ref()
                        .is_some_and(|v| v.model_for != Some(id))
                {
                    self.load_model_mesh(id);
                }
                // Set up the inspector waveform for a newly-selected audio asset: from server peaks
                // when present, else fall back to a client-side decode.
                if is_audio && self.waveform.as_ref().map(|(w, _)| *w) != Some(id) {
                    match server_peaks {
                        Some(peaks) if !peaks.is_empty() => {
                            self.waveform = Some((id, peaks));
                            self.waveform_loading = false;
                        }
                        _ => {
                            self.waveform = None;
                            self.waveform_loading = true;
                            self.load_waveform(id);
                        }
                    }
                }
            }
            Msg::Detail(Err(e)) => {
                self.detail_loading = false;
                self.error = Some(format!("Couldn't load asset: {e}"));
            }
            Msg::Sources(Ok(s)) => self.sources = s,
            Msg::Sources(Err(e)) => self.error = Some(format!("Couldn't list sources: {e}")),
            Msg::SourceAdded(Ok(())) => {
                self.dirty_sources = true;
                self.dirty_stats = true;
            }
            Msg::SourceAdded(Err(e)) => {
                self.dirty_sources = true;
                self.error = Some(format!("Couldn't add source: {e}"));
            }
            Msg::Stats(Ok(s)) => self.stats = Some(s),
            Msg::Stats(Err(e)) => self.error = Some(format!("Couldn't load stats: {e}")),
            Msg::Thumb(id, Some(px)) => {
                let img = egui::ColorImage::from_rgba_unmultiplied(px.size, &px.rgba);
                let tex = self.egui_ctx.load_texture(
                    format!("thumb-{id}"),
                    img,
                    egui::TextureOptions::LINEAR,
                );
                self.thumbs.insert(id, Thumb::Ready(tex));
            }
            Msg::Thumb(id, None) => {
                self.thumbs.insert(id, Thumb::None);
            }
            Msg::Folders(key, Ok(entries)) => {
                self.folders.insert(key, FolderState::Loaded(entries));
            }
            Msg::Folders(key, Err(_)) => {
                self.folders.insert(key, FolderState::Failed);
            }
            Msg::Similar(id, Ok(hits)) => {
                self.similar_for = Some(id);
                self.similar = hits;
                self.similar_loading = false;
            }
            Msg::Similar(_, Err(e)) => {
                self.similar_loading = false;
                self.error = Some(format!("Similarity search failed: {e}"));
            }
            Msg::Collections(Ok(c)) => self.collections = c,
            Msg::Collections(Err(e)) => {
                self.error = Some(format!("Couldn't load collections: {e}"))
            }
            Msg::CollectionMutated(Ok(())) => {
                // Reload the list (names/counts), the browse view, and the inspected asset's chips.
                let ctx = self.egui_ctx.clone();
                self.load_collections(&ctx);
                self.load_assets(&ctx);
                if let Some(id) = self.selected {
                    self.reload_detail(id);
                }
            }
            Msg::CollectionMutated(Err(e)) => {
                self.error = Some(format!("Collection update failed: {e}"))
            }
            Msg::Export(Ok(rep)) => {
                self.export_status = Some(Ok(format!(
                    "Exported {} asset(s) → {} ({} file(s))",
                    rep.assets, rep.output, rep.files_written
                )));
            }
            Msg::Export(Err(e)) => self.export_status = Some(Err(e)),
            Msg::Duplicates(Ok(g)) => self.duplicates = g,
            Msg::Duplicates(Err(_)) => {} // non-fatal; the dup section just won't show
            Msg::DupView(Ok(g)) => {
                self.dup_groups = g;
                self.dup_loading = false;
            }
            Msg::DupView(Err(e)) => {
                self.dup_loading = false;
                self.error = Some(format!("Couldn't load duplicates: {e}"));
            }
            Msg::AssetRemoved(Ok(())) => {
                let ctx = self.egui_ctx.clone();
                // Clear a stale inspector focus, then refresh the browse, dup caches, and blocklist.
                self.selected = None;
                self.detail = None;
                self.selection.clear();
                self.load_assets(&ctx);
                self.load_duplicates(&ctx);
                if self.dup_view {
                    self.load_dup_view(&ctx);
                }
                self.load_blocklist(&ctx);
            }
            Msg::AssetRemoved(Err(e)) => self.error = Some(format!("Remove failed: {e}")),
            Msg::Blocklist(Ok(b)) => {
                self.blocklist = b;
                self.blocklist_loading = false;
            }
            Msg::Blocklist(Err(e)) => {
                self.blocklist_loading = false;
                self.error = Some(format!("Couldn't load blocklist: {e}"));
            }
            Msg::Unblocked(Ok(())) => {
                let ctx = self.egui_ctx.clone();
                self.load_blocklist(&ctx);
            }
            Msg::Unblocked(Err(e)) => self.error = Some(format!("Unblock failed: {e}")),
            Msg::Convert(Ok(rep)) => {
                self.convert_status = Some(Ok(format!(
                    "{} done, {} failed, {} unsupported → {}",
                    rep.done, rep.failed, rep.unsupported, rep.output_dir
                )));
            }
            Msg::Convert(Err(e)) => self.convert_status = Some(Err(e)),
            Msg::Waveform(id, Ok(peaks)) => {
                self.waveform = Some((id, peaks));
                self.waveform_loading = false;
            }
            Msg::Waveform(_, Err(_)) => self.waveform_loading = false, // non-fatal; just no waveform
            Msg::AudioBytes(id, Ok(bytes)) => self.play_audio(id, bytes),
            Msg::AudioBytes(_, Err(e)) => {
                self.audio_error = Some(format!("couldn't read audio: {e}"))
            }
            Msg::ModelMesh(id, Ok(bytes)) => {
                if let Some(v) = &mut self.viewer3d {
                    let _ = v.set_model(id, &bytes); // fail-soft: inspector uses the still on error
                }
            }
            Msg::ModelMesh(_, Err(_)) => {}

            // Map a live event to the coalesced refresh flags (flushed, throttled, in `update`).
            Msg::Event(ev) => match ev {
                LibraryEvent::AssetAdded(_) | LibraryEvent::AssetRemoved(_) => {
                    self.dirty_assets = true;
                    self.dirty_stats = true;
                }
                LibraryEvent::AssetChanged { id, .. } => {
                    self.dirty_assets = true;
                    self.dirty_stats = true;
                    if self.selected == Some(id) {
                        self.dirty_detail = true;
                    }
                }
                LibraryEvent::SourceState { .. } => self.dirty_sources = true,
                LibraryEvent::JobProgress(status) => {
                    // A finished job (scan/analyze/convert) reshapes the catalogue.
                    if matches!(status.state, JobState::Done) {
                        self.dirty_assets = true;
                        self.dirty_stats = true;
                        self.dirty_sources = true;
                        self.dirty_detail = self.selected.is_some();
                        self.folders.clear(); // subtree counts may have changed
                    }
                }
                LibraryEvent::CatalogReset => {
                    self.dirty_assets = true;
                    self.dirty_stats = true;
                    self.dirty_sources = true;
                    self.folders.clear();
                    self.expanded.clear();
                }
            },
        }
    }

    /// Flush the coalesced live-update refreshes, throttled so a scan's event burst can't re-query
    /// per event. Keeps the flags set (and reschedules a repaint) when inside the throttle window.
    fn flush_live_updates(&mut self, ctx: &egui::Context) {
        if !(self.dirty_assets || self.dirty_stats || self.dirty_sources || self.dirty_detail) {
            return;
        }
        let now = ctx.input(|i| i.time);
        if now - self.last_refresh < 0.4 {
            ctx.request_repaint_after(std::time::Duration::from_millis(400));
            return;
        }
        self.last_refresh = now;
        if std::mem::take(&mut self.dirty_assets) {
            self.load_assets(ctx);
        }
        if std::mem::take(&mut self.dirty_stats) {
            self.load_stats(ctx);
            // Catalog changes also shift smart-folder counts + duplicate groups.
            self.load_collections(ctx);
            self.load_duplicates(ctx);
        }
        if std::mem::take(&mut self.dirty_sources) {
            self.load_sources(ctx);
        }
        if std::mem::take(&mut self.dirty_detail) {
            if let Some(id) = self.selected {
                self.reload_detail(id);
            }
        }
    }

    /// Render one source's folder subtree at `prefix` (issue #66), recursing into expanded nodes and
    /// collecting clicks/loads into `acts` (the panel closure borrows `self` immutably).
    fn folder_level(
        &self,
        ui: &mut egui::Ui,
        source: SourceId,
        prefix: &str,
        depth: usize,
        acts: &mut NavActions,
    ) {
        let key = (source, prefix.to_string());
        match self.folders.get(&key) {
            None => {
                acts.load.push(key);
                indent_hint(ui, depth, "Loading…");
            }
            Some(FolderState::Loading) => indent_hint(ui, depth, "Loading…"),
            Some(FolderState::Failed) => indent_hint(ui, depth, "(couldn't list)"),
            Some(FolderState::Loaded(entries)) => {
                if entries.is_empty() {
                    if depth == 1 {
                        indent_hint(ui, depth, "(no subfolders)");
                    }
                    return;
                }
                for e in entries {
                    let full = format!("{prefix}{}/", e.name);
                    let open = self.expanded.contains(&(source, full.clone()));
                    let scoped = self.source_filter == Some(source)
                        && self.path.as_deref() == Some(full.as_str());
                    ui.horizontal(|ui| {
                        ui.add_space(depth as f32 * 10.0);
                        let disclose = ui.small_button(if open { "v" } else { ">" });
                        ui::access_label(
                            &disclose,
                            &format!(
                                "{} folder {}",
                                if open { "Collapse" } else { "Expand" },
                                e.name
                            ),
                        );
                        if disclose.clicked() {
                            acts.toggle.push((source, full.clone()));
                        }
                        if ui
                            .selectable_label(scoped, format!("{} ({})", e.name, e.asset_count))
                            .clicked()
                        {
                            acts.scope = Some((source, Some(full.clone())));
                        }
                    });
                    if open {
                        self.folder_level(ui, source, &full, depth + 1, acts);
                    }
                }
            }
        }
    }
}

/// A left-indented, muted hint line inside the folder tree (loading / empty / error).
fn indent_hint(ui: &mut egui::Ui, depth: usize, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 10.0 + 6.0);
        ui.label(egui::RichText::new(text).small().weak());
    });
}

impl eframe::App for DamGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Apply the web-matched visuals (issue #68) for the theme the preference currently resolves
        // to — re-applied only when the resolved dark/light mode flips (a toggle, or `System` tracking
        // the OS), so it's a no-op most frames.
        let dark = ctx.theme() == egui::Theme::Dark;
        if self.theme_applied != Some(dark) {
            ctx.set_visuals(crate::theme::visuals(dark));
            self.theme_applied = Some(dark);
        }

        // Drain everything the worker posted since the last frame.
        while let Ok(msg) = self.rx.try_recv() {
            self.note_conn_for(&msg);
            self.apply(msg);
        }
        // Fold any live-update events into throttled refreshes.
        self.flush_live_updates(ctx);
        // Clear the "playing" state once a track finishes on its own.
        if self.audio_sink.as_ref().is_some_and(|s| s.empty()) {
            self.audio_sink = None;
            self.audio_for = None;
        }

        // Actions gathered while rendering (immutable borrows of self), applied after the panels.
        let mut do_query = false;
        let mut grid_click: Option<(AssetId, ClickMods)> = None;
        // Double-click / activate on a grid card or table row (issue #52): the click part already
        // focuses the asset in the inspector; audio additionally starts playing, mirroring the web.
        let mut grid_activate: Option<(AssetId, MediaType)> = None;
        // A table-header sort click (new SORTS index), applied post-panel to re-query.
        let mut sort_click: Option<usize> = None;
        // Duplicates view: toggle request + a member click (selects it and returns to the library) +
        // filter picks (kind/media) collected under the panel borrow, applied after.
        let mut open_dup_view = false;
        let mut dup_member_click: Option<AssetId> = None;
        let mut pick_dup_kind: Option<DupKind> = None;
        let mut pick_dup_media: Option<Option<MediaType>> = None;
        // Blocklist view: toggle request + an unblock action.
        let mut open_blocklist_view = false;
        let mut unblock_hash: Option<ContentHash> = None;
        let mut nav = NavActions::default();
        let mut tag_review: Option<(AssetId, String, ReviewAction)> = None;
        let mut asset_action: Option<AssetAction> = None;
        let mut find_sim: Option<AssetId> = None;
        let mut open_similar: Option<AssetId> = None;
        // Some(Some(id)) selects a collection; Some(None) clears collection mode.
        let mut pick_collection: Option<Option<CollectionId>> = None;
        // A collection CRUD / membership action (create/rename/delete/add/remove), applied post-panel.
        let mut collection_action: Option<CollectionAction> = None;
        // Rename-modal open request (collected here to avoid a &mut self under the collections borrow).
        let mut open_rename: Option<(CollectionId, String)> = None;
        let mut audio_play: Option<AssetId> = None;
        let mut audio_stop = false;
        // Backend switch requested from the Connect dialog (issue #70) — applied after the panels so
        // it doesn't collide with the mutable self-borrow the panels hold.
        let mut connect_request: Option<Conn> = None;
        let mut orbit_drag = egui::Vec2::ZERO;
        let mut orbit_scroll = 0.0f32;
        // 3D-viewer control-bar actions (issue #65), collected under the panel's immutable borrow
        // and applied after the panels — mirrors the web viewer's auto-orbit/reset/wireframe/lighting.
        let (v_auto, v_wire, v_light) = self
            .viewer3d
            .as_ref()
            .map(|v| (v.auto_orbit, v.wireframe, v.lighting))
            .unwrap_or((false, false, 0));
        let mut ctl_auto = false;
        let mut ctl_wire = false;
        let mut ctl_light = false;
        let mut ctl_reset = false;
        let mut ctl_fullscreen = false;
        let mut exit_fullscreen = false;

        // Render the interactive 3D preview into its off-screen texture when the selected asset is a
        // model whose mesh is loaded — the inspector then draws it in place of the turntable still.
        let show_3d = self.selected.is_some()
            && self
                .detail
                .as_ref()
                .is_some_and(|a| a.summary.media == MediaType::Model)
            && self
                .viewer3d
                .as_ref()
                .is_some_and(|v| v.model_for == self.selected);
        let model_tex = if show_3d {
            let dt = ctx.input(|i| i.stable_dt).min(0.1);
            self.viewer3d.as_mut().map(|v| {
                v.tick(dt);
                if v.auto_orbit {
                    ctx.request_repaint();
                }
                v.render()
            })
        } else {
            None
        };

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let dim = colors(ui.visuals().dark_mode).fg_dim;
                let sicon = ui.label(egui::RichText::new(icon::MAGNIFYING_GLASS).color(dim));
                ui::access_text(&sicon, "Search assets");
                let resp = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("Search assets…")
                            .desired_width(240.0),
                    )
                    .labelled_by(sicon.id);
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    do_query = true;
                }
                if ui.button("Search").clicked() {
                    do_query = true;
                }
                // The media filter now lives in the nav LIBRARY section (web parity) — the toolbar
                // stays focused on the query, sort, structured filters, view and export.
                ui.separator();

                // Sort preset (mirrors the web sort control).
                let sort_combo = egui::ComboBox::from_id_salt("sort")
                    .selected_text(SORTS[self.sort].0)
                    .show_ui(ui, |ui| {
                        for (i, (label, _, _)) in SORTS.iter().enumerate() {
                            if ui.selectable_label(self.sort == i, *label).clicked() {
                                self.sort = i;
                                do_query = true;
                            }
                        }
                    });
                ui::access_combo(&sort_combo.response, "Sort order", SORTS[self.sort].0);

                // Search-mode selector — only meaningful with a text query, so it appears with one
                // (semantic-search M5: hybrid/semantic widen with embedding neighbours of the hits).
                if !self.search.trim().is_empty() {
                    let mode_label = match self.mode {
                        SearchMode::Lexical => "Keywords",
                        SearchMode::Hybrid => "Keywords + similar",
                        SearchMode::Semantic => "Most similar",
                    };
                    let mode_combo = egui::ComboBox::from_id_salt("mode")
                        .selected_text(mode_label)
                        .show_ui(ui, |ui| {
                            for (m, label) in [
                                (SearchMode::Lexical, "Keywords"),
                                (SearchMode::Hybrid, "Keywords + similar"),
                                (SearchMode::Semantic, "Most similar"),
                            ] {
                                if ui.selectable_label(self.mode == m, label).clicked() {
                                    self.mode = m;
                                    do_query = true;
                                }
                            }
                        });
                    ui::access_combo(&mode_combo.response, "Search mode", mode_label);
                }

                ui.separator();
                // Advanced Search: structured attribute + tag filters (contextual to the media type).
                let adv_label = if self.adv.is_empty() {
                    format!("{}  Filters", icon::FUNNEL)
                } else {
                    format!("{}  Filters ({})", icon::FUNNEL, self.adv.len())
                };
                if ui
                    .selectable_label(self.adv_open || !self.adv.is_empty(), adv_label)
                    .on_hover_text("Advanced structured & tag filters")
                    .clicked()
                {
                    self.adv_open = !self.adv_open;
                }
                // View toggle, count and export — right-aligned like the web toolbar. (The connection
                // status now lives in the bottom status bar, matching the web StatusBar.)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Export the current view (collection or faceted query) as a manifest.
                    if ui
                        .button(format!("{}  Export", icon::EXPORT))
                        .on_hover_text("Export this view as a manifest")
                        .clicked()
                    {
                        self.export_assets.clear(); // export the view, not a stale selection
                        self.export_status = None;
                        self.export_open = true;
                    }
                    ui.separator();
                    let grid_toggle = ui
                        .selectable_label(self.view == View::Grid, icon::GRID_FOUR)
                        .on_hover_text("Grid");
                    ui::access_toggle(&grid_toggle, self.view == View::Grid, "Grid view");
                    if grid_toggle.clicked() {
                        self.view = View::Grid;
                    }
                    let table_toggle = ui
                        .selectable_label(self.view == View::List, icon::LIST)
                        .on_hover_text("Table");
                    ui::access_toggle(&table_toggle, self.view == View::List, "Table view");
                    if table_toggle.clicked() {
                        self.view = View::List;
                    }
                    ui.separator();
                    if self.loading {
                        ui.spinner();
                    }
                    let count = match self.total {
                        Some(t) => format!("{} of {}", self.assets.len(), t),
                        None => format!("{}", self.assets.len()),
                    };
                    ui.label(egui::RichText::new(count).color(dim));
                });
            });
            ui.add_space(4.0);
        });

        // Batch-action bar (issue #10): shown when a multi-selection is active. Actions collected
        // here and applied after the panels.
        let mut batch_analyze = false;
        let mut batch_export = false;
        let mut batch_clear = false;
        if self.selection.len() > 1 {
            egui::TopBottomPanel::top("batchbar").show(ctx, |ui| {
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    let accent = colors(ui.visuals().dark_mode).accent;
                    ui.label(
                        egui::RichText::new(format!("{} selected", self.selection.len()))
                            .color(accent)
                            .strong(),
                    );
                    ui.separator();
                    if ui.button(format!("{}  Analyze", icon::SPARKLE)).clicked() {
                        batch_analyze = true;
                    }
                    if ui.button(format!("{}  Export", icon::EXPORT)).clicked() {
                        batch_export = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(format!("{}  Clear", icon::X)).clicked() {
                            batch_clear = true;
                        }
                    });
                });
                ui.add_space(3.0);
            });
        }

        egui::SidePanel::left("nav")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                let c = colors(ui.visuals().dark_mode);
                // ── Brand header: app mark + name, with the total-asset count on the right ──
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(icon::STACK).size(18.0).color(c.accent));
                    ui.label(egui::RichText::new("3DAM").size(15.0).strong().color(c.fg));
                    if let Some(stats) = &self.stats {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(format!("{} assets", stats.total))
                                    .size(11.0)
                                    .color(c.fg_dim),
                            );
                        });
                    }
                });
                ui.add_space(6.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;

                        // ── LIBRARY: the media filter (All / Audio / Images / 3D), each row a
                        // colour-coded identity glyph + count, matching the web nav. ──
                        ui::section_label(ui, "Library");
                        let media_rows = [
                            (None, "All assets", icon::STACK, None),
                            (
                                Some(MediaType::Audio),
                                "Audio",
                                icon::MUSIC_NOTES,
                                Some(c.media_audio),
                            ),
                            (
                                Some(MediaType::Image),
                                "Images",
                                icon::IMAGE,
                                Some(c.media_image),
                            ),
                            (
                                Some(MediaType::Model),
                                "3D Models",
                                icon::CUBE,
                                Some(c.media_model),
                            ),
                        ];
                        for (val, label, glyph, col) in media_rows {
                            let count = self.stats.as_ref().map(|s| match val {
                                None => s.total as i64,
                                Some(m) => {
                                    s.by_media.get(media_value(m)).copied().unwrap_or(0) as i64
                                }
                            });
                            let active = self.media_filter == val
                                && !self.favorites
                                && !self.dup_view
                                && !self.blocklist_view;
                            if ui::nav_row(ui, glyph, label, count, active, col).clicked() {
                                self.media_filter = val;
                                self.class = None; // class is media-specific
                                self.favorites = false;
                                self.dup_view = false;
                                self.blocklist_view = false;
                                do_query = true;
                            }
                        }

                        // Favorites facet (composes with media/license/text).
                        let fav_active = self.favorites && !self.dup_view && !self.blocklist_view;
                        if ui::nav_row(ui, icon::HEART, "Favorites", None, fav_active, None)
                            .clicked()
                        {
                            self.favorites = !self.favorites;
                            self.dup_view = false;
                            self.blocklist_view = false;
                            do_query = true;
                        }

                        // Recently added — a shortcut that sorts by scan time, newest first. Active
                        // when that sort is applied in the ordinary library view.
                        let recent_active =
                            self.sort == NEWEST_SORT && !self.dup_view && !self.blocklist_view;
                        if ui::nav_row(ui, icon::CLOCK, "Recently added", None, recent_active, None)
                            .clicked()
                        {
                            self.sort = NEWEST_SORT;
                            self.dup_view = false;
                            self.blocklist_view = false;
                            do_query = true;
                        }

                        // Analysis-class quick facet — contextual to a single active media type (the
                        // full typed-attribute set is Advanced Search). Chips toggle a class filter.
                        if let Some(m) = self.media_filter {
                            let (_, options) = class_facet(m);
                            ui::section_label(ui, "Type");
                            ui.horizontal_wrapped(|ui| {
                                for (val, label) in options {
                                    let on = self.class.as_deref() == Some(*val);
                                    if ui.selectable_label(on, *label).clicked() {
                                        self.class =
                                            if on { None } else { Some((*val).to_string()) };
                                        do_query = true;
                                    }
                                }
                            });
                        }

                        // ── LICENSE: each class as a coloured dot + label + toggle ──
                        ui::section_label(ui, "License");
                        for (lic, label) in LICENSES {
                            let on = self.license == Some(*lic);
                            let col = ui::license_color(&c, label);
                            if ui::nav_row(ui, icon::CIRCLE, label, None, on, Some(col)).clicked() {
                                self.license = if on { None } else { Some(*lic) };
                                do_query = true;
                            }
                        }

                        // ── SOURCES ──
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("SOURCES").size(10.0).color(c.fg_dim));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button(egui::RichText::new(icon::PLUS))
                                        .on_hover_text("Add a source")
                                        .clicked()
                                    {
                                        nav.toggle_add = true;
                                    }
                                },
                            );
                        });
                        // Add-source form: a local folder or a federated 3DAM peer (issue #39).
                        // SFTP/SMB (credentials) stay owed.
                        if self.add_open {
                            ui.horizontal(|ui| {
                                for (kind, label) in [
                                    (SourceKind::LocalFs, "Folder"),
                                    (SourceKind::Federated, "Peer"),
                                ] {
                                    if ui.selectable_label(self.add_kind == kind, label).clicked() {
                                        self.add_kind = kind;
                                    }
                                }
                            });
                            if self.add_kind == SourceKind::Federated {
                                // Federated peer: endpoint + optional bearer token (rides in
                                // `options.password`; the engine validates the peer at add time).
                                let ep = ui.add(
                                    egui::TextEdit::singleline(&mut self.add_endpoint)
                                        .hint_text("3dam://host:7878")
                                        .desired_width(150.0),
                                );
                                ui::access_edit_label(&ep, "Peer endpoint");
                                let tok = ui.add(
                                    egui::TextEdit::singleline(&mut self.add_token)
                                        .hint_text("token (optional)")
                                        .password(true)
                                        .desired_width(150.0),
                                );
                                ui::access_edit_label(&tok, "Peer token");
                                let submit = ui.small_button("Add peer").clicked()
                                    || ((ep.lost_focus() || tok.lost_focus())
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                                if submit && !self.add_endpoint.trim().is_empty() {
                                    let token = (!self.add_token.is_empty())
                                        .then(|| self.add_token.clone());
                                    nav.source = Some(SourceAction::AddFederated(
                                        self.add_endpoint.trim().to_string(),
                                        token,
                                    ));
                                }
                            } else {
                                ui.horizontal(|ui| {
                                    let resp = ui.add(
                                        egui::TextEdit::singleline(&mut self.add_path)
                                            .hint_text("/path/to/assets")
                                            .desired_width(150.0),
                                    );
                                    ui::access_edit_label(&resp, "Source path");
                                    let submit = ui.small_button("Add").clicked()
                                        || (resp.lost_focus()
                                            && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                                    if submit && !self.add_path.trim().is_empty() {
                                        nav.source = Some(SourceAction::AddLocal(
                                            self.add_path.trim().to_string(),
                                        ));
                                    }
                                });
                            }
                        }
                        if self.sources.is_empty() {
                            ui.label(egui::RichText::new("No sources yet — add one above.").weak());
                        }
                        // Each source is an expandable folder tree (issue #66): the disclosure loads its
                        // directory tree lazily; clicking the name scopes the browse to that source (or, for
                        // a folder, to its subtree via the path-prefix filter). Trailing controls rescan/remove.
                        for s in &self.sources {
                            let sid = s.id;
                            // Federated peers (issue #39) are catalog rows, not local bytes: no
                            // folder tree to expand, no scan/rescan, and the local asset count
                            // doesn't describe them — render a "peer" row instead.
                            let federated = s.kind == SourceKind::Federated;
                            let open = !federated && self.expanded.contains(&(sid, String::new()));
                            let scoped = self.source_filter == Some(sid) && self.path.is_none();
                            ui.horizontal(|ui| {
                                if federated {
                                    let peer = ui.label(
                                        egui::RichText::new(format!(
                                            "{}  {}",
                                            icon::GLOBE_HEMISPHERE_WEST,
                                            s.name
                                        ))
                                        .color(c.fg_muted),
                                    );
                                    peer.on_hover_text(format!(
                                        "Federated peer — {} (results merge into the library)",
                                        s.uri
                                    ));
                                    ui::pill(ui, "peer", c.fg_muted, ui::tint(c.fg_muted, 0.14));
                                } else {
                                    let disclose = ui.small_button(if open {
                                        icon::CARET_DOWN
                                    } else {
                                        icon::CARET_RIGHT
                                    });
                                    ui::access_label(
                                        &disclose,
                                        &format!(
                                            "{} source {}",
                                            if open { "Collapse" } else { "Expand" },
                                            s.name
                                        ),
                                    );
                                    if disclose.clicked() {
                                        nav.toggle.push((sid, String::new()));
                                    }
                                    if ui
                                        .selectable_label(
                                            scoped,
                                            format!(
                                                "{}  {} ({})",
                                                icon::FOLDER,
                                                s.name,
                                                s.stats.asset_count
                                            ),
                                        )
                                        .clicked()
                                    {
                                        nav.scope = Some((sid, None));
                                    }
                                }
                                if self.confirm_remove == Some(sid) {
                                    ui.label(egui::RichText::new("remove?").small());
                                    if ui.small_button("yes").clicked() {
                                        nav.source = Some(SourceAction::Remove(sid));
                                        nav.set_confirm = Some(None);
                                    }
                                    if ui.small_button("no").clicked() {
                                        nav.set_confirm = Some(None);
                                    }
                                } else {
                                    if !federated {
                                        let rescan = ui
                                            .small_button(icon::ARROWS_CLOCKWISE)
                                            .on_hover_text("Rescan source");
                                        ui::access_label(
                                            &rescan,
                                            &format!("Rescan source {}", s.name),
                                        );
                                        if rescan.clicked() {
                                            nav.source = Some(SourceAction::Rescan(sid));
                                        }
                                    }
                                    let remove =
                                        ui.small_button(icon::TRASH).on_hover_text("Remove source");
                                    ui::access_label(&remove, &format!("Remove source {}", s.name));
                                    if remove.clicked() {
                                        nav.set_confirm = Some(Some(sid));
                                    }
                                }
                            });
                            if open {
                                self.folder_level(ui, sid, "", 1, &mut nav);
                            }
                        }

                        // ── COLLECTIONS & smart folders — clicking one browses its members; the
                        // header "+" creates one, right-click renames/deletes (web parity). ──
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("COLLECTIONS")
                                    .size(10.0)
                                    .color(c.fg_dim),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button(egui::RichText::new(icon::PLUS))
                                        .on_hover_text("Create a collection")
                                        .clicked()
                                    {
                                        self.collection_new_name.clear();
                                        self.collection_new_smart = false;
                                        self.collection_new_open = true;
                                    }
                                },
                            );
                        });
                        if self.collections.is_empty() {
                            ui.label(
                                egui::RichText::new("Group assets into a collection")
                                    .small()
                                    .weak(),
                            );
                        }
                        for col_item in &self.collections {
                            let on = self.collection == Some(col_item.id);
                            let smart = matches!(col_item.kind, CollectionKind::Smart);
                            let glyph = if smart {
                                icon::SPARKLE
                            } else {
                                icon::FOLDER_OPEN
                            };
                            let icon_col = if smart { Some(c.accent) } else { None };
                            let resp = ui::nav_row(
                                ui,
                                glyph,
                                &col_item.name,
                                col_item.count.map(|n| n as i64),
                                on,
                                icon_col,
                            )
                            .on_hover_text(if smart {
                                "Smart collection"
                            } else {
                                "Collection"
                            });
                            if resp.clicked() {
                                pick_collection = Some(if on { None } else { Some(col_item.id) });
                            }
                            resp.context_menu(|ui| {
                                if ui
                                    .button(format!("{}  Rename…", icon::PENCIL_SIMPLE))
                                    .clicked()
                                {
                                    open_rename = Some((col_item.id, col_item.name.clone()));
                                    ui.close_menu();
                                }
                                if ui.button(format!("{}  Delete", icon::TRASH)).clicked() {
                                    collection_action = Some(CollectionAction::Delete(col_item.id));
                                    ui.close_menu();
                                }
                            });
                        }

                        // ── Footer: dedupe / blocklist views + the theme cycle (web parity). ──
                        ui::hairline(ui);
                        let dup_count = self
                            .duplicates
                            .iter()
                            .filter(|g| g.members.len() > 1)
                            .count();
                        if ui::nav_row(
                            ui,
                            icon::COPY,
                            "Duplicates",
                            (dup_count > 0).then_some(dup_count as i64),
                            self.dup_view,
                            None,
                        )
                        .clicked()
                        {
                            open_dup_view = true;
                        }
                        let bl_count = self.blocklist.len();
                        if ui::nav_row(
                            ui,
                            icon::PROHIBIT,
                            "Blocklist",
                            (bl_count > 0).then_some(bl_count as i64),
                            self.blocklist_view,
                            None,
                        )
                        .clicked()
                        {
                            open_blocklist_view = true;
                        }
                        // Theme cycle System → Dark → Light (dark-first); egui resolves the concretes.
                        let (tglyph, tlabel) = match self.theme_pref {
                            egui::ThemePreference::System => (icon::MONITOR, "Theme: System"),
                            egui::ThemePreference::Dark => (icon::MOON, "Theme: Dark"),
                            egui::ThemePreference::Light => (icon::SUN, "Theme: Light"),
                        };
                        if ui::link_row(ui, tglyph, tlabel, false).clicked() {
                            self.theme_pref = match self.theme_pref {
                                egui::ThemePreference::System => egui::ThemePreference::Dark,
                                egui::ThemePreference::Dark => egui::ThemePreference::Light,
                                egui::ThemePreference::Light => egui::ThemePreference::System,
                            };
                            ui.ctx().set_theme(self.theme_pref);
                        }
                        ui.add_space(6.0);
                    });
            });

        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.add_space(2.0);
                ui.heading(format!("{}  Inspector", icon::SLIDERS_HORIZONTAL));
                ui::hairline(ui);
                // Key the scroll state to the selected asset so switching assets resets to the top
                // (web parity) instead of inheriting the previous asset's offset and hiding the title.
                egui::ScrollArea::vertical()
                    .id_salt(self.selected)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.detail_loading {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Loading…");
                            });
                        } else if let Some(asset) = &self.detail {
                            let thumb = match self.thumbs.get(&asset.summary.id) {
                                Some(Thumb::Ready(tex)) => Some(tex),
                                _ => None,
                            };
                            // Interactive 3D preview replaces the still when the mesh is loaded.
                            if let Some(tex) = model_tex {
                                let sz = ui.available_width().min(300.0);
                                let resp = ui.add(
                                    egui::Image::new((tex, egui::vec2(sz, sz)))
                                        .sense(egui::Sense::drag()),
                                );
                                if resp.dragged() {
                                    orbit_drag = resp.drag_delta();
                                }
                                if resp.hovered() {
                                    orbit_scroll = ui.input(|i| i.raw_scroll_delta.y);
                                }
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .selectable_label(
                                            v_auto,
                                            format!("{}  Auto-orbit", icon::ARROWS_CLOCKWISE),
                                        )
                                        .on_hover_text("Spin the model continuously")
                                        .clicked()
                                    {
                                        ctl_auto = true;
                                    }
                                    if ui
                                        .selectable_label(
                                            v_wire,
                                            format!("{}  Wireframe", icon::CUBE),
                                        )
                                        .on_hover_text("Show mesh edges")
                                        .clicked()
                                    {
                                        ctl_wire = true;
                                    }
                                    let light_name = match v_light {
                                        0 => "Studio",
                                        1 => "Soft",
                                        _ => "Flat",
                                    };
                                    if ui
                                        .button(format!("{}  {light_name}", icon::SUN))
                                        .on_hover_text("Cycle lighting mode")
                                        .clicked()
                                    {
                                        ctl_light = true;
                                    }
                                    let reset = ui
                                        .button(icon::ARROW_COUNTER_CLOCKWISE)
                                        .on_hover_text("Reset view");
                                    ui::access_label(&reset, "Reset view");
                                    if reset.clicked() {
                                        ctl_reset = true;
                                    }
                                    let fullscreen = ui.button(icon::ARROWS_OUT).on_hover_text(
                                        "Expand the viewer to fill the window (Esc to exit)",
                                    );
                                    ui::access_label(&fullscreen, "Fullscreen viewer");
                                    if fullscreen.clicked() {
                                        ctl_fullscreen = true;
                                    }
                                });
                                ui.label(
                                    egui::RichText::new("drag to orbit · scroll to zoom")
                                        .small()
                                        .weak(),
                                );
                                inspector(ui, asset, None, &mut tag_review, &mut asset_action);
                            } else {
                                inspector(ui, asset, thumb, &mut tag_review, &mut asset_action);
                            }

                            // Audio preview: waveform + play/stop for the selected audio asset.
                            if asset.summary.media == MediaType::Audio {
                                let aid = asset.summary.id;
                                // Waveform (peaks computed off-thread on selection).
                                match &self.waveform {
                                    Some((wid, peaks)) if *wid == aid => draw_waveform(ui, peaks),
                                    _ if self.waveform_loading => {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            ui.label(
                                                egui::RichText::new("waveform…").small().weak(),
                                            );
                                        });
                                    }
                                    _ => {}
                                }
                                let playing = self.audio_for == Some(aid);
                                ui.horizontal(|ui| {
                                    if playing {
                                        if ui.button(format!("{}  Stop", icon::STOP)).clicked() {
                                            audio_stop = true;
                                        }
                                    } else if ui.button(format!("{}  Play", icon::PLAY)).clicked() {
                                        audio_play = Some(aid);
                                    }
                                    if let Some(err) = &self.audio_error {
                                        ui.colored_label(
                                            colors(ui.visuals().dark_mode).danger,
                                            err,
                                        );
                                    }
                                });
                            }

                            // Collection memberships: current manual-collection chips (click × to
                            // remove) + an "Add to…" menu of the manual collections it isn't in yet.
                            {
                                let aid = asset.summary.id;
                                ui.separator();
                                ui::section_label(ui, "Collections");
                                ui.horizontal_wrapped(|ui| {
                                    for cid in &asset.collections {
                                        let name = self
                                            .collections
                                            .iter()
                                            .find(|c| c.id == *cid)
                                            .map(|c| c.name.as_str())
                                            .unwrap_or("(collection)");
                                        if ui
                                            .small_button(format!("{name} ×"))
                                            .on_hover_text("Remove from collection")
                                            .clicked()
                                        {
                                            collection_action =
                                                Some(CollectionAction::RemoveMember {
                                                    id: *cid,
                                                    asset: aid,
                                                });
                                        }
                                    }
                                    // Manual collections this asset isn't a member of yet.
                                    let addable: Vec<(CollectionId, String)> = self
                                        .collections
                                        .iter()
                                        .filter(|c| {
                                            matches!(c.kind, CollectionKind::Manual)
                                                && !asset.collections.contains(&c.id)
                                        })
                                        .map(|c| (c.id, c.name.clone()))
                                        .collect();
                                    if !addable.is_empty() {
                                        ui.menu_button("Add to…", |ui| {
                                            for (cid, name) in addable {
                                                if ui.button(name).clicked() {
                                                    collection_action =
                                                        Some(CollectionAction::AddMember {
                                                            id: cid,
                                                            asset: aid,
                                                        });
                                                    ui.close_menu();
                                                }
                                            }
                                        });
                                    }
                                });
                            }

                            // Exact duplicates: the byte-identical copies of this asset (grouping only —
                            // 3DAM never deletes; the user disposes of a copy). Absent when it has no twin.
                            let id = asset.summary.id;
                            if let Some(g) = self.duplicates.iter().find(|g| {
                                g.members.len() > 1 && g.members.iter().any(|m| m.id == id)
                            }) {
                                ui::section_label(
                                    ui,
                                    &format!("Duplicates ({})", g.members.len() - 1),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "byte-identical ({}); dispose of copies yourself",
                                        g.signal
                                    ))
                                    .small()
                                    .weak(),
                                );
                                // Cap the rendered list — a big pack can have hundreds of identical copies.
                                const DUP_CAP: usize = 24;
                                for m in g.members.iter().take(DUP_CAP) {
                                    let keep = m.id == g.suggested_keep;
                                    let cur = m.id == id;
                                    let label = format!(
                                        "{}{}{}",
                                        if keep { "[keep] " } else { "" },
                                        m.name,
                                        if cur { "  (this)" } else { "" },
                                    );
                                    if ui.selectable_label(cur, label).clicked() {
                                        open_similar = Some(m.id);
                                    }
                                }
                                if g.members.len() > DUP_CAP {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "… and {} more",
                                            g.members.len() - DUP_CAP
                                        ))
                                        .small()
                                        .weak(),
                                    );
                                }
                            }

                            // "Find similar" (tech-spec 05 §3): opt-in nearest-neighbour ranking. Un-analyzed
                            // assets have no vector, so we point at Analyze instead of querying into the void.
                            ui.separator();
                            ui::section_label(ui, "Similar");
                            if asset.timestamps.analyzed.is_none() {
                                ui.label(
                                    egui::RichText::new("Analyze this asset to find similar ones.")
                                        .weak(),
                                );
                            } else if self.similar_for == Some(id) {
                                if self.similar_loading {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        ui.label("Searching…");
                                    });
                                } else if self.similar.is_empty() {
                                    ui.label(
                                        egui::RichText::new("No similar assets found.").weak(),
                                    );
                                } else {
                                    for hit in &self.similar {
                                        let pct = (hit.score * 100.0).round() as i32;
                                        if ui
                                            .selectable_label(
                                                false,
                                                format!(
                                                    "{}  {}   ·   {pct}%",
                                                    media_tag(hit.asset.media),
                                                    hit.asset.name
                                                ),
                                            )
                                            .clicked()
                                        {
                                            open_similar = Some(hit.asset.id);
                                        }
                                    }
                                }
                            } else if ui.button("Find similar").clicked() {
                                find_sim = Some(id);
                            }
                        } else {
                            ui.label(egui::RichText::new("Select an asset to inspect it.").weak());
                        }
                    });
            });

        // ── Bottom status bar (web StatusBar parity): live job pulse + per-media breakdown on the
        // left; connection pill, server and version on the right. Must precede the CentralPanel. ──
        egui::TopBottomPanel::bottom("statusbar").show(ctx, |ui| {
            let c = colors(ui.visuals().dark_mode);
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add_space(2.0);
                if self.loading {
                    ui.spinner();
                    ui.label(egui::RichText::new("Working…").size(11.0).color(c.fg_dim));
                    ui.label(egui::RichText::new("·").size(11.0).color(c.border_strong));
                }
                if let Some(stats) = &self.stats {
                    // Per-media counts, each in its identity hue (matches the web breakdown).
                    let mut first = true;
                    for (mv, col) in [
                        ("model", c.media_model),
                        ("image", c.media_image),
                        ("audio", c.media_audio),
                    ] {
                        if !first {
                            ui.label(egui::RichText::new("·").size(11.0).color(c.border_strong));
                        }
                        first = false;
                        let n = stats.by_media.get(mv).copied().unwrap_or(0);
                        ui.label(
                            egui::RichText::new(format!("{} {}", ui::media_tag(mv), n))
                                .size(11.0)
                                .color(col),
                        );
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                            .size(11.0)
                            .color(c.fg_dim),
                    );
                    ui.label(egui::RichText::new("·").size(11.0).color(c.border_strong));
                    // Connection pill: coloured dot + label; click to open the Connect dialog.
                    let (col, label) = self.status_chip(ui.visuals().dark_mode);
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(format!("● {label}"))
                                    .size(11.0)
                                    .color(col),
                            )
                            .frame(false),
                        )
                        .on_hover_text("Connect to a 3DAM server")
                        .clicked()
                    {
                        self.connect_url = self.conn.endpoint.clone().unwrap_or_default();
                        self.connect_token = self.conn.token.clone().unwrap_or_default();
                        self.connect_open = true;
                    }
                });
            });
            ui.add_space(3.0);
        });

        // Assets whose thumbnails are worth fetching this frame (visible + not yet requested),
        // gathered under the immutable render borrow and kicked off afterwards.
        let mut to_load: Vec<AssetId> = Vec::new();
        // A breadcrumb crumb click re-scopes the folder browse: `Some(prefix)` for a parent folder,
        // `Some(None)` for the source root (issue #66, web Breadcrumb parity).
        let mut breadcrumb_to: Option<Option<String>> = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(err) = &self.error {
                ui.colored_label(colors(ui.visuals().dark_mode).danger, err);
                ui.separator();
            }
            // ── Partial-results strip (issue #39): one slim warn-tinted line, non-blocking — the
            // results below are real, just possibly missing a slow peer's contribution. Mirrors the
            // web Browser strip. Hidden in the dup/blocklist views (they aren't federated queries).
            if !self.blocklist_view && !self.dup_view {
                if let Some(peers) = &self.dropped_peers {
                    let c = colors(ui.visuals().dark_mode);
                    let mut text =
                        "Some sources didn't answer — results may be partial".to_string();
                    if !peers.is_empty() {
                        text.push_str(&format!(" ({})", peers.join(", ")));
                    }
                    egui::Frame::new()
                        .fill(ui::tint(c.warn, 0.10))
                        .corner_radius(3)
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(icon::CLOUD_SLASH)
                                        .size(12.0)
                                        .color(c.warn),
                                );
                                ui.label(egui::RichText::new(text).size(11.0).color(c.warn));
                            });
                        });
                    ui.add_space(4.0);
                }
            }
            // ── Breadcrumb (issue #66): source + folder segments over the Browser, click to
            // re-scope up — mirrors the web Breadcrumb (hidden in collection/dup/blocklist views). ──
            if self.collection.is_none() && !self.blocklist_view && !self.dup_view {
                if let Some(sid) = self.source_filter {
                    let c = colors(ui.visuals().dark_mode);
                    let src_name = self
                        .sources
                        .iter()
                        .find(|s| s.id == sid)
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| "Source".to_string());
                    let segs: Vec<&str> = self
                        .path
                        .as_deref()
                        .unwrap_or("")
                        .split('/')
                        .filter(|s| !s.is_empty())
                        .collect();
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if segs.is_empty() {
                            ui.label(egui::RichText::new(&src_name).size(11.0).color(c.fg_muted));
                        } else if ui
                            .link(egui::RichText::new(&src_name).size(11.0).color(c.fg_dim))
                            .clicked()
                        {
                            breadcrumb_to = Some(None);
                        }
                        let mut acc = String::new();
                        for (i, seg) in segs.iter().enumerate() {
                            acc.push_str(seg);
                            acc.push('/');
                            ui.label(egui::RichText::new("›").size(11.0).color(c.border_strong));
                            if i + 1 == segs.len() {
                                ui.label(egui::RichText::new(*seg).size(11.0).color(c.fg_muted));
                            } else if ui
                                .link(egui::RichText::new(*seg).size(11.0).color(c.fg_dim))
                                .clicked()
                            {
                                breadcrumb_to = Some(Some(acc.clone()));
                            }
                        }
                    });
                    ui.separator();
                }
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.blocklist_view {
                        self.blocklist_review(ui, &mut unblock_hash);
                    } else if self.dup_view {
                        self.dup_review(
                            ui,
                            &mut dup_member_click,
                            &mut asset_action,
                            &mut pick_dup_kind,
                            &mut pick_dup_media,
                        );
                    } else if self.assets.is_empty() && !self.loading {
                        ui.add_space(12.0);
                        ui.label(egui::RichText::new("No assets match.").weak());
                    } else if self.view == View::Grid {
                        self.grid(
                            ui,
                            &mut grid_click,
                            &mut grid_activate,
                            &mut to_load,
                            &mut asset_action,
                        );
                    } else {
                        self.list(
                            ui,
                            &mut grid_click,
                            &mut grid_activate,
                            &mut asset_action,
                            &mut sort_click,
                        );
                    }
                });
        });

        // Export-manifest modal (non-destructive; writes a JSON/CSV/sidecar over the current view).
        let mut do_export = false;
        if self.export_open {
            let mut open = true;
            egui::Window::new("Export manifest")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Format:");
                        for (f, label) in [
                            (ExportFormat::Json, "JSON"),
                            (ExportFormat::Csv, "CSV"),
                            (ExportFormat::Sidecar, "Sidecar"),
                        ] {
                            if ui
                                .selectable_label(self.export_format == f, label)
                                .clicked()
                            {
                                self.export_format = f;
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        let l = ui.label("Output:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.export_path).desired_width(320.0),
                        )
                        .labelled_by(l.id);
                    });
                    ui.checkbox(
                        &mut self.export_attribution,
                        "Attribution / license fields only",
                    );
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Export").clicked() {
                            do_export = true;
                        }
                        if ui.button("Close").clicked() {
                            self.export_open = false;
                        }
                    });
                    match &self.export_status {
                        Some(Ok(msg)) => {
                            ui.colored_label(colors(ui.visuals().dark_mode).ok, msg);
                        }
                        Some(Err(msg)) => {
                            ui.colored_label(
                                colors(ui.visuals().dark_mode).danger,
                                format!("Export failed: {msg}"),
                            );
                        }
                        None => {}
                    }
                });
            if !open {
                self.export_open = false;
            }
        }
        if do_export {
            self.export_status = None;
            self.run_export();
        }

        // Batch actions over the multi-selection (issue #10).
        if batch_analyze {
            self.submit_analyze_batch(self.selection.iter().copied().collect(), false);
        }
        if batch_export {
            self.export_assets = self.selection.iter().copied().collect();
            self.export_status = None;
            self.export_open = true;
        }
        if batch_clear {
            self.selection.clear();
            self.anchor = None;
        }

        // Convert modal (image/audio transcode; non-destructive, writes to an output dir).
        let mut do_convert = false;
        if self.convert_open {
            let mut open = true;
            if let Some((_, media)) = self.convert_asset {
                let formats = CONVERT_FORMATS
                    .iter()
                    .find(|(m, _)| *m == media)
                    .map(|(_, f)| *f)
                    .unwrap_or(&[]);
                egui::Window::new("Convert")
                    .collapsible(false)
                    .resizable(false)
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("To:");
                            for f in formats {
                                if ui
                                    .selectable_label(self.convert_format == *f, f.to_uppercase())
                                    .clicked()
                                {
                                    self.convert_format = (*f).to_string();
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            let l = ui.label("Output dir:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.convert_output)
                                    .desired_width(300.0),
                            )
                            .labelled_by(l.id);
                        });
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Convert").clicked() {
                                do_convert = true;
                            }
                            if ui.button("Close").clicked() {
                                self.convert_open = false;
                            }
                        });
                        match &self.convert_status {
                            Some(Ok(msg)) => {
                                ui.colored_label(colors(ui.visuals().dark_mode).ok, msg);
                            }
                            Some(Err(msg)) => {
                                ui.colored_label(
                                    colors(ui.visuals().dark_mode).danger,
                                    format!("Convert failed: {msg}"),
                                );
                            }
                            None => {}
                        }
                    });
            }
            if !open {
                self.convert_open = false;
            }
        }
        if do_convert {
            if let Some((id, media)) = self.convert_asset {
                self.convert_status = None;
                self.run_convert(
                    id,
                    media,
                    self.convert_format.clone(),
                    self.convert_output.clone(),
                );
            }
        }

        // Folder-tree actions (issue #66), applied before the query so a folder scope re-queries
        // with the updated source/path filters.
        for k in nav.toggle {
            if !self.expanded.remove(&k) {
                self.expanded.insert(k);
            }
        }
        for k in nav.load {
            self.folders.insert(k.clone(), FolderState::Loading);
            self.load_folders(k);
        }
        // Source management (add/remove/rescan + the add input's toggle / confirm state).
        if nav.toggle_add {
            self.add_open = !self.add_open;
        }
        if let Some(c) = nav.set_confirm {
            self.confirm_remove = c;
        }
        match nav.source {
            Some(SourceAction::AddLocal(path)) => {
                self.add_local_source(path);
                self.add_path.clear();
                self.add_open = false;
                self.dirty_sources = true;
            }
            Some(SourceAction::AddFederated(endpoint, token)) => {
                self.add_federated_source(endpoint, token);
                self.add_endpoint.clear();
                self.add_token.clear();
                self.add_open = false;
                // The rail refresh rides on `Msg::SourceAdded` — the engine validates the peer
                // first, so an eager reload here would race the handshake.
            }
            Some(SourceAction::Remove(id)) => {
                self.remove_source_svc(id);
                // Don't leave the browse scoped to a source that's going away.
                if self.source_filter == Some(id) {
                    self.source_filter = None;
                    self.path = None;
                    do_query = true;
                }
                self.dirty_sources = true;
                self.dirty_stats = true;
            }
            Some(SourceAction::Rescan(id)) => self.rescan_source(id),
            None => {}
        }
        if let Some((sid, p)) = nav.scope {
            // Clicking the already-active source/folder clears the scope; otherwise set it.
            if self.source_filter == Some(sid) && self.path == p {
                self.source_filter = None;
                self.path = None;
            } else {
                self.source_filter = Some(sid);
                self.path = p;
            }
            do_query = true;
        }
        // A breadcrumb click re-scopes the folder browse up the tree (source stays as-is).
        if let Some(target) = breadcrumb_to {
            self.path = target;
            do_query = true;
        }

        // A table-header sort click changes the sort preset and re-queries.
        if let Some(idx) = sort_click {
            if idx != self.sort {
                self.sort = idx;
                do_query = true;
            }
        }

        // Duplicates view: toggle, filter changes, and member navigation.
        if open_dup_view {
            self.dup_view = !self.dup_view;
            if self.dup_view {
                self.blocklist_view = false; // the two full-panel views are mutually exclusive
                self.dup_loading = true;
                self.load_dup_view(ctx);
            }
        }
        let mut dup_reload = false;
        if let Some(k) = pick_dup_kind {
            if k != self.dup_kind {
                self.dup_kind = k;
                dup_reload = true;
            }
        }
        if let Some(m) = pick_dup_media {
            if m != self.dup_media {
                self.dup_media = m;
                dup_reload = true;
            }
        }
        if dup_reload {
            self.dup_loading = true;
            self.load_dup_view(ctx);
        }
        if let Some(id) = dup_member_click {
            self.dup_view = false; // return to the library with the member selected
            self.select_asset(id, ClickMods::default(), ctx);
        }

        // Blocklist view: toggle + unblock.
        if open_blocklist_view {
            self.blocklist_view = !self.blocklist_view;
            if self.blocklist_view {
                self.dup_view = false; // the two full-panel views are mutually exclusive
                self.blocklist_loading = true;
                self.load_blocklist(ctx);
            }
        }
        if let Some(hash) = unblock_hash {
            self.unblock(hash);
        }

        // Advanced Search modal — returns true when the filter set changed (re-query).
        if self.advanced_modal(ctx) {
            do_query = true;
        }

        // Collection CRUD: open the rename modal, apply a gathered action, and draw the modals.
        if let Some(r) = open_rename {
            self.collection_rename = Some(r);
        }
        if let Some(action) = collection_action {
            self.collection_action(action);
        }
        self.collection_modals(ctx);
        self.confirm_remove_modal(ctx);

        // Connect dialog (hosted mode, issue #70): pick/enter a server or return to embedded.
        if let Some(c) = self.connect_dialog(ctx) {
            connect_request = Some(c);
        }

        // Collection selection (mutually exclusive with the facets, mirroring the web).
        if let Some(sel) = pick_collection {
            self.collection = sel;
            if sel.is_some() {
                self.media_filter = None;
                self.class = None;
                self.license = None;
                self.favorites = false;
                self.source_filter = None;
                self.path = None;
                self.search.clear();
            }
            do_query = true;
        } else if do_query {
            // Any facet/search/sort/scope change exits collection mode.
            self.collection = None;
        }

        if do_query {
            self.load_assets(ctx);
        }
        for id in to_load {
            self.thumbs.insert(id, Thumb::Loading);
            self.load_thumb(id);
        }
        if let Some((id, mods)) = grid_click {
            self.select_asset(id, mods, ctx);
        }
        // Double-click = activate (issue #52): audio starts playing right away; other media just
        // opens in the inspector, which the click above already did.
        if let Some((id, media)) = grid_activate {
            if media == MediaType::Audio {
                self.audio_error = None;
                self.load_audio(id);
            }
        }
        if let Some((id, tag, action)) = tag_review {
            self.review_tag(id, tag, action);
        }
        if let Some(id) = audio_play {
            self.audio_error = None;
            self.load_audio(id);
        }
        if audio_stop {
            self.stop_audio();
        }
        // Switch backend last (embedded ↔ remote) — rebuilds the service and re-subscribes.
        if let Some(conn) = connect_request {
            self.switch_backend(conn);
        }
        // Fullscreen 3D-viewer overlay (#65, final part): the preview fills the window over the
        // normal layout, with the same orbit/zoom and control bar. Esc or "Exit fullscreen" leaves.
        if self.viewer_fullscreen {
            if let Some(tex) = model_tex {
                egui::Window::new("viewer-fullscreen")
                    .title_bar(false)
                    .fixed_rect(ctx.screen_rect())
                    .frame(egui::Frame::default().fill(crate::theme::VIEWER_BG))
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            if ui.selectable_label(v_auto, "Auto-orbit").clicked() {
                                ctl_auto = true;
                            }
                            if ui.selectable_label(v_wire, "Wireframe").clicked() {
                                ctl_wire = true;
                            }
                            let light_name = match v_light {
                                0 => "Light: Studio",
                                1 => "Light: Soft",
                                _ => "Light: Flat",
                            };
                            if ui.button(light_name).clicked() {
                                ctl_light = true;
                            }
                            if ui.button("Reset").clicked() {
                                ctl_reset = true;
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Exit fullscreen").clicked() {
                                        exit_fullscreen = true;
                                    }
                                    ui.label(
                                        egui::RichText::new(
                                            "drag to orbit · scroll to zoom · Esc to exit",
                                        )
                                        .small()
                                        .weak(),
                                    );
                                },
                            );
                        });
                        // A large centred square viewer filling the remaining space.
                        let sz = ui.available_width().min(ui.available_height()).max(64.0);
                        ui.vertical_centered(|ui| {
                            let resp = ui.add(
                                egui::Image::new((tex, egui::vec2(sz, sz)))
                                    .sense(egui::Sense::drag()),
                            );
                            if resp.dragged() {
                                orbit_drag = resp.drag_delta();
                            }
                            if resp.hovered() {
                                orbit_scroll = ui.input(|i| i.raw_scroll_delta.y);
                            }
                        });
                    });
                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    exit_fullscreen = true;
                }
            } else {
                exit_fullscreen = true; // model went away → leave fullscreen
            }
        }

        // Apply an orbit/zoom to the 3D preview and repaint so it re-renders this interaction.
        if orbit_drag != egui::Vec2::ZERO || orbit_scroll != 0.0 {
            if let Some(v) = &mut self.viewer3d {
                v.orbit(orbit_drag, orbit_scroll);
            }
            ctx.request_repaint();
        }
        // Apply 3D-viewer control-bar toggles (#65).
        if ctl_auto || ctl_wire || ctl_light || ctl_reset {
            if let Some(v) = &mut self.viewer3d {
                if ctl_auto {
                    v.auto_orbit = !v.auto_orbit;
                }
                if ctl_wire {
                    v.wireframe = !v.wireframe;
                }
                if ctl_light {
                    v.lighting = (v.lighting + 1) % 3;
                }
                if ctl_reset {
                    v.reset_pose();
                    v.auto_orbit = false;
                }
            }
            ctx.request_repaint();
        }
        // Fullscreen toggle / exit (#65). Also leave fullscreen if the selection is no longer a model.
        if ctl_fullscreen {
            self.viewer_fullscreen = !self.viewer_fullscreen;
        }
        if exit_fullscreen || !show_3d {
            self.viewer_fullscreen = false;
        }
        match asset_action {
            Some(AssetAction::Reanalyze(id)) => self.submit_analyze_asset(id),
            Some(AssetAction::RegenThumb(id)) => self.regen_thumb(id),
            Some(AssetAction::Convert(id, media)) => {
                self.convert_asset = Some((id, media));
                self.convert_format = match media {
                    MediaType::Audio => "wav",
                    _ => "png",
                }
                .to_string();
                self.convert_status = None;
                self.convert_open = true;
            }
            Some(AssetAction::Export(id)) => {
                self.export_assets = vec![id];
                self.export_status = None;
                self.export_open = true;
            }
            Some(AssetAction::Remove(id, name, block)) => {
                self.confirm_remove_asset = Some((id, name, block));
            }
            None => {}
        }
        if let Some(id) = find_sim {
            self.similar_for = Some(id);
            self.similar.clear();
            self.similar_loading = true;
            self.find_similar_asset(id);
        }
        // Selecting a similar/duplicate hit navigates the inspector to it (a plain single-select).
        if let Some(id) = open_similar {
            self.select_asset(id, ClickMods::default(), ctx);
        }
    }
}

impl DamGui {
    /// The list view — a responsive multi-column table (Name · Format · License · Detail · Size)
    /// mirroring the web table. Optional columns drop as the panel narrows so nothing overlaps. The
    /// Name and Size headers are clickable to sort; a click posts the new `SORTS` index via
    /// `sort_click` (the other columns aren't sortable).
    fn list(
        &self,
        ui: &mut egui::Ui,
        click: &mut Option<(AssetId, ClickMods)>,
        activate: &mut Option<(AssetId, MediaType)>,
        menu: &mut Option<AssetAction>,
        sort_click: &mut Option<usize>,
    ) {
        const FORMAT_W: f32 = 56.0;
        const LICENSE_W: f32 = 96.0;
        const DETAIL_W: f32 = 150.0;
        const SIZE_W: f32 = 74.0;
        const ROW_H: f32 = 20.0;
        let w = ui.available_width();
        // Drop optional columns as width shrinks (Name + Size are always shown) so they never collide.
        let show_format = w >= 360.0;
        let show_license = w >= 470.0;
        let show_detail = w >= 620.0;
        let fixed = SIZE_W
            + if show_format { FORMAT_W } else { 0.0 }
            + if show_license { LICENSE_W } else { 0.0 }
            + if show_detail { DETAIL_W } else { 0.0 };
        let name_w = (w - fixed - 8.0).max(80.0);
        let name_end = 4.0 + name_w; // NAME column right edge / start of the optional columns
        let mut cx = name_end;
        let format_x = cx;
        if show_format {
            cx += FORMAT_W;
        }
        let license_x = cx;
        if show_license {
            cx += LICENSE_W;
        }
        let detail_x = cx;
        if show_detail {
            cx += DETAIL_W;
        }
        let size_col_x = cx; // left edge of the (right-aligned) size column
        let font = egui::FontId::proportional(11.0);
        let name_chars = ((name_w - 34.0) / 6.2).max(6.0) as usize; // media tag + ellipsised name

        // Sort state → header carets (Phosphor).
        let name_active = self.sort <= 1;
        let size_active = self.sort == 2 || self.sort == 3;
        let arrow = |asc_idx: usize| {
            if self.sort == asc_idx {
                icon::CARET_UP
            } else {
                icon::CARET_DOWN
            }
        };
        let name_hdr = if name_active {
            format!("NAME  {}", arrow(0))
        } else {
            "NAME".to_string()
        };
        let size_hdr = if size_active {
            format!("SIZE  {}", arrow(3))
        } else {
            "SIZE".to_string()
        };
        let c = colors(ui.visuals().dark_mode);
        let dim = c.fg_dim;
        let fg = c.fg;

        // Paint one table row's cells into `rect` (shared by the header and data rows). `peer`
        // paints a small neutral origin chip after the name for federated results (issue #39).
        let paint_row = |p: &egui::Painter,
                         rect: egui::Rect,
                         tag: &str,
                         tag_col: egui::Color32,
                         name: &str,
                         name_col: egui::Color32,
                         peer: Option<&str>,
                         format: &str,
                         license: &str,
                         detail: Option<&str>,
                         size: &str,
                         size_col: egui::Color32| {
            let y = rect.center().y;
            let l = rect.left();
            // Colour-coded media tag, then the name after it.
            let mut name_x = l + 4.0;
            if !tag.is_empty() {
                let tg = p.layout_no_wrap(tag.to_owned(), font.clone(), tag_col);
                let adv = tg.size().x + 6.0;
                p.galley(egui::pos2(l + 4.0, y - tg.size().y / 2.0), tg, tag_col);
                name_x += adv;
            }
            let ng = p.layout_no_wrap(name.to_owned(), font.clone(), name_col);
            let name_adv = ng.size().x;
            p.galley(egui::pos2(name_x, y - ng.size().y / 2.0), ng, name_col);
            // Peer-origin chip (web PeerBadge parity): tinted pill in the neutral text tone —
            // information, not exposure risk. Skipped when the name column can't fit it.
            if let Some(peer) = peer {
                let pf = egui::FontId::proportional(10.0);
                let pg = p.layout_no_wrap(peer.to_owned(), pf, dim);
                let px = name_x + name_adv + 6.0;
                if px + pg.size().x + 10.0 <= l + name_end {
                    let chip = egui::Rect::from_min_size(
                        egui::pos2(px, y - pg.size().y / 2.0 - 1.0),
                        pg.size() + egui::vec2(10.0, 2.0),
                    );
                    p.rect_filled(chip, 3.0, ui::tint(dim, 0.14));
                    p.galley(egui::pos2(px + 5.0, y - pg.size().y / 2.0), pg, dim);
                }
            }
            if show_format {
                p.text(
                    egui::pos2(l + format_x, y),
                    egui::Align2::LEFT_CENTER,
                    format,
                    font.clone(),
                    dim,
                );
            }
            if show_license {
                p.text(
                    egui::pos2(l + license_x, y),
                    egui::Align2::LEFT_CENTER,
                    license,
                    font.clone(),
                    dim,
                );
            }
            if show_detail {
                if let Some(d) = detail {
                    p.text(
                        egui::pos2(l + detail_x, y),
                        egui::Align2::LEFT_CENTER,
                        d,
                        font.clone(),
                        dim,
                    );
                }
            }
            p.text(
                egui::pos2(rect.right() - 4.0, y),
                egui::Align2::RIGHT_CENTER,
                size,
                font.clone(),
                size_col,
            );
        };

        // ── header (clickable Name / Size) ──
        let (hrect, _) = ui.allocate_exact_size(egui::vec2(w, ROW_H), egui::Sense::hover());
        paint_row(
            &ui.painter_at(hrect),
            hrect,
            "",
            dim,
            &name_hdr,
            if name_active { fg } else { dim },
            None,
            "FORMAT",
            "LICENSE",
            Some("DETAIL"),
            &size_hdr,
            if size_active { fg } else { dim },
        );
        // Two explicit interaction rects for the sortable columns — more reliable than hit-testing a
        // single wide response's pointer position (which can be `None` on release).
        let name_rect = egui::Rect::from_min_max(
            hrect.left_top(),
            egui::pos2(hrect.left() + name_end, hrect.bottom()),
        );
        let size_rect = egui::Rect::from_min_max(
            egui::pos2(hrect.left() + size_col_x, hrect.top()),
            hrect.right_bottom(),
        );
        let sort_name = ui.interact(name_rect, ui.id().with("sort-name"), egui::Sense::click());
        ui::access_label(&sort_name, "Sort by name");
        if sort_name.clicked() {
            *sort_click = Some(if self.sort == 0 { 1 } else { 0 }); // Name A-Z ⇄ Z-A
        }
        let sort_size = ui.interact(size_rect, ui.id().with("sort-size"), egui::Sense::click());
        ui::access_label(&sort_size, "Sort by size");
        if sort_size.clicked() {
            *sort_click = Some(if self.sort == 2 { 3 } else { 2 }); // Largest ⇄ Smallest
        }
        ui.separator();

        // ── rows ──
        for a in &self.assets {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, ROW_H), egui::Sense::click());
            let selected = self.selected == Some(a.id) || self.selection.contains(&a.id);
            // Name/type/size (+ favourite) for the AccessKit tree — the web row's aria-label.
            // Federated-origin attribution (issue #39) — `None` (no chip) for local assets.
            let peer = match &a.origin {
                Origin::Peer(p) => Some(p.as_str()),
                Origin::Local => None,
            };
            ui::access_toggle(
                &resp,
                selected,
                &format!(
                    "{}, {}, {}{}{}",
                    a.name,
                    media_tag(a.media),
                    human_bytes(a.size),
                    if a.favorite { ", favorite" } else { "" },
                    peer.map(|p| format!(", from peer {p}")).unwrap_or_default()
                ),
            );
            if resp.clicked() {
                *click = Some((a.id, click_mods(ui)));
            }
            if resp.double_clicked() {
                *activate = Some((a.id, a.media));
            }
            resp.context_menu(|ui| asset_context_menu(ui, a, menu));
            if !ui.is_rect_visible(rect) {
                continue;
            }
            let p = ui.painter_at(rect);
            if selected {
                p.rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
            } else if resp.hovered() {
                p.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.bg_fill);
            }
            ui::focus_ring(ui, &resp, rect);
            let detail = detail_attr(a).map(|d| ellipsize(&d, 20));
            paint_row(
                &p,
                rect,
                media_tag(a.media),
                ui::media_color(&c, media_value(a.media)),
                // Leave the peer chip room in the fixed name column (chip glyphs are ~10px vs 11px).
                &ellipsize(
                    &a.name,
                    match peer {
                        Some(p) => name_chars.saturating_sub(p.chars().count() + 3).max(6),
                        None => name_chars,
                    },
                ),
                fg,
                peer,
                &a.format.to_uppercase(),
                license_label(a.license.status),
                detail.as_deref(),
                &human_bytes(a.size),
                dim,
            );
        }
    }

    /// The blocklist management surface (issue #21): the removed-and-blocked content hashes, each
    /// with an Unblock button that lets a future scan re-import the content. Mirrors the web
    /// blocklist page.
    fn blocklist_review(&self, ui: &mut egui::Ui, unblock: &mut Option<ContentHash>) {
        ui.add_space(4.0);
        ui.heading(format!("{}  Blocklist", icon::PROHIBIT));
        ui.label(
            egui::RichText::new(
                "Content hashes of assets you removed with \"block\". A scan / watch / auto-rescan \
                 won't re-import these bytes until you unblock them.",
            )
            .small()
            .weak(),
        );
        ui.separator();
        if self.blocklist_loading {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading blocklist…");
            });
            return;
        }
        if self.blocklist.is_empty() {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(
                    "Nothing blocked. Remove an asset with \"Remove + block\" to add its hash here.",
                )
                .weak(),
            );
            return;
        }
        egui::Grid::new("blocklist")
            .num_columns(3)
            .striped(true)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                for e in &self.blocklist {
                    let name = e.label.as_deref().unwrap_or("(unknown)");
                    ui.label(egui::RichText::new(name).small());
                    // Short hash prefix (monospace) — the full 32-byte hash is long.
                    let hex = e.hash.to_string();
                    let short = hex.get(..16).unwrap_or(&hex);
                    ui.label(egui::RichText::new(short).small().monospace().weak());
                    if ui.small_button("Unblock").clicked() {
                        *unblock = Some(e.hash);
                    }
                    ui.end_row();
                }
            });
    }

    /// The dedicated duplicate-review surface (issue #8): kind (exact/near) + media filters, then a
    /// card per linked group (media · count · signal) with a member tile row; the suggested "keep" is
    /// marked. 3DAM only groups — nothing is deleted. Clicking a member opens it in the library;
    /// right-click gives the same per-asset actions as the browser. Mirrors web `Duplicates.tsx`.
    fn dup_review(
        &self,
        ui: &mut egui::Ui,
        member_click: &mut Option<AssetId>,
        menu: &mut Option<AssetAction>,
        pick_kind: &mut Option<DupKind>,
        pick_media: &mut Option<Option<MediaType>>,
    ) {
        ui.add_space(4.0);
        ui.heading(format!("{}  Duplicate review", icon::COPY));
        ui.label(
            egui::RichText::new(
                "Groups the analysis pass linked. 3DAM only groups — nothing is deleted. Each \
                 group marks a suggested Keep; click a member to inspect it.",
            )
            .small()
            .weak(),
        );
        ui.add_space(4.0);

        // Controls: kind (exact/near) toggle + media filter + group count.
        ui.horizontal(|ui| {
            for (k, label) in [(DupKind::Exact, "Exact"), (DupKind::Near, "Near")] {
                if ui.selectable_label(self.dup_kind == k, label).clicked() {
                    *pick_kind = Some(k);
                }
            }
            ui.separator();
            for (m, label) in [
                (None, "All media"),
                (Some(MediaType::Image), "Images"),
                (Some(MediaType::Audio), "Audio"),
                (Some(MediaType::Model), "3D"),
            ] {
                if ui.selectable_label(self.dup_media == m, label).clicked() {
                    *pick_media = Some(m);
                }
            }
            ui.separator();
            ui.label(
                egui::RichText::new(format!("{} groups", self.dup_groups.len()))
                    .small()
                    .weak(),
            );
        });
        ui.separator();

        if self.dup_loading {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Scanning for duplicates…");
            });
            return;
        }
        if self.dup_groups.is_empty() {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(
                    "No duplicates in this view. Run the analysis pass to populate near-duplicate \
                     signals.",
                )
                .weak(),
            );
            return;
        }

        // One card per group.
        for (gi, g) in self.dup_groups.iter().enumerate() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                let c = colors(ui.visuals().dark_mode);
                ui.horizontal(|ui| {
                    ui::media_badge(ui, media_value(g.media));
                    ui.label(
                        egui::RichText::new(format!("{} items", g.members.len()))
                            .size(11.0)
                            .color(c.fg_muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(&g.signal)
                                .size(10.0)
                                .monospace()
                                .color(c.fg_dim),
                        );
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    for m in &g.members {
                        let keep = m.id == g.suggested_keep;
                        let label = format!(
                            "{}{}  {}\n{}",
                            if keep { "[keep] " } else { "" },
                            media_tag(m.media),
                            ellipsize(&m.name, 22),
                            human_bytes(m.size),
                        );
                        let resp = ui.add_sized(
                            egui::vec2(150.0, 40.0),
                            egui::SelectableLabel::new(keep, label),
                        );
                        if resp.clicked() {
                            *member_click = Some(m.id);
                        }
                        resp.context_menu(|ui| asset_context_menu(ui, m, menu));
                    }
                });
            });
            if gi + 1 < self.dup_groups.len() {
                ui.add_space(4.0);
            }
        }
    }

    /// The thumbnail grid — wrapped fixed-size cards, each a server thumbnail (loaded lazily as it
    /// scrolls into view) or a typed placeholder tile for audio / un-rendered 3D.
    fn grid(
        &self,
        ui: &mut egui::Ui,
        click: &mut Option<(AssetId, ClickMods)>,
        activate: &mut Option<(AssetId, MediaType)>,
        to_load: &mut Vec<AssetId>,
        menu: &mut Option<AssetAction>,
    ) {
        const CARD_W: f32 = 150.0;
        const FOOTER_H: f32 = 40.0;
        const CARD_H: f32 = 150.0;
        const TILE_H: f32 = CARD_H - FOOTER_H; // thumbnail area
        let c = colors(ui.visuals().dark_mode);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
            for a in &self.assets {
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(CARD_W, CARD_H), egui::Sense::click());
                let selected = self.selected == Some(a.id) || self.selection.contains(&a.id);
                // Name/type/size (+ favourite) for the AccessKit tree — the web cell's aria-label.
                ui::access_toggle(
                    &resp,
                    selected,
                    &format!(
                        "{}, {}, {}{}",
                        a.name,
                        media_tag(a.media),
                        human_bytes(a.size),
                        if a.favorite { ", favorite" } else { "" }
                    ),
                );
                if resp.clicked() {
                    *click = Some((a.id, click_mods(ui)));
                }
                if resp.double_clicked() {
                    *activate = Some((a.id, a.media));
                }
                resp.context_menu(|ui| asset_context_menu(ui, a, menu));
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                // Lazily request the thumbnail the first time a card is actually on screen.
                if !self.thumbs.contains_key(&a.id) {
                    to_load.push(a.id);
                }

                let painter = ui.painter_at(rect);
                // Bordered surface card (web GridCell): accent border when selected, strong on
                // hover — and the keyboard-focus ring (web `:focus-visible`) doubles as accent.
                let border = if selected || resp.has_focus() {
                    c.accent
                } else if resp.hovered() {
                    c.border_strong
                } else {
                    c.border
                };
                painter.rect(
                    rect,
                    egui::CornerRadius::same(4),
                    c.surface,
                    egui::Stroke::new(1.0, border),
                    egui::StrokeKind::Inside,
                );

                // Thumbnail (or a typed placeholder), inset 1px so the card border stays crisp.
                let tile = egui::Rect::from_min_size(
                    rect.min + egui::vec2(1.0, 1.0),
                    egui::vec2(CARD_W - 2.0, TILE_H - 1.0),
                );
                match self.thumbs.get(&a.id) {
                    Some(Thumb::Ready(tex)) => {
                        let img = egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                            .maintain_aspect_ratio(true)
                            .fit_to_exact_size(tile.size());
                        img.paint_at(ui, tile);
                    }
                    _ => placeholder_tile(&painter, tile, a.media, ui.visuals()),
                }

                // Media badge overlay (top-left) — a legible dark chip with the identity-hue tag.
                let mv = media_value(a.media);
                let mcol = ui::media_color(&c, mv);
                let bfont = egui::FontId::new(10.0, egui::FontFamily::Proportional);
                let bg = painter.layout_no_wrap(ui::media_tag(mv).to_owned(), bfont, mcol);
                let bpad = egui::vec2(5.0, 2.0);
                let brect = egui::Rect::from_min_size(
                    tile.min + egui::vec2(5.0, 5.0),
                    bg.size() + bpad * 2.0,
                );
                painter.rect_filled(
                    brect,
                    egui::CornerRadius::same(3),
                    egui::Color32::from_black_alpha(160),
                );
                painter.galley(brect.min + bpad, bg, mcol);

                // ── Footer: name line, then favourite star + license dot + size ──
                let fx = rect.min.x + 7.0;
                let name_y = rect.min.y + TILE_H + 5.0;
                painter.text(
                    egui::pos2(fx, name_y),
                    egui::Align2::LEFT_TOP,
                    ellipsize(&a.name, 20),
                    egui::FontId::new(12.0, egui::FontFamily::Proportional),
                    c.fg,
                );
                let line2_y = rect.min.y + TILE_H + 22.0;
                let small = egui::FontId::new(10.0, egui::FontFamily::Proportional);
                // Size, right-aligned.
                painter.text(
                    egui::pos2(rect.right() - 7.0, line2_y),
                    egui::Align2::RIGHT_TOP,
                    human_bytes(a.size),
                    small.clone(),
                    c.fg_dim,
                );
                // License dot + label, left.
                let licol = ui::license_color(&c, license_label(a.license.status));
                painter.circle_filled(egui::pos2(fx + 3.0, line2_y + 6.0), 3.0, licol);
                painter.text(
                    egui::pos2(fx + 10.0, line2_y),
                    egui::Align2::LEFT_TOP,
                    license_label(a.license.status),
                    small,
                    c.fg_dim,
                );
                // Favourite star tucked at the far right of the name line.
                if a.favorite {
                    painter.text(
                        egui::pos2(rect.right() - 7.0, name_y),
                        egui::Align2::RIGHT_TOP,
                        icon::STAR,
                        egui::FontId::new(11.0, egui::FontFamily::Proportional),
                        c.warn,
                    );
                }
            }
        });
    }
}

/// Render the full-detail inspector for a selected asset.
fn inspector(
    ui: &mut egui::Ui,
    asset: &Asset,
    thumb: Option<&egui::TextureHandle>,
    tag_review: &mut Option<(AssetId, String, ReviewAction)>,
    action: &mut Option<AssetAction>,
) {
    let s = &asset.summary;
    let c = colors(ui.visuals().dark_mode);
    // "Preview not available" (issue #18): the USD family has no decode path yet, so make the absent
    // preview explicit instead of a silent tile — mirrors the web NoModelPreview card. Metadata is
    // still catalogued below.
    let no_preview = s.media == MediaType::Model
        && matches!(s.format.as_str(), "usd" | "usda" | "usdc" | "usdz");
    if no_preview {
        egui::Frame::group(ui.style())
            .fill(c.surface2)
            .show(ui, |ui| {
                ui.set_width(ui.available_width().min(300.0));
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.label(egui::RichText::new(icon::CUBE).size(24.0).color(c.fg_dim));
                    ui.label(
                        egui::RichText::new("Preview not available")
                            .size(12.0)
                            .color(c.fg_muted),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "3DAM can't render .{} yet — its metadata is still catalogued below.",
                            s.format
                        ))
                        .size(11.0)
                        .color(c.fg_dim),
                    );
                    ui.add_space(18.0);
                });
            });
        ui.add_space(8.0);
    }
    // Preview: the asset's thumbnail (reuses the grid texture) scaled to the panel width. Absent for
    // audio / un-rendered 3D — those just show the metadata below.
    if let Some(tex) = thumb.filter(|_| !no_preview) {
        let w = ui.available_width().min(300.0);
        ui.add(
            egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                .maintain_aspect_ratio(true)
                .corner_radius(4)
                .max_width(w),
        );
        ui.add_space(8.0);
    }
    // Title: media badge (+ peer-origin chip, + favourite star), then the name and the license
    // badge underneath.
    ui.horizontal(|ui| {
        ui::media_badge(ui, media_value(s.media));
        // Federated-origin attribution (issue #39): a small neutral chip naming the peer — web
        // PeerBadge parity. Local assets get nothing (the common case stays chrome-free).
        if let Origin::Peer(peer) = &s.origin {
            ui::pill(ui, peer, c.fg_muted, ui::tint(c.fg_muted, 0.14))
                .on_hover_text(format!("From federated peer \u{201c}{peer}\u{201d}"));
        }
        if s.favorite {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let star = ui.label(egui::RichText::new(icon::STAR).color(c.warn));
                ui::access_text(&star, "Favorite");
            });
        }
    });
    ui.add(
        egui::Label::new(egui::RichText::new(&s.name).strong().color(c.fg).size(14.0)).truncate(),
    )
    .on_hover_text(&s.name);
    ui.add_space(3.0);
    ui::license_badge(ui, license_label(s.license.status));
    ui.add_space(6.0);

    // Per-asset maintenance actions (mirrors the web context menu / inspector actions).
    ui.horizontal_wrapped(|ui| {
        let analyzed = asset.timestamps.analyzed.is_some();
        if ui
            .button(format!(
                "{}  {}",
                icon::SPARKLE,
                if analyzed { "Reanalyze" } else { "Analyze" }
            ))
            .clicked()
        {
            *action = Some(AssetAction::Reanalyze(s.id));
        }
        // Only media with a server thumbnail can be regenerated.
        if matches!(s.media, MediaType::Image | MediaType::Model)
            && ui
                .button(format!("{}  Thumbnail", icon::IMAGE))
                .on_hover_text("Regenerate thumbnail")
                .clicked()
        {
            *action = Some(AssetAction::RegenThumb(s.id));
        }
        // Convert (transcode) — image/audio only; 3D can't transcode in v1.
        if matches!(s.media, MediaType::Image | MediaType::Audio)
            && ui.button(format!("{}  Convert", icon::SWAP)).clicked()
        {
            *action = Some(AssetAction::Convert(s.id, s.media));
        }
        // Remove from the catalog (non-destructive to the file; confirms first). Block is offered in
        // the right-click menu.
        if ui
            .button(format!("{}  Remove", icon::TRASH))
            .on_hover_text("Remove from the catalog (the file is untouched)")
            .clicked()
        {
            *action = Some(AssetAction::Remove(s.id, s.name.clone(), false));
        }
    });

    // ── DETAILS ──
    ui::section_label(ui, "Details");
    ui::meta_row(ui, "Type", media_label(s.media));
    ui::meta_row(ui, "Format", &s.format.to_uppercase());
    ui::meta_row(ui, "Size", &human_bytes(s.size));
    // Origin (issue #39): local vs the federated peer that owns the row — web Inspector parity.
    let origin_label = match &s.origin {
        Origin::Local => "Local".to_string(),
        Origin::Peer(p) => format!("Peer: {p}"),
    };
    ui::meta_row(ui, "Origin", &origin_label);
    // Path: dim label left, truncated path right (full path on hover; left-click copies).
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(egui::RichText::new("Path").color(c.fg_dim).size(12.0))
                .selectable(false),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let resp = ui
                .add(
                    egui::Label::new(egui::RichText::new(&asset.path).color(c.fg).size(12.0))
                        .truncate()
                        .sense(egui::Sense::click()),
                )
                .on_hover_text(format!("{}\n(click to copy)", asset.path));
            ui::access_label(&resp, &format!("Copy path {}", asset.path));
            if resp.clicked() {
                ui.ctx().copy_text(asset.path.clone());
            }
        });
    });

    // Media-specific attributes (audio/image/model), including the analysis-pass extras when present.
    let media = media_rows(&asset.attributes);
    if !media.is_empty() {
        ui::section_label(ui, "Media");
        for (k, v) in &media {
            ui::meta_row(ui, k, v);
        }
    }

    // Continuous analysis features as 0–1 bars (image seamlessness; audio brightness/harmonicity) —
    // the visual analogue of the web inspector's FeatureBar / Seamlessness blocks.
    feature_bars(ui, &asset.attributes);

    if !asset.tags.is_empty() {
        ui::section_label(ui, &format!("Tags ({})", asset.tags.len()));
        // Reject-only lifecycle (tech-spec 05): an auto tag is active (powers search) unless rejected.
        // Active auto tags are accent pills with an × to reject; rejected ones are dim with a restore.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            for t in &asset.tags {
                let auto = t.source == "auto";
                let rejected = t.state == "rejected";
                let (fg, bg) = if rejected {
                    (c.fg_dim, c.surface2)
                } else if auto {
                    (c.accent, ui::tint(c.accent, 0.14))
                } else {
                    (c.fg, c.surface2)
                };
                ui::pill(ui, &t.name, fg, bg);
                if auto {
                    if rejected {
                        let restore = ui
                            .small_button(icon::ARROW_COUNTER_CLOCKWISE)
                            .on_hover_text("Restore tag");
                        ui::access_label(&restore, &format!("Restore tag {}", t.name));
                        if restore.clicked() {
                            *tag_review = Some((s.id, t.name.clone(), ReviewAction::Accept));
                        }
                    } else {
                        let reject = ui.small_button(icon::X).on_hover_text("Reject tag");
                        ui::access_label(&reject, &format!("Reject tag {}", t.name));
                        if reject.clicked() {
                            *tag_review = Some((s.id, t.name.clone(), ReviewAction::Reject));
                        }
                    }
                }
            }
        });
    }
}

/// A typed placeholder tile for assets with no raster thumbnail (audio, un-rendered 3D) or one still
/// loading — a filled panel with the media tag centred, so the grid never shows an empty hole.
fn placeholder_tile(
    painter: &egui::Painter,
    rect: egui::Rect,
    media: MediaType,
    visuals: &egui::Visuals,
) {
    let c = colors(visuals.dark_mode);
    painter.rect_filled(rect, egui::CornerRadius::same(3), c.bg);
    // A large media-identity glyph in a faint tint, so audio / un-rendered 3D still read at a glance.
    let glyph = match media {
        MediaType::Audio => icon::MUSIC_NOTES,
        MediaType::Image => icon::IMAGE,
        MediaType::Model => icon::CUBE,
    };
    let col = ui::media_color(&c, media_value(media));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::new(34.0, egui::FontFamily::Proportional),
        ui::tint(col, 0.55),
    );
}

/// Truncate to `max` chars with an ellipsis (grid card names are one line).
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// The store's `media_type` facet value (matches `MediaType`'s lowercase serde name).
fn media_value(m: MediaType) -> &'static str {
    match m {
        MediaType::Audio => "audio",
        MediaType::Image => "image",
        MediaType::Model => "model",
    }
}

/// Build the inspector's media-attribute rows for the asset's media type — the cheap-tier container
/// facts plus any analysis-pass extras that have been filled in. Empty (no block) when nothing known.
/// Paint the analysis "features" block: 0–1 bars for image seamlessness (+ a coloured tile-class
/// badge) and audio brightness / harmonicity. Mirrors the web inspector's Seamlessness / FeatureBar.
/// No-op until the analyze pass has scored the asset.
fn feature_bars(ui: &mut egui::Ui, attrs: &MediaAttributes) {
    let mut bars: Vec<(&str, f32)> = Vec::new();
    let mut badge: Option<(String, egui::Color32)> = None;
    match attrs {
        MediaAttributes::Image(a) => {
            if let Some(t) = a.tileability {
                bars.push(("Seamlessness", t));
            }
            let tc_colors = colors(ui.visuals().dark_mode);
            badge = a.tile_class.as_deref().map(|tc| {
                let (label, color) = match tc {
                    "seamless" => ("Seamless", tc_colors.ok),
                    "tiled" => ("Tiled", tc_colors.accent),
                    "non_tiling" => ("Non-tiling", tc_colors.fg_dim),
                    other => (other, tc_colors.fg_dim),
                };
                (label.to_string(), color)
            });
        }
        MediaAttributes::Audio(a) => {
            if let Some(b) = a.brightness {
                bars.push(("Brightness", b));
            }
            if let Some(h) = a.harmonicity {
                bars.push(("Harmonicity", h));
            }
        }
        _ => {}
    }
    if bars.is_empty() && badge.is_none() {
        return;
    }
    ui::section_label(ui, "Features");
    if let Some((label, color)) = badge {
        ui.horizontal(|ui| {
            let c = colors(ui.visuals().dark_mode);
            ui.add(
                egui::Label::new(egui::RichText::new("Tiling").color(c.fg_dim).size(11.0))
                    .selectable(false),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui::pill(ui, &label, color, ui::tint(color, 0.16));
            });
        });
    }
    for (label, val) in bars {
        ui::feature_bar(ui, label, val.clamp(0.0, 1.0));
    }
}

fn media_rows(attrs: &MediaAttributes) -> Vec<(String, String)> {
    let mut r: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: String| r.push((k.to_string(), v));
    match attrs {
        MediaAttributes::Audio(a) => {
            if let Some(x) = a.duration_ms {
                push("Duration", duration(x));
            }
            if let Some(x) = a.sample_rate {
                push("Sample rate", format!("{x} Hz"));
            }
            if let Some(x) = a.bit_depth {
                push("Bit depth", format!("{x}-bit"));
            }
            if let Some(x) = a.channels {
                push("Channels", channel_label(x));
            }
            if let Some(x) = &a.codec {
                push("Codec", x.clone());
            }
            if let Some(x) = &a.container {
                push("Container", x.clone());
            }
            if let Some(x) = &a.class {
                push("Type", x.clone());
            }
            if let Some(x) = a.loudness_lufs {
                push("Loudness", format!("{x:.1} LUFS"));
            }
        }
        MediaAttributes::Image(a) => {
            if let (Some(w), Some(h)) = (a.width, a.height) {
                push("Dimensions", format!("{w} × {h}"));
            }
            if let Some(x) = a.color_depth {
                push("Bit depth", format!("{x}-bit"));
            }
            if let Some(x) = a.has_alpha {
                push("Alpha", yesno(x));
            }
            if let Some(x) = &a.color_space {
                push("Color space", x.clone());
            }
            if let Some(x) = &a.tile_class {
                push("Tiling", x.clone());
            }
            if let Some(x) = &a.class {
                push("Type", x.clone());
            }
        }
        MediaAttributes::Model(a) => {
            if let Some(x) = a.vertex_count {
                push("Vertices", group_int(x));
            }
            if let Some(x) = a.triangle_count {
                push("Triangles", group_int(x));
            }
            if let Some(x) = a.mesh_count {
                push("Meshes", x.to_string());
            }
            if let Some(x) = a.material_count {
                push("Materials", x.to_string());
            }
            if let Some(x) = a.texture_count {
                push("Textures", x.to_string());
            }
            if let Some(x) = a.has_rig {
                push("Rigged", yesno(x));
            }
            if let Some(x) = a.has_animation {
                push("Animation", yesno(x));
            }
            if let Some(x) = a.has_uvs {
                push("UVs", yesno(x));
            }
            if let Some(x) = &a.class {
                push("Complexity", x.clone());
            }
        }
        MediaAttributes::None => {}
    }
    r
}

fn yesno(b: bool) -> String {
    if b { "yes" } else { "no" }.to_string()
}

/// Human channel label: 1 → Mono, 2 → Stereo, else "N ch".
fn channel_label(n: i64) -> String {
    match n {
        1 => "Mono".to_string(),
        2 => "Stereo".to_string(),
        _ => format!("{n} ch"),
    }
}

/// `mm:ss` from milliseconds.
fn duration(ms: i64) -> String {
    let secs = (ms.max(0) / 1000) as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Group an integer with thousands separators (12,345).
// `is_multiple_of` (clippy's suggestion) only stabilised in 1.87; MSRV here is 1.85, so keep the `%`.
#[allow(clippy::manual_is_multiple_of)]
fn group_int(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let bytes = digits.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// The store's `license` facet value (matches `LicenseStatus`'s lowercase serde name).
fn license_value(l: LicenseStatus) -> &'static str {
    match l {
        LicenseStatus::Permissive => "permissive",
        LicenseStatus::Attribution => "attribution",
        LicenseStatus::Restricted => "restricted",
        LicenseStatus::Unknown => "unknown",
    }
}

fn media_label(m: MediaType) -> &'static str {
    match m {
        MediaType::Audio => "Audio",
        MediaType::Image => "Image",
        MediaType::Model => "3D Model",
    }
}

fn media_tag(m: MediaType) -> &'static str {
    match m {
        MediaType::Audio => "[AUD]",
        MediaType::Image => "[IMG]",
        MediaType::Model => "[3D ]",
    }
}

/// The right-click context menu for a grid card / list row — the per-asset actions, mirroring the
/// web context menu. Writes the chosen action into `menu` (applied after the panels).
fn asset_context_menu(ui: &mut egui::Ui, a: &AssetSummary, menu: &mut Option<AssetAction>) {
    if ui.button("Analyze").clicked() {
        *menu = Some(AssetAction::Reanalyze(a.id));
        ui.close_menu();
    }
    if matches!(a.media, MediaType::Image | MediaType::Model)
        && ui.button("Regenerate thumbnail").clicked()
    {
        *menu = Some(AssetAction::RegenThumb(a.id));
        ui.close_menu();
    }
    if matches!(a.media, MediaType::Image | MediaType::Audio) && ui.button("Convert…").clicked() {
        *menu = Some(AssetAction::Convert(a.id, a.media));
        ui.close_menu();
    }
    if ui.button("Export…").clicked() {
        *menu = Some(AssetAction::Export(a.id));
        ui.close_menu();
    }
    ui.separator();
    // Remove from the catalog (the file is never touched). "+ block" also blocklists its hash so a
    // rescan can't re-import it. Both confirm first (issue #21).
    if ui.button("Remove…").clicked() {
        *menu = Some(AssetAction::Remove(a.id, a.name.clone(), false));
        ui.close_menu();
    }
    if ui.button("Remove + block…").clicked() {
        *menu = Some(AssetAction::Remove(a.id, a.name.clone(), true));
        ui.close_menu();
    }
}

/// Read the ctrl/cmd + shift modifiers at click time (for multi-select).
fn click_mods(ui: &egui::Ui) -> ClickMods {
    let m = ui.input(|i| i.modifiers);
    ClickMods {
        toggle: m.ctrl || m.command,
        range: m.shift,
    }
}

/// A sensible default manifest path under the user's home directory (falls back to the cwd).
fn default_export_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{home}/3dam-manifest.json")
}

/// A sensible default output directory for conversions (never a source tree — §5.1).
fn default_convert_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{home}/3dam-converted")
}

/// Compact human-readable byte size (1.7 KB, 4.8 MB, …).
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}
