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
    AnalyzeRequest, Asset, AssetId, AssetSummary, EventTopic, FacetField, Filter, FilterOp,
    FilterValue, FolderEntry, FolderListing, JobState, LibraryEvent, LibraryStats, LicenseStatus,
    MediaAttributes, MediaType, Page, PageParams, QueryRequest, ReviewAction, SearchMode, Sort,
    SortDir, SortField, SourceId, SourceInfo, SubscribeRequest, SuggestionReview,
    ThumbnailRegenRequest,
};
use futures::StreamExt;

/// A per-asset maintenance action fired from the inspector (mirrors the web context-menu actions).
enum AssetAction {
    /// Force a re-analysis (embeddings, tileability, auto-tags) — background job.
    Reanalyze(AssetId),
    /// Drop + rebuild the cached preview thumbnail from source.
    RegenThumb(AssetId),
}

/// A source-relative folder-tree node key: which source, and the source-relative prefix (trailing
/// slash, or empty for the source root).
type FolderKey = (SourceId, String);

/// Lazy-loaded state of one folder node's immediate children.
enum FolderState {
    Loading,
    Loaded(Vec<FolderEntry>),
    Failed,
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
    license: Option<LicenseStatus>,
    favorites: bool,
    source_filter: Option<SourceId>,
    /// Folder scope (issue #66): a source-relative path prefix, paired with `source_filter`.
    path: Option<String>,
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
            license: None,
            favorites: false,
            source_filter: None,
            path: None,
            sort: 0,
            mode: SearchMode::Lexical,
            view: View::Grid,
            expanded: HashSet::new(),
            folders: HashMap::new(),
            assets: Vec::new(),
            total: None,
            loading: false,
            selected: None,
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
        };
        // Kick the initial loads against the freshly opened library.
        let egctx = cc.egui_ctx.clone();
        app.load_assets(&egctx);
        app.load_sources(&egctx);
        app.load_stats(&egctx);
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

    /// Run the current search/filter against the engine, off-thread.
    fn load_assets(&mut self, egctx: &egui::Context) {
        self.loading = true;
        let (lib, auth, tx, egctx, req) = (
            self.lib.clone(),
            self.auth.clone(),
            self.tx.clone(),
            egctx.clone(),
            self.build_query(),
        );
        self.rt.spawn(async move {
            let r = lib.query(&auth, req).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Assets(r));
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
        let (lib, auth) = (self.lib.clone(), self.auth.clone());
        self.rt.spawn(async move {
            let req = AnalyzeRequest {
                assets: vec![id],
                force: true,
            };
            let _ = lib.submit_analyze(&auth, req).await;
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
                self.detail = Some(asset);
                self.detail_loading = false;
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

        // Actions gathered while rendering (immutable borrows of self), applied after the panels.
        let mut do_query = false;
        let mut open_asset: Option<AssetId> = None;
        let mut nav = NavActions::default();
        let mut tag_review: Option<(AssetId, String, ReviewAction)> = None;
        let mut asset_action: Option<AssetAction> = None;

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
                });
            });
            ui.add_space(4.0);
        });

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
                ui.label(egui::RichText::new("SOURCES").small().weak());
                if self.sources.is_empty() {
                    ui.label(egui::RichText::new("No sources yet — add one with the CLI.").weak());
                }
                // Each source is an expandable folder tree (issue #66): the disclosure loads its
                // directory tree lazily; clicking the name scopes the browse to that source (or, for
                // a folder, to its subtree via the path-prefix filter).
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
                    });
                    if open {
                        self.folder_level(ui, sid, "", 1, &mut nav);
                    }
                }
            });

        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("Inspector");
                ui.separator();
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
                    inspector(ui, asset, thumb, &mut tag_review, &mut asset_action);
                } else {
                    ui.label(egui::RichText::new("Select an asset to inspect it.").weak());
                }
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
                        self.grid(ui, &mut open_asset, &mut to_load);
                    } else {
                        self.list(ui, &mut open_asset);
                    }
                });
        });

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

        if do_query {
            self.load_assets(ctx);
        }
        for id in to_load {
            self.thumbs.insert(id, Thumb::Loading);
            self.load_thumb(id);
        }
        if let Some(id) = open_asset {
            self.selected = Some(id);
            self.load_detail(id, ctx);
            // Make sure the inspector preview has a thumbnail even if the asset wasn't in view.
            let mut fetch = false;
            self.thumbs.entry(id).or_insert_with(|| {
                fetch = true;
                Thumb::Loading
            });
            if fetch {
                self.load_thumb(id);
            }
        }
        if let Some((id, tag, action)) = tag_review {
            self.review_tag(id, tag, action);
        }
        match asset_action {
            Some(AssetAction::Reanalyze(id)) => self.submit_analyze_asset(id),
            Some(AssetAction::RegenThumb(id)) => self.regen_thumb(id),
            None => {}
        }
    }
}

impl DamGui {
    /// The flat text list (audio-friendly; the web "table" analogue).
    fn list(&self, ui: &mut egui::Ui, open_asset: &mut Option<AssetId>) {
        for a in &self.assets {
            let selected = self.selected == Some(a.id);
            let text = format!(
                "{}   {}   ·   {}   ·   {}",
                media_tag(a.media),
                a.name,
                a.format.to_uppercase(),
                human_bytes(a.size),
            );
            if ui.selectable_label(selected, text).clicked() {
                *open_asset = Some(a.id);
            }
        }
    }

    /// The thumbnail grid — wrapped fixed-size cards, each a server thumbnail (loaded lazily as it
    /// scrolls into view) or a typed placeholder tile for audio / un-rendered 3D.
    fn grid(
        &self,
        ui: &mut egui::Ui,
        open_asset: &mut Option<AssetId>,
        to_load: &mut Vec<AssetId>,
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
                    *open_asset = Some(a.id);
                }
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                // Lazily request the thumbnail the first time a card is actually on screen.
                if !self.thumbs.contains_key(&a.id) {
                    to_load.push(a.id);
                }

                let painter = ui.painter_at(rect);
                let selected = self.selected == Some(a.id);
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
