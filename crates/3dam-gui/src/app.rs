//! The browse / search / inspect workspace (tech-spec 12 §B) — a three-region layout mirroring the
//! web client: a left rail (library stats + sources), a centre asset list, and a right inspector.
//!
//! ## Async bridge
//! `LibraryService` is async; egui's `update` is a synchronous per-frame call. So every service call
//! is `spawn`ed onto the background Tokio runtime, and its result is sent back over an `mpsc` channel
//! that `update` drains at the top of each frame — the frame never blocks on I/O (golden rule 5).
//! The worker calls `Context::request_repaint()` when it posts a result, so the UI wakes exactly when
//! there's something new to show rather than polling.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use eframe::egui;

use dam_api::service::{AuthContext, LibraryService};
use dam_api::{
    Asset, AssetId, AssetSummary, FacetField, Filter, FilterOp, FilterValue, LibraryStats,
    MediaType, Page, PageParams, QueryRequest, SortField, SourceInfo,
};

/// The most assets to pull into the list at once. The GUI is a single-page browse for now (infinite
/// scroll / pagination is an owed parity item); a generous cap keeps a typical library one request.
const LIST_LIMIT: u32 = 500;

/// A result posted from the background runtime back to the UI thread. Variants differ in size
/// (a full `Asset` vs a small stats struct), but each is posted at most once per user action, so the
/// enum size is irrelevant here — not worth boxing every payload.
#[allow(clippy::large_enum_variant)]
enum Msg {
    Assets(Result<Page<AssetSummary>, String>),
    Detail(Result<Asset, String>),
    Sources(Result<Vec<SourceInfo>, String>),
    Stats(Result<LibraryStats, String>),
}

pub struct DamGui {
    rt: Arc<tokio::runtime::Runtime>,
    lib: Arc<dyn LibraryService>,
    auth: AuthContext,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    // ── view state ──
    search: String,
    media_filter: Option<MediaType>,
    assets: Vec<AssetSummary>,
    total: Option<u64>,
    loading: bool,
    selected: Option<AssetId>,
    detail: Option<Asset>,
    detail_loading: bool,
    sources: Vec<SourceInfo>,
    stats: Option<LibraryStats>,
    error: Option<String>,
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
            tx,
            rx,
            search: String::new(),
            media_filter: None,
            assets: Vec::new(),
            total: None,
            loading: false,
            selected: None,
            detail: None,
            detail_loading: false,
            sources: Vec::new(),
            stats: None,
            error: None,
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
        let text = self.search.trim();
        QueryRequest {
            text: (!text.is_empty()).then(|| text.to_string()),
            filters,
            sort: dam_api::Sort {
                field: SortField::Name,
                dir: dam_api::SortDir::Asc,
            },
            page: PageParams {
                after: None,
                limit: LIST_LIMIT,
            },
            include_facets: false,
            mode: dam_api::SearchMode::Lexical,
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
                match self.total {
                    Some(t) => ui.label(format!("{} of {}", self.assets.len(), t)),
                    None => ui.label(format!("{}", self.assets.len())),
                };
                if self.loading {
                    ui.spinner();
                }
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
                    }
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
                            open_asset = Some(a.id);
                        }
                    }
                });
        });

        if do_query {
            self.load_assets(ctx);
        }
        if let Some(id) = open_asset {
            self.selected = Some(id);
            self.load_detail(id, ctx);
        }
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
