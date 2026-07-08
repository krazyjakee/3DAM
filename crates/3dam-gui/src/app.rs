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
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use eframe::egui;

use dam_api::service::{AuthContext, LibraryService};
use dam_api::{
    AddSource, AnalyzeRequest, Asset, AssetId, AssetSummary, Collection, CollectionId,
    CollectionKind, CollectionMembers, CollisionRule, ConvertReport, ConvertRequest, ConvertTarget,
    DupGroup, DupKind, DupRequest, EventTopic, ExportFormat, ExportReport, ExportRequest,
    FacetField, Filter, FilterOp, FilterValue, FolderEntry, FolderListing, JobState, LibraryEvent,
    LibraryStats, LicenseStatus, MediaAttributes, MediaType, NewCollection, Page, PageParams,
    QueryRequest, RemoveSource, ReviewAction, ScanMode, ScanRequest, SearchMode, SimilarHit,
    SimilarRequest, Sort, SortDir, SortField, SourceId, SourceInfo, SourceKind, SubscribeRequest,
    SuggestionReview, ThumbnailRegenRequest, UpdateCollection,
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
    /// A live change from the engine's event stream (scan/analyze/convert, source state, …).
    Event(LibraryEvent),
    Similar(AssetId, Result<Vec<SimilarHit>, String>),
    Collections(Result<Vec<Collection>, String>),
    Export(Result<ExportReport, String>),
    Duplicates(Result<Vec<DupGroup>, String>),
    Convert(Result<ConvertReport, String>),
    /// Raw bytes of an audio asset fetched for inspector playback.
    AudioBytes(AssetId, Result<Vec<u8>, String>),
    /// The DMSH preview-mesh blob for a model asset (interactive 3D viewer).
    ModelMesh(AssetId, Result<Vec<u8>, String>),
    /// A collection create/rename/delete/membership mutation finished (reload on success).
    CollectionMutated(Result<(), String>),
}

/// A collection CRUD / membership action gathered while rendering, applied after the panels.
enum CollectionAction {
    Create { name: String, smart: bool },
    Rename { id: CollectionId, name: String },
    Delete(CollectionId),
    AddMember { id: CollectionId, asset: AssetId },
    RemoveMember { id: CollectionId, asset: AssetId },
}

pub struct DamGui {
    rt: Arc<tokio::runtime::Runtime>,
    lib: Arc<dyn LibraryService>,
    auth: AuthContext,
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
    /// Dark vs light theme (dark-first, DESIGN_GUIDELINES §4).
    dark: bool,
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
    sort: usize, // index into SORTS
    mode: SearchMode,
    view: View,
    /// Which folder-tree nodes are expanded, and the lazily-fetched children of the open ones.
    expanded: HashSet<FolderKey>,
    folders: HashMap<FolderKey, FolderState>,
    assets: Vec<AssetSummary>,
    total: Option<u64>,
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
    add_path: String,
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
    audio_error: Option<String>,
    /// Interactive 3D preview renderer (present only on the wgpu backend). `!Send`; UI-thread only.
    viewer3d: Option<crate::viewer3d::Viewer3d>,
}

