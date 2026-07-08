//! The browse / search / inspect workspace (tech-spec 12 §B) — a three-region layout mirroring the
//! web client: a left rail (library stats + sources), a centre asset list, and a right inspector.
//!
//! ## Async bridge
//! `LibraryService` is async; egui's `update` is a synchronous per-frame call. So every service call
//! is `spawn`ed onto the background Tokio runtime, and its result is sent back over an `mpsc` channel
//! that `update` drains at the top of each frame — the frame never blocks on I/O (golden rule 5).
//! The worker calls `Context::request_repaint()` when it posts a result, so the UI wakes exactly when
//! there's something new to show rather than polling.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use eframe::egui;

use dam_api::service::{AuthContext, LibraryService};
use dam_api::{
    Asset, AssetId, AssetSummary, FacetField, Filter, FilterOp, FilterValue, LibraryStats,
    LicenseStatus, MediaType, Page, PageParams, QueryRequest, SearchMode, Sort, SortDir, SortField,
    SourceInfo,
};

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
    sort: usize, // index into SORTS
    mode: SearchMode,
    view: View,
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
            sort: 0,
            mode: SearchMode::Lexical,
            view: View::Grid,
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
        };
        // Kick the initial loads against the freshly opened library.
        let egctx = cc.egui_ctx.clone();
        app.load_assets(&egctx);
        app.load_sources(&egctx);
        app.load_stats(&egctx);
        app
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
        }
    }
}

impl eframe::App for DamGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain everything the worker posted since the last frame.
        while let Ok(msg) = self.rx.try_recv() {
            self.apply(msg);
        }

        // Actions gathered while rendering (immutable borrows of self), applied after the panels.
        let mut do_query = false;
        let mut open_asset: Option<AssetId> = None;

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
                for s in &self.sources {
                    ui.label(format!("{}  ({})", s.name, s.stats.asset_count));
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
                    inspector(ui, asset);
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
fn inspector(ui: &mut egui::Ui, asset: &Asset) {
    let s = &asset.summary;
    ui.label(egui::RichText::new(&s.name).strong());
    egui::Grid::new("detail").num_columns(2).show(ui, |ui| {
        row(ui, "Type", media_label(s.media));
        row(ui, "Format", &s.format.to_uppercase());
        row(ui, "Size", &human_bytes(s.size));
        row(ui, "Path", &asset.path);
        ui.end_row();
    });

    if !asset.tags.is_empty() {
        ui.separator();
        ui.label(
            egui::RichText::new(format!("TAGS ({})", asset.tags.len()))
                .small()
                .weak(),
        );
        ui.horizontal_wrapped(|ui| {
            for t in &asset.tags {
                ui.label(egui::RichText::new(&t.name).small());
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