impl DamGui {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: Arc<tokio::runtime::Runtime>,
        lib: Arc<dyn LibraryService>,
        auth: AuthContext,
    ) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = Self {
            rt,
            lib,
            auth,
            egui_ctx: cc.egui_ctx.clone(),
            tx,
            rx,
            search: String::new(),
            media_filter: None,
            class: None,
            license: None,
            favorites: false,
            dark: true,
            source_filter: None,
            path: None,
            collection: None,
            collections: Vec::new(),
            collection_new_open: false,
            collection_new_name: String::new(),
            collection_new_smart: false,
            collection_rename: None,
            duplicates: Vec::new(),
            sort: 0,
            mode: SearchMode::Lexical,
            view: View::Grid,
            expanded: HashSet::new(),
            folders: HashMap::new(),
            assets: Vec::new(),
            total: None,
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
            add_path: String::new(),
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
            audio_error: None,
            viewer3d: cc
                .wgpu_render_state
                .as_ref()
                .map(crate::viewer3d::Viewer3d::new),
        };
        // Kick the initial loads against the freshly opened library.
        let egctx = cc.egui_ctx.clone();
        app.load_assets(&egctx);
        app.load_sources(&egctx);
        app.load_stats(&egctx);
        app.load_collections(&egctx);
        app.load_duplicates(&egctx);
        app.spawn_events();
        app
    }

    /// Subscribe to the engine's event stream and forward each change to the UI thread (live
    /// updates). The events are coalesced into throttled refreshes in `update` — this task just
    /// pumps them across the channel and wakes the frame loop.
    fn spawn_events(&self) {
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
        });
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
        egui::ComboBox::from_id_salt(("adve", label))
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
        egui::ComboBox::from_id_salt(("advn", label))
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
        egui::ComboBox::from_id_salt(("advb", label))
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

        ui.label(if unit.is_empty() {
            label.to_string()
        } else {
            format!("{label} ({unit})")
        });
        // Commit when either box loses focus (tab/click away/Enter) — the standard egui pattern.
        let mut commit = false;
        ui.horizontal(|ui| {
            let r1 = ui.add(
                egui::TextEdit::singleline(&mut lo)
                    .desired_width(56.0)
                    .hint_text("min"),
            );
            ui.label("–");
            let r2 = ui.add(
                egui::TextEdit::singleline(&mut hi)
                    .desired_width(56.0)
                    .hint_text("max"),
            );
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
        ui.label(egui::RichText::new("Tags").weak());
        let mut add = false;
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.adv_tag_input)
                    .hint_text("Add a tag filter, then Enter")
                    .desired_width(200.0),
            );
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
                    ui.label("Name");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.collection_new_name)
                            .hint_text("Collection name")
                            .desired_width(220.0),
                    );
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
    /// the scan refresh the rail + grid live.
    fn add_local_source(&self, path: String) {
        let (lib, auth) = (self.lib.clone(), self.auth.clone());
        self.rt.spawn(async move {
            let req = AddSource {
                kind: SourceKind::LocalFs,
                uri: path,
                name: None,
                options: Default::default(),
            };
            if let Ok(id) = lib.add_source(&auth, req).await {
                let _ = lib
                    .submit_scan(
                        &auth,
                        ScanRequest {
                            sources: vec![id],
                            mode: ScanMode::Full,
                        },
                    )
                    .await;
            }
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

    /// Fold a posted result into view state.
    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Assets(Ok(page)) => {
                self.total = page.total;
                self.assets = page.items;
                self.loading = false;
                self.error = None;
            }
            Msg::Assets(Err(e)) => {
                self.loading = false;
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
            }
            Msg::Detail(Err(e)) => {
                self.detail_loading = false;
                self.error = Some(format!("Couldn't load asset: {e}"));
            }
            Msg::Sources(Ok(s)) => self.sources = s,
            Msg::Sources(Err(e)) => self.error = Some(format!("Couldn't list sources: {e}")),
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
            Msg::Convert(Ok(rep)) => {
                self.convert_status = Some(Ok(format!(
                    "{} done, {} failed, {} unsupported → {}",
                    rep.done, rep.failed, rep.unsupported, rep.output_dir
                )));
            }
            Msg::Convert(Err(e)) => self.convert_status = Some(Err(e)),
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
                        if ui.small_button(if open { "v" } else { ">" }).clicked() {
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
        // Drain everything the worker posted since the last frame.
        while let Ok(msg) = self.rx.try_recv() {
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
                ui.label("🔍");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search assets…")
                        .desired_width(260.0),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    do_query = true;
                }
                if ui.button("Search").clicked() {
                    do_query = true;
                }
                ui.separator();
                for (label, val) in [
                    ("All", None),
                    ("Audio", Some(MediaType::Audio)),
                    ("Images", Some(MediaType::Image)),
                    ("3D", Some(MediaType::Model)),
                ] {
                    if ui
                        .selectable_label(self.media_filter == val, label)
                        .clicked()
                    {
                        self.media_filter = val;
                        self.class = None; // class is media-specific
                        do_query = true;
                    }
                }
                ui.separator();

                // Sort preset (mirrors the web sort control).
                egui::ComboBox::from_id_salt("sort")
                    .selected_text(SORTS[self.sort].0)
                    .show_ui(ui, |ui| {
                        for (i, (label, _, _)) in SORTS.iter().enumerate() {
                            if ui.selectable_label(self.sort == i, *label).clicked() {
                                self.sort = i;
                                do_query = true;
                            }
                        }
                    });

                // Search-mode selector — only meaningful with a text query, so it appears with one
                // (semantic-search M5: hybrid/semantic widen with embedding neighbours of the hits).
                if !self.search.trim().is_empty() {
                    let mode_label = match self.mode {
                        SearchMode::Lexical => "Keywords",
                        SearchMode::Hybrid => "Keywords + similar",
                        SearchMode::Semantic => "Most similar",
                    };
                    egui::ComboBox::from_id_salt("mode")
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
                }

                ui.separator();
                // Advanced Search: structured attribute + tag filters (contextual to the media type).
                let adv_label = if self.adv.is_empty() {
                    "Filters".to_string()
                } else {
                    format!("Filters ({})", self.adv.len())
                };
                if ui
                    .selectable_label(self.adv_open || !self.adv.is_empty(), adv_label)
                    .on_hover_text("Advanced structured & tag filters")
                    .clicked()
                {
                    self.adv_open = !self.adv_open;
                }
                ui.separator();
                match self.total {
                    Some(t) => ui.label(format!("{} of {}", self.assets.len(), t)),
                    None => ui.label(format!("{}", self.assets.len())),
                };
                if self.loading {
                    ui.spinner();
                }
                // View toggle (grid / list) — right-aligned like the web toolbar.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .selectable_label(self.view == View::List, "List")
                        .clicked()
                    {
                        self.view = View::List;
                    }
                    if ui
                        .selectable_label(self.view == View::Grid, "Grid")
                        .clicked()
                    {
                        self.view = View::Grid;
                    }
                    ui.separator();
                    // Export the current view (collection or faceted query) as a manifest.
                    if ui.button("Export").clicked() {
                        self.export_assets.clear(); // export the view, not a stale selection
                        self.export_status = None;
                        self.export_open = true;
                    }
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
                    ui.label(
                        egui::RichText::new(format!("{} selected", self.selection.len())).strong(),
                    );
                    ui.separator();
                    if ui.button("Analyze").clicked() {
                        batch_analyze = true;
                    }
                    if ui.button("Export").clicked() {
                        batch_export = true;
                    }
                    ui.separator();
                    if ui.button("Clear").clicked() {
                        batch_clear = true;
                    }
                });
                ui.add_space(3.0);
            });
        }

        egui::SidePanel::left("nav")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("3DAM");
                if let Some(stats) = &self.stats {
                    ui.label(format!("{} assets", stats.total));
                    ui.separator();
                    ui.label(egui::RichText::new("LIBRARY").small().weak());
                    for (media, label) in [
                        (MediaType::Audio, "Audio"),
                        (MediaType::Image, "Images"),
                        (MediaType::Model, "3D Models"),
                    ] {
                        let n = stats.by_media.get(media_value(media)).copied().unwrap_or(0);
                        ui.label(format!("{label}: {n}"));
                    }
                }

                // Favorites facet (composes with media/license/text).
                if ui.selectable_label(self.favorites, "★ Favorites").clicked() {
                    self.favorites = !self.favorites;
                    do_query = true;
                }

                // Analysis-class quick facet — contextual to a single active media type (the full
                // typed-attribute set is Advanced Search, still owed). Chips toggle a class filter.
                if let Some(m) = self.media_filter {
                    let (_, options) = class_facet(m);
                    ui.horizontal_wrapped(|ui| {
                        for (val, label) in options {
                            let on = self.class.as_deref() == Some(*val);
                            if ui.selectable_label(on, *label).clicked() {
                                self.class = if on { None } else { Some((*val).to_string()) };
                                do_query = true;
                            }
                        }
                    });
                }

                ui.separator();
                ui.label(egui::RichText::new("LICENSE").small().weak());
                for (lic, label) in LICENSES {
                    let on = self.license == Some(*lic);
                    if ui.selectable_label(on, *label).clicked() {
                        self.license = if on { None } else { Some(*lic) };
                        do_query = true;
                    }
                }

                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("SOURCES").small().weak());
                    if ui.small_button("+").clicked() {
                        nav.toggle_add = true;
                    }
                });
                // Add-source input (local filesystem path). SFTP/SMB (credentials) stay owed.
                if self.add_open {
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.add_path)
                                .hint_text("/path/to/assets")
                                .desired_width(150.0),
                        );
                        let submit = ui.small_button("Add").clicked()
                            || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                        if submit && !self.add_path.trim().is_empty() {
                            nav.source =
                                Some(SourceAction::AddLocal(self.add_path.trim().to_string()));
                        }
                    });
                }
                if self.sources.is_empty() {
                    ui.label(egui::RichText::new("No sources yet — add one above.").weak());
                }
                // Each source is an expandable folder tree (issue #66): the disclosure loads its
                // directory tree lazily; clicking the name scopes the browse to that source (or, for
                // a folder, to its subtree via the path-prefix filter). Trailing controls rescan/remove.
                for s in &self.sources {
                    let sid = s.id;
                    let open = self.expanded.contains(&(sid, String::new()));
                    let scoped = self.source_filter == Some(sid) && self.path.is_none();
                    ui.horizontal(|ui| {
                        if ui.small_button(if open { "v" } else { ">" }).clicked() {
                            nav.toggle.push((sid, String::new()));
                        }
                        if ui
                            .selectable_label(
                                scoped,
                                format!("{} ({})", s.name, s.stats.asset_count),
                            )
                            .clicked()
                        {
                            nav.scope = Some((sid, None));
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
                            if ui.small_button("scan").clicked() {
                                nav.source = Some(SourceAction::Rescan(sid));
                            }
                            if ui.small_button("x").clicked() {
                                nav.set_confirm = Some(Some(sid));
                            }
                        }
                    });
                    if open {
                        self.folder_level(ui, sid, "", 1, &mut nav);
                    }
                }

                // Collections & smart folders — clicking one browses its members (mutually exclusive
                // with the facets); the header "+ New" creates one, and right-click renames/deletes
                // (mirrors the web Collections section). Membership editing lives in the inspector.
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("COLLECTIONS").small().weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("+ New")
                            .on_hover_text("Create a collection")
                            .clicked()
                        {
                            self.collection_new_name.clear();
                            self.collection_new_smart = false;
                            self.collection_new_open = true;
                        }
                    });
                });
                if self.collections.is_empty() {
                    ui.label(
                        egui::RichText::new("Group assets into a collection")
                            .small()
                            .weak(),
                    );
                }
                for c in &self.collections {
                    let on = self.collection == Some(c.id);
                    let smart = matches!(c.kind, CollectionKind::Smart);
                    let suffix = if smart { " ~smart" } else { "" };
                    let label = match c.count {
                        Some(n) => format!("{} ({n}){suffix}", c.name),
                        None => format!("{}{suffix}", c.name),
                    };
                    let resp = ui.selectable_label(on, label);
                    if resp.clicked() {
                        pick_collection = Some(if on { None } else { Some(c.id) });
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Rename…").clicked() {
                            open_rename = Some((c.id, c.name.clone()));
                            ui.close_menu();
                        }
                        if ui.button("Delete").clicked() {
                            collection_action = Some(CollectionAction::Delete(c.id));
                            ui.close_menu();
                        }
                    });
                }

                // Theme toggle at the foot of the rail (dark-first).
                ui.separator();
                let label = if self.dark {
                    "Theme: Dark"
                } else {
                    "Theme: Light"
                };
                if ui.button(label).clicked() {
                    self.dark = !self.dark;
                    ui.ctx().set_visuals(if self.dark {
                        egui::Visuals::dark()
                    } else {
                        egui::Visuals::light()
                    });
                }
            });

        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("Inspector");
                ui.separator();
                egui::ScrollArea::vertical()
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
                                        .selectable_label(v_auto, "Auto-orbit")
                                        .on_hover_text("Spin the model continuously")
                                        .clicked()
                                    {
                                        ctl_auto = true;
                                    }
                                    if ui
                                        .selectable_label(v_wire, "Wireframe")
                                        .on_hover_text("Show mesh edges")
                                        .clicked()
                                    {
                                        ctl_wire = true;
                                    }
                                    let light_name = match v_light {
                                        0 => "Light: Studio",
                                        1 => "Light: Soft",
                                        _ => "Light: Flat",
                                    };
                                    if ui
                                        .button(light_name)
                                        .on_hover_text("Cycle lighting mode")
                                        .clicked()
                                    {
                                        ctl_light = true;
                                    }
                                    if ui.button("Reset").on_hover_text("Reset view").clicked() {
                                        ctl_reset = true;
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

                            // Audio preview: play/stop the selected audio asset (rodio).
                            if asset.summary.media == MediaType::Audio {
                                let aid = asset.summary.id;
                                let playing = self.audio_for == Some(aid);
                                ui.horizontal(|ui| {
                                    if playing {
                                        if ui.button("Stop").clicked() {
                                            audio_stop = true;
                                        }
                                    } else if ui.button("Play").clicked() {
                                        audio_play = Some(aid);
                                    }
                                    if let Some(err) = &self.audio_error {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(0xef, 0x44, 0x44),
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
                                ui.label(egui::RichText::new("COLLECTIONS").small().weak());
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
                                ui.separator();
                                ui.label(
                                    egui::RichText::new(format!(
                                        "DUPLICATES ({})",
                                        g.members.len() - 1
                                    ))
                                    .small()
                                    .weak(),
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
                            ui.label(egui::RichText::new("SIMILAR").small().weak());
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

        // Assets whose thumbnails are worth fetching this frame (visible + not yet requested),
        // gathered under the immutable render borrow and kicked off afterwards.
        let mut to_load: Vec<AssetId> = Vec::new();

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(err) = &self.error {
                ui.colored_label(egui::Color32::from_rgb(0xef, 0x44, 0x44), err);
                ui.separator();
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.assets.is_empty() && !self.loading {
                        ui.add_space(12.0);
                        ui.label(egui::RichText::new("No assets match.").weak());
                    } else if self.view == View::Grid {
                        self.grid(ui, &mut grid_click, &mut to_load, &mut asset_action);
                    } else {
                        self.list(ui, &mut grid_click, &mut asset_action);
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
                        ui.label("Output:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.export_path).desired_width(320.0),
                        );
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
                            ui.colored_label(egui::Color32::from_rgb(0x4a, 0xde, 0x80), msg);
                        }
                        Some(Err(msg)) => {
                            ui.colored_label(
                                egui::Color32::from_rgb(0xef, 0x44, 0x44),
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
                            ui.label("Output dir:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.convert_output)
                                    .desired_width(300.0),
                            );
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
                                ui.colored_label(egui::Color32::from_rgb(0x4a, 0xde, 0x80), msg);
                            }
                            Some(Err(msg)) => {
                                ui.colored_label(
                                    egui::Color32::from_rgb(0xef, 0x44, 0x44),
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
    /// The flat text list (audio-friendly; the web "table" analogue).
    fn list(
        &self,
        ui: &mut egui::Ui,
        click: &mut Option<(AssetId, ClickMods)>,
        menu: &mut Option<AssetAction>,
    ) {
        for a in &self.assets {
            let selected = self.selected == Some(a.id) || self.selection.contains(&a.id);
            let text = format!(
                "{}   {}   ·   {}   ·   {}",
                media_tag(a.media),
                a.name,
                a.format.to_uppercase(),
                human_bytes(a.size),
            );
            let resp = ui.selectable_label(selected, text);
            if resp.clicked() {
                *click = Some((a.id, click_mods(ui)));
            }
            resp.context_menu(|ui| asset_context_menu(ui, a, menu));
        }
    }

    /// The thumbnail grid — wrapped fixed-size cards, each a server thumbnail (loaded lazily as it
    /// scrolls into view) or a typed placeholder tile for audio / un-rendered 3D.
    fn grid(
        &self,
        ui: &mut egui::Ui,
        click: &mut Option<(AssetId, ClickMods)>,
        to_load: &mut Vec<AssetId>,
        menu: &mut Option<AssetAction>,
    ) {
        const TILE: f32 = 128.0;
        const CARD_W: f32 = TILE;
        const CARD_H: f32 = TILE + 26.0; // tile + a two-ish-line label strip
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            for a in &self.assets {
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(CARD_W, CARD_H), egui::Sense::click());
                if resp.clicked() {
                    *click = Some((a.id, click_mods(ui)));
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
                let selected = self.selected == Some(a.id) || self.selection.contains(&a.id);
                if selected {
                    painter.rect_filled(rect, 4.0, ui.visuals().selection.bg_fill);
                } else if resp.hovered() {
                    painter.rect_filled(rect, 4.0, ui.visuals().widgets.hovered.bg_fill);
                }

                let tile = egui::Rect::from_min_size(rect.min, egui::vec2(TILE, TILE));
                match self.thumbs.get(&a.id) {
                    Some(Thumb::Ready(tex)) => {
                        let img = egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                            .maintain_aspect_ratio(true)
                            .fit_to_exact_size(egui::vec2(TILE, TILE));
                        img.paint_at(ui, tile);
                    }
                    _ => placeholder_tile(&painter, tile, a.media, ui.visuals()),
                }

                // One-line, ellipsised name under the tile.
                let name_pos = rect.min + egui::vec2(2.0, TILE + 3.0);
                painter.text(
                    name_pos,
                    egui::Align2::LEFT_TOP,
                    ellipsize(&a.name, 18),
                    egui::FontId::proportional(11.0),
                    ui.visuals().text_color(),
                );
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
    // Preview: the asset's thumbnail (reuses the grid texture) scaled to the panel width. Absent for
    // audio / un-rendered 3D — those just show the metadata below.
    if let Some(tex) = thumb {
        let w = ui.available_width().min(300.0);
        ui.add(
            egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                .maintain_aspect_ratio(true)
                .max_width(w),
        );
        ui.add_space(6.0);
    }
    ui.label(egui::RichText::new(&s.name).strong());

    // Per-asset maintenance actions (mirrors the web context menu / inspector actions).
    ui.horizontal(|ui| {
        let analyzed = asset.timestamps.analyzed.is_some();
        if ui
            .button(if analyzed { "Reanalyze" } else { "Analyze" })
            .clicked()
        {
            *action = Some(AssetAction::Reanalyze(s.id));
        }
        // Only media with a server thumbnail can be regenerated.
        if matches!(s.media, MediaType::Image | MediaType::Model)
            && ui.button("Regenerate thumbnail").clicked()
        {
            *action = Some(AssetAction::RegenThumb(s.id));
        }
        // Convert (transcode) — image/audio only; 3D can't transcode in v1.
        if matches!(s.media, MediaType::Image | MediaType::Audio) && ui.button("Convert").clicked()
        {
            *action = Some(AssetAction::Convert(s.id, s.media));
        }
    });

    egui::Grid::new("detail").num_columns(2).show(ui, |ui| {
        row(ui, "Type", media_label(s.media));
        row(ui, "Format", &s.format.to_uppercase());
        row(ui, "Size", &human_bytes(s.size));
        row(ui, "Path", &asset.path);
        ui.end_row();
    });

    // Media-specific attributes (audio/image/model), including the analysis-pass extras when present.
    let media = media_rows(&asset.attributes);
    if !media.is_empty() {
        ui.separator();
        ui.label(egui::RichText::new("MEDIA").small().weak());
        egui::Grid::new("media").num_columns(2).show(ui, |ui| {
            for (k, v) in &media {
                row(ui, k, v);
            }
            ui.end_row();
        });
    }

    if !asset.tags.is_empty() {
        ui.separator();
        ui.label(
            egui::RichText::new(format!("TAGS ({})", asset.tags.len()))
                .small()
                .weak(),
        );
        // Reject-only lifecycle (tech-spec 05): an auto tag is active (powers search) unless rejected.
        // Active auto tags offer "reject"; rejected ones show struck-through with "restore". User tags
        // are static.
        for t in &asset.tags {
            let auto = t.source == "auto";
            let rejected = t.state == "rejected";
            ui.horizontal(|ui| {
                let mut label = egui::RichText::new(&t.name).small();
                if rejected {
                    label = label.strikethrough().weak();
                }
                ui.label(label);
                if auto {
                    if rejected {
                        if ui.small_button("restore").clicked() {
                            *tag_review = Some((s.id, t.name.clone(), ReviewAction::Accept));
                        }
                    } else if ui.small_button("reject").clicked() {
                        *tag_review = Some((s.id, t.name.clone(), ReviewAction::Reject));
                    }
                }
            });
        }
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
    painter.rect_filled(rect, 3.0, visuals.extreme_bg_color);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        media_tag(media),
        egui::FontId::proportional(13.0),
        visuals.weak_text_color(),
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

fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(egui::RichText::new(label).weak());
    ui.label(value);
    ui.end_row();
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
