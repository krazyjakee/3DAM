# 12 — Desktop GUI

> **Superseded in implementation by [ADR 0013](../adr/0013-desktop-shell-tauri.md)** (2026-07-16).
> The shipped desktop app is `crates/3dam-desktop`: a **Tauri webview shell** over the embedded
> React client, served by an in-process `dam-server` (loopback, ephemeral port) in embedded mode or
> a remote `3dam serve` in hosted mode. The egui client this spec describes was built to
> substantive parity (see `docs/GUI_PARITY.md` history) and then retired; this file is kept as the
> design record of that client and of the native-toolkit road not taken. The workspace/interaction
> design it encodes (three-region layout, virtualised browser, license-prioritised inspector) lives
> on in the web client ([09](09-server-and-web-client.md)).

Status: **Draft v0.1 (superseded — historical)** · Scope: the native `3dam-gui` shell — toolkit-choice framing, the app-shell/state architecture over a `LibraryService` handle, the three-region workspace, the virtualised grid/table, the license-prioritised inspector, embedding the wgpu 3D viewer and audio/image previews, keyboard navigation, and dark-mode handling.

This file is the low-level design for the **native desktop client** — the `3dam-gui` crate named in [01-architecture-and-crates.md](01-architecture-and-crates.md). It is a thin, GPU-accelerated front-end over the shared engine: it holds a `LibraryService` handle (embedded engine or API client) and does no data work of its own. It turns [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.3/§6.4 (browse/search/preview), §7 (candidate GUI stack), and §8 (60 fps at 100k+, virtualisation) into an implementable shell, and it realises [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §3 (three-region workspace, grid+table equal, license-prioritised inspector) and §4 (dark-first, low-chrome) natively.

It stays inside its border. The `LibraryService` trait, its DTOs, pagination and the live-update stream are **owned by [03-library-service-and-api.md](03-library-service-and-api.md)** — the GUI *consumes* them and never re-specs them. The wgpu viewer's internals (render graph, camera, software raster) are **owned by [06-3d-render.md](06-3d-render.md)** — the GUI *hosts* the widget and describes only the surface handoff. The **web client is [09-server-and-web-client.md](09-server-and-web-client.md)'s**; per the web-first phasing (PRODUCT_SPEC §9) it ships *before* this GUI and settles the interaction patterns, so this file describes the native counterpart that reuses them, not a second design of the same workspace. Off-UI-thread concurrency lives in [14-concurrency-performance-reliability.md](14-concurrency-performance-reliability.md); the derivative cache the grid draws from lives in [02-data-model-and-storage.md](02-data-model-and-storage.md).

---

## 1. Toolkit choice — **egui / eframe** ([ADR 0005](../adr/0005-gui-toolkit-egui.md))

**Decided: `egui`/`eframe`.** The GUI toolkit is settled by [ADR 0005](../adr/0005-gui-toolkit-egui.md) — immediate-mode fits the dense, virtualised, dark tooling UI (100k-row grid/table via `ScrollArea::show_rows`), `egui-wgpu` gives first-class embedding of [06](06-3d-render.md)'s viewer onto the toolkit's own wgpu device, and the ecosystem (`egui_extras`, dev-tool precedent incl. `mogen-studio`) matches the product. The rendering/perf spike below is now a **validation** step, not a gate. The tradeoff table and Iced comparison are retained for the record; where this file says "under Iced …", read it as the road not taken.

The product's demands, in priority order (DESIGN_GUIDELINES §1.1 first):

1. **Virtualised scrolling at 100k+ visible-scale, 60 fps** (PRODUCT_SPEC §8). The centre browser must draw only what is on screen and recycle cheaply.
2. **Information density** — tight grids and sortable tables, low chrome (DESIGN_GUIDELINES §4).
3. **A wgpu surface embedded mid-layout** for the 3D viewer, plus GPU-drawn thumbnails/waveforms.
4. **Full keyboard navigation and OS light/dark** (DESIGN_GUIDELINES §3.5).
5. **Cross-platform** (Linux/Windows/macOS) from one codebase.

| Axis | **egui / eframe** (immediate-mode) | **Iced** (retained, Elm-like) |
|------|-------------------------------------|-------------------------------|
| Paradigm | Immediate mode: rebuild UI each frame. Simple mental model, trivial "draw only visible rows". | Retained + `Message`/`update`/`view`. Explicit state, diffed widget tree. |
| Virtualisation | Natural — you iterate the visible index range yourself and emit widgets; no list-diffing to fight. Mature `ScrollArea` + `show_rows`. | Needs a virtualised/lazy list; retained diffing wants care so 100k rows don't materialise. Doable, more bespoke. |
| Dense tables/grids | Ecosystem tables (`egui_extras::TableBuilder`) exist; dense output is idiomatic. | Clean layout primitives; large sortable tables are more hand-built. |
| wgpu embedding | First-class: `egui-wgpu` + a paint callback hands a wgpu render pass into a rect ([06](06-3d-render.md) plugs in here). | Also wgpu-based; embedding a custom `wgpu` primitive/shader widget is supported but less trodden for a hosted external renderer. |
| Text input / IME / a11y | Improving (`accesskit` integration exists); historically the weaker side for heavy text UIs. | Retained model maps more conventionally to a11y trees; still maturing. |
| Frame cost model | Redraws every frame unless throttled; must gate repaints to stay at idle 0% CPU (see §4). | Redraws on message/state change; idle is naturally cheap. |
| Fit to *this* UI | Strong: a browser that is mostly a virtualised list + panels is exactly immediate-mode's sweet spot. | Strong on structured, event-driven chrome; the virtualised hot path is the thing to prove. |

**Both are viable; the decision is a spike, not a doc.** The blocking evidence is the **rendering/perf spike from PRODUCT_SPEC §10** — build the virtualised grid *and* table at 100k+ rows in each toolkit with the real derivative cache behind it, measure sustained scroll frame time and idle CPU, and embed the [06](06-3d-render.md) viewer in a mid-layout rect in each. The ADR records the pick with those numbers.

**This file is written toolkit-agnostic.** The app-shell, state model, virtualisation strategy, and viewer handoff below are expressed so they hold under either choice; where a snippet must be concrete it uses egui-flavoured pseudocode (the [01](01-architecture-and-crates.md) crate table lists `egui`/`eframe` *or* `iced` with `egui-wgpu`) and flags what would differ under Iced. Nothing here presumes the ADR's outcome.

---

## 2. App-shell architecture

The GUI is a shell around **one `LibraryService` handle**. It never links `3dam-store`, never issues SQL, never decodes a file — it renders state and forwards intent to the engine (DESIGN_GUIDELINES §1.4). The shell has three parts: the **backend handle**, the **UI state tree**, and the **event/update loop** that keeps all heavy work off the UI thread.

### 2.1 The backend handle

Startup resolves a `Box<dyn LibraryService>` exactly as [01](01-architecture-and-crates.md) §4 specifies — `open_backend(Backend::Embedded | Connected)` keyed on `--connect`. The GUI holds it behind an `Arc` so worker tasks can call it concurrently. Whether a call is an in-process function or an HTTP round-trip is invisible above the trait; the GUI code is identical in embedded and connected mode.

```rust
// crate: 3dam-gui
struct App {
    /// The one seam to the engine. Arc so worker tasks share it. (file 03 owns the trait.)
    library: Arc<dyn LibraryService>,
    /// Async runtime handle for spawning off-UI-thread work (file 14).
    rt: tokio::runtime::Handle,
    /// Channel the UI thread drains each frame for results from workers.
    inbox: mpsc::UnboundedReceiver<UiEvent>,
    outbox: mpsc::UnboundedSender<UiEvent>,   // cloned into every spawned task
    /// The whole observable UI state (§2.2).
    ui: UiState,
    /// GPU handles shared with the embedded viewer widget (§6).
    render_ctx: RenderCtx,
}
```

`RenderCtx` carries the wgpu `Device`/`Queue` the toolkit already owns (both candidates run on wgpu) so the [06](06-3d-render.md) viewer renders onto the *same* device — no second GPU context. Detail in §6.

### 2.2 The UI state tree

State is a plain tree the render pass reads and the update loop mutates. It is deliberately *not* the domain model — it holds view state (which region has focus, scroll offset, selection) plus **handles to lazily-loaded engine data** (pages of hits, per-asset detail, thumbnails), each in a load-state so the UI can draw "loading" truthfully rather than block (DESIGN_GUIDELINES §1.1, §3.4).

```rust
struct UiState {
    nav: NavState,          // left region: sources/collections/tags/smart-folders tree (§3.1)
    browser: BrowserState,  // centre region: grid|table, the query, the virtual window (§3.2, §4)
    inspector: InspectorState, // right region: selected asset detail + suggestions (§5)
    query: Query,           // text + facet chips; shared, drives the browser (§3.3)
    selection: Selection,   // set of AssetIds; preserved across grid<->table switch (§3.2)
    theme: Theme,           // resolved dark/light (§7)
    toasts: Vec<Toast>,     // non-blocking progress/error feedback (§2.4)
}

/// Every piece of engine-sourced data the UI draws is one of these.
enum Loadable<T> { Idle, Loading, Ready(T), Failed(Error) }

struct BrowserState {
    view: ViewMode,                 // Grid | Table — user-toggleable (§3.2)
    /// Result set as a *windowed* sparse vector: only loaded pages are resident (§4).
    rows: VirtualRows,              // total_count + resident pages of AssetHit
    scroll: ScrollAnchor,          // stable across view switch and reload
    columns: TableColumns,         // sort key + order for table mode
}
```

`Loadable` is the load-truthful primitive the whole shell is built on: a cell that is `Loading` draws a placeholder skeleton, `Failed` draws an inline error affordance (fail-soft, DESIGN_GUIDELINES §6) — one bad thumbnail never blanks the grid.

### 2.3 The event/update loop — heavy work never on the UI thread

The UI thread does exactly one job: **read `UiState`, paint a frame, translate input into `Intent`s.** It must never `await` a `LibraryService` call, decode a thumbnail, or block on I/O — the 60 fps target (DESIGN_GUIDELINES §1.1) is only reachable if the paint thread never stalls. Everything heavy is spawned onto the async runtime / worker pool ([14](14-concurrency-performance-reliability.md)) and streams results back over a channel the UI drains each frame.

```
   UI THREAD (paint, ~16ms budget)                 WORKERS (tokio/rayon, file 14)
   ┌───────────────────────────────┐               ┌──────────────────────────────┐
   │ 1. drain inbox → apply UiEvent │◄──UiEvent─────│  library.search(q) → Page    │
   │ 2. paint UiState (§4 virtual)  │               │  library.get_asset(id)       │
   │ 3. collect input → Vec<Intent> │──Intent──────►│  fetch_thumb(id) (cache→§02) │
   │ 4. dispatch(Intent) → spawn ───┼───spawn──────►│  incremental page prefetch   │
   └───────────────────────────────┘               └──────────────────────────────┘
        never awaits, never blocks                    all await/CPU lives here
```

```rust
// UI-thread frame (egui-flavoured; Iced folds steps 1/3/4 into update(Message)).
impl App {
    fn frame(&mut self, ctx: &Ctx) {
        // 1. Drain everything workers sent since last frame — cheap, non-blocking.
        while let Ok(ev) = self.inbox.try_recv() { self.apply(ev); }

        // 2. Paint from state only. The virtualised browser (§4) is the hot path.
        let intents = self.ui.paint(ctx, &self.render_ctx);

        // 3+4. Turn user actions into async work. dispatch() SPAWNS; it never awaits.
        for intent in intents { self.dispatch(intent); }
    }

    fn dispatch(&mut self, intent: Intent) {
        let (lib, tx) = (self.library.clone(), self.outbox.clone());
        match intent {
            Intent::RunQuery(q) => { self.ui.browser.rows.reset(); self.spawn_search(q); }
            Intent::LoadPage(range) => self.spawn_page(range),
            Intent::Select(id)      => self.spawn_detail(id),   // feeds the inspector (§5)
            Intent::NeedThumb(id)   => self.spawn_thumb(id),    // grid cell became visible (§4)
            Intent::AcceptTag(id, t)=> self.spawn(async move { lib.tag(id, t).await }, tx),
            // …
        }
    }

    fn spawn_search(&self, q: Query) {
        let (lib, tx) = (self.library.clone(), self.outbox.clone());
        self.rt.spawn(async move {
            // Incremental: stream pages as the engine yields them (file 03 stream, file 14).
            match lib.search(q.into()).await {
                Ok(mut page_stream) => {
                    while let Some(page) = page_stream.next().await {
                        // Each page is a partial result — usable immediately (§1.1).
                        let _ = tx.send(UiEvent::Page(page));
                    }
                }
                Err(e) => { let _ = tx.send(UiEvent::QueryFailed(e)); }
            }
        });
        // ctx.request_repaint() so the UI wakes when results arrive (see §7 repaint gating).
    }
}
```

The load-bearing invariant: **`dispatch` and every `spawn_*` return immediately.** The UI thread's only contact with the engine is (a) draining `inbox` and (b) firing intents. This is the concrete realisation of "the UI never blocks on I/O or analysis" (DESIGN_GUIDELINES §1.1); the worker-pool sizing, cancellation, and back-pressure that make it hold are [14](14-concurrency-performance-reliability.md)'s.

### 2.4 Live updates, cancellation, feedback

- **Live updates.** In connected mode the engine pushes scan/analysis progress and new assets over the [03](03-library-service-and-api.md) WebSocket stream; embedded mode exposes the same stream in-process. A single long-lived task forwards stream items as `UiEvent`s (new asset in current query → splice into `VirtualRows`; progress → a toast), so the browser self-updates without a manual refresh (DESIGN_GUIDELINES §3.3 smart folders are live).
- **Cancellation.** A superseded query (user typed more, changed a facet) drops its result channel and signals cancel; the stale task's sends are ignored and it is cancelled at the engine boundary ([14](14-concurrency-performance-reliability.md)). Scroll-driven prefetches for a range no longer visible are likewise dropped.
- **Feedback.** Long operations surface as non-blocking **toasts** with progress and a cancel affordance; they never freeze the UI (DESIGN_GUIDELINES §3.4). Errors are inline and local (a failed thumbnail, an offline source) — fail-soft, never a modal that halts browsing.

---

## 3. The three-region workspace

The window is the three-region workspace of DESIGN_GUIDELINES §3.1, rendered natively: **left navigation**, **centre browser**, **right inspector**. This is the same layout the web client (file 09) settles first; the GUI reuses those interaction patterns (PRODUCT_SPEC §9).

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│  ⌕ search…            [type▾][license▾][tags▾][format▾]  + facet     [▦ grid][≣ tbl]│  toolbar: query + facets (§3.3) + view toggle (§3.2)
├───────────────┬──────────────────────────────────────────────┬───────────────────┤
│ NAVIGATION    │  BROWSER  (grid ▦  or  table ≣ — toggleable)   │  INSPECTOR        │
│ (left)        │  (centre, virtualised — §4)                    │  (right)          │
│               │                                                │                   │
│ ▾ SOURCES     │  ┌────┐ ┌────┐ ┌────┐ ┌────┐ ┌────┐            │  ┌─────────────┐  │
│   • Local     │  │▦img│ │◭3d │ │∿wav│ │▦img│ │◭3d │            │  │ large       │  │
│   • NAS (SMB) │  └────┘ └────┘ └────┘ └────┘ └────┘            │  │ preview     │  │  §5.1 media preview
│   • studio ⚿  │  ┌────┐ ┌────┐ ┌────┐ ┌────┐ ┌────┐            │  │ (viewer/    │  │  (3D viewer §6,
│ ▾ COLLECTIONS │  │∿wav│ │▦img│ │◭3d │ │▦img│ │∿wav│            │  │  wave/img)  │  │   wave, image)
│   • Props     │  └────┘ └────┘ └────┘ └────┘ └────┘            │  └─────────────┘  │
│ ▾ SMART       │        ▲ only visible cells are built          │  Title.fbx        │
│   • safe-ship │        ▲ thumbnails lazy-load into cells       │  ┌─────────────┐  │
│   • non-tiling│                                                │  │● CC-BY-4.0  │  │  §5.2 LICENSE — high,
│ ▾ TAGS        │  ── OR table mode ─────────────────────        │  │ attribution │  │   colour-coded badge,
│   • metallic  │  Name        Type  License   Tris   BPM        │  │ required    │  │   directly under title
│   • loop      │  brick.png   img   ●CC0      —      —          │  └─────────────┘  │
│               │  step.wav    aud   ●Propr.   —      92         │  Metadata · Features
│               │  crate.fbx   3d    ○Unknown  4,812  —          │  Tags [metallic ✓][rust ?✓✗]  §5.3 accept/reject
└───────────────┴──────────────────────────────────────────────┴───────────────────┘
```

Both side regions are collapsible; the centre browser is never hidden. Region splits are draggable and persisted. On narrow windows the native shell keeps three regions (it is desktop-first); the *responsive collapse to one column* is the web client's tablet/phone concern (file 09), not the desktop GUI's.

### 3.1 Left navigation

A hierarchical tree (the Connecter-style source/category sidebar, DESIGN_GUIDELINES §3, `docs/existing-product-screenshots/`) with four top-level groups, each fed by `LibraryService`:

- **Sources** — file sources (local, SFTP, SMB) and federated peers, each with an online/offline dot (fail-soft: an offline source stays listed with its cached results usable, DESIGN_GUIDELINES §6). Federated peers show a lock/auth glyph.
- **Collections** — manual sets.
- **Smart folders** — saved queries; selecting one *is* running its query (live, self-updating via the §2.4 stream).
- **Tags** — the tag vocabulary; selecting narrows the query.

Selecting a node sets or extends `UiState.query` and fires `Intent::RunQuery`. The tree itself virtualises if a group is large (same mechanism as §4), but is small in the common case.

### 3.2 Centre browser — grid and table are equal

Grid and table are **coequal views of one result set** (DESIGN_GUIDELINES §3.1), toggled from the toolbar. The invariant: **switching view preserves selection *and* filter, and keeps the scroll anchored** — the toggle is a re-render of the same `BrowserState.rows` and `selection`, not a new query.

- **Grid** — thumbnail-first visual scanning: turntable/rendered thumb for 3D, image thumb, waveform mini for audio (content-truthful, DESIGN_GUIDELINES §4). Fixed-aspect cells in a wrapping flow.
- **Table** — dense, sortable, attribute-driven work: Name, Type, **License** (colour dot), plus media-specific columns (Tris/Verts, Dimensions, BPM/Key, Sample rate). Sort is a `LibraryService` re-query on the sort key (the engine sorts; the GUI does not re-sort a partial set), preserving the window.

```rust
fn toggle_view(&mut self, to: ViewMode) {
    // Same rows, same selection, same query — only the projection changes.
    let anchor = self.ui.browser.scroll.current_asset(); // anchor on an AssetId, not a pixel
    self.ui.browser.view = to;
    self.ui.browser.scroll.restore_to(anchor);           // keep the user where they were
    // No RunQuery: rows/selection/query untouched. (DESIGN_GUIDELINES §3.1)
}
```

Selection is a set of `AssetId`s (single click, ctrl/shift multi-select, keyboard range — §7). A single selected asset drives the inspector (§5); a multi-selection enables bulk actions (retag/convert/export) that preview their effect (DESIGN_GUIDELINES §3.4).

### 3.3 Query & facets

One `Query` (text + facet chips) lives in `UiState` and drives the browser regardless of view. Text search is instant and debounced (each keystroke supersedes the prior in-flight search, §2.4). Facets are chips/dropdowns — type, tags, format, source, size, and media-specific facets — that compose; the active set is always visible (DESIGN_GUIDELINES §3.3). **License is a first-class facet** (PRODUCT_SPEC §6.3): "commercial-use allowed", "no attribution required", "redistributable", "unknown license", so a *safe-to-ship* smart folder is one saved query. Facet vocabularies come from `LibraryService` (the GUI does not hardcode license values). "Find similar" (§5) and adding a peer live here too; the query object and its serialization are [03](03-library-service-and-api.md)'s.

---

## 4. Virtualised grid/table — 60 fps at 100k+

The centre browser must sustain 60 fps scrolling a 100k+ result set (PRODUCT_SPEC §8, DESIGN_GUIDELINES §1.1) on a machine that cannot hold 100k thumbnails in VRAM or 100k detail rows in RAM. Two mechanisms, working together: **windowed row virtualisation** (build only visible items) and **lazy, cancellable derivative loading** (fetch only visible thumbnails/metadata, from the [02](02-data-model-and-storage.md) derivative cache).

### 4.1 Windowed rows

The result set is a **sparse windowed vector**: the engine reports `total_count`; the GUI holds only the *pages* overlapping (and a small margin around) the visible range. Off-screen pages are evicted under an LRU cap. This is the same model in grid and table — only the geometry (cells-per-row vs one-row-per-hit) differs.

```rust
struct VirtualRows {
    total: usize,                          // from the engine; the scrollbar is sized to this
    page_size: usize,                      // e.g. 200
    resident: LruMap<PageIdx, Vec<AssetHit>>, // only nearby pages kept
    inflight: HashSet<PageIdx>,            // pages being fetched (don't double-request)
}

impl VirtualRows {
    /// Called each frame with the range the scroll position exposes (+ prefetch margin).
    fn ensure_window(&mut self, visible: Range<usize>, out: &Outbox) {
        for page in pages_covering(&visible, self.page_size) {
            if !self.resident.contains(&page) && self.inflight.insert(page) {
                out.request(Intent::LoadPage(page.range(self.page_size))); // async fetch (§2.3)
            }
        }
        self.resident.evict_beyond(&visible, MARGIN_PAGES);
    }
    fn get(&self, i: usize) -> RowState {
        match self.resident.get(&page_of(i)) {
            Some(hits) => RowState::Ready(&hits[offset_in_page(i)]),
            None       => RowState::Placeholder, // draw a skeleton this frame (§2.2 Loadable)
        }
    }
}
```

The paint loop asks the toolkit for the visible index range and builds **only those items** — immediate-mode makes this direct (`ScrollArea::show_rows(total, |ui, range| …)` in egui); under Iced it is a virtualised/lazy list over the same `total`. A row/cell whose page is not yet resident draws a fixed-size skeleton so layout is stable and the scrollbar never jumps.

### 4.2 Lazy derivative loading

A visible grid cell needs a thumbnail; a visible table row needs its attribute columns. Neither is fetched until the item is on screen, and each is cancellable when it scrolls off:

- **Thumbnails** come from the [02](02-data-model-and-storage.md) **derivative/blob cache** (rendered turntable / image thumb / waveform mini). A visible cell with no resident thumb fires `Intent::NeedThumb(id)`; a worker pulls the cached blob (or triggers generation if absent — [02](02-data-model-and-storage.md)/[06](06-3d-render.md)) and returns bytes the UI uploads to a GPU texture, into a bounded **texture atlas/LRU** so VRAM stays capped regardless of set size. Content-truthful: the real asset's thumb, correct aspect, never a decorative placeholder (DESIGN_GUIDELINES §4).
- **Metadata** for table columns rides in the `AssetHit` page payload (file 03 decides how much projection a hit carries) so a resident page already has the columns; full detail is fetched only on selection (§5).
- **Prefetch margin.** `ensure_window` requests a page or two beyond the visible range in the scroll direction so fast scrolling meets resident pages, not skeletons — bounded so it never becomes a batch load (incremental over batch, DESIGN_GUIDELINES §1.1).
- **Cancellation.** Scrolling a cell off-screen drops its pending thumb request (§2.4); the texture LRU evicts its GPU texture. This keeps both worker queue depth and VRAM bounded at any library size — the mechanism that makes 100k (and 1M+, PRODUCT_SPEC §8) tractable.

The GUI owns *windowing and eviction policy*; the *cache that answers the fetch* is [02](02-data-model-and-storage.md)'s and the *pool that runs the fetch* is [14](14-concurrency-performance-reliability.md)'s.

---

## 5. The inspector

The right region reviews and corrects the selected asset — the single place automated results are seen and fixed (DESIGN_GUIDELINES §3.1). Selecting an asset fires `Intent::Select(id)`; a worker calls `library.get_asset(id)` and streams the detail back (§2.3), so the inspector fills in progressively (title/preview first, features as they arrive) rather than blocking. Order top-to-bottom is deliberate (DESIGN_GUIDELINES §3.1):

### 5.1 Large preview (top)

Media-appropriate, per §6:
- **3D** — the embedded interactive orbit **viewer widget** ([06](06-3d-render.md), §6 below), with poly/vert count, materials, bounds beside it.
- **Image** — thumbnail escalating to full-resolution zoom/pan; format, dimensions, colour space, alpha, dominant colours; the tileability score/classification (PRODUCT_SPEC §5).
- **Audio** — **waveform + scrubbable playback** (§6.3), spectral/feature readouts (BPM, key, loudness), space to hit play.

### 5.2 License — prioritised, directly under the title

**License is shown high, immediately under the asset title, as a colour-coded badge** — never buried in technical metadata (DESIGN_GUIDELINES §3.1, PRODUCT_SPEC §5). This is a hard requirement of the inspector, not a nicety: a user must never ship an asset without having seen what they may do with it.

- A **status-coloured badge** carries the state: permissive / attribution-required / restricted / **unknown**. The one accent is used for "safe" states; **unknown or unverified is styled as exactly that** — a distinct caution tone — and never dressed to look permissive (DESIGN_GUIDELINES §3.1, §4 state-truthful).
- Beside it: the SPDX id (or `Proprietary`/`Custom`/`Unknown`), the key rights (commercial use, modification, redistribution, attribution-required), the attribution string to reproduce, and provenance (where the value came from). Colour semantics come from the engine's rights model, not hardcoded in the GUI.

### 5.3 Metadata, features, tags, source — with one-action accept/reject

Below the license: full metadata, extracted features, the source/provenance, and **tags with a suggested-vs-confirmed distinction**. Every automated suggestion (auto-tag, auto-category, near-duplicate flag) carries a visible **one-action accept / reject** (DESIGN_GUIDELINES §3.4, §1.2) that fires an intent (`Intent::AcceptTag`/`RejectTag`, `set_license`, …) to the engine — reviewable, reversible, explainable. A suggested tag renders distinctly from a confirmed one with inline ✓/✗ affordances. **Find similar** ("more like this", DESIGN_GUIDELINES §3.3) is a first-class inspector action on any asset, firing a similarity query that repopulates the browser.

For a **federated** asset the inspector shows its origin peer and that it is a read-only reference with remote-owned previews (PRODUCT_SPEC §4.4) — fetched on demand and cached, never regenerated locally.

---

## 6. Embedding the 3D viewer and other previews

### 6.1 The 3D viewer surface handoff

The interactive orbit viewer is [06](06-3d-render.md)'s `3dam-render` widget **hosted** in the inspector's preview rect. The GUI does not implement rendering — it hands the renderer a target surface and forwards input. Because both toolkit candidates run on wgpu, the viewer renders onto the **same wgpu device the toolkit already owns** (no second GPU context, no cross-context copy):

```rust
// egui-flavoured: a paint callback hands the toolkit's wgpu pass into the viewer's rect.
fn paint_3d_viewer(&mut self, ui: &mut Ui, asset: AssetId) {
    let (rect, response) = ui.allocate_exact_size(viewer_size, Sense::drag());
    // Forward orbit/pan/zoom input to the renderer's camera (file 06 owns camera math).
    self.viewer.handle_input(&response);
    // Register a wgpu paint callback: file 06 records draw commands into the SAME
    // device/queue (RenderCtx, §2.1) onto a texture the toolkit composites into `rect`.
    ui.painter().add(egui_wgpu::Callback::new_paint_callback(
        rect,
        ViewerCallback { asset, camera: self.viewer.camera(), ctx: self.render_ctx.clone() },
    ));
}
```

The contract with [06](06-3d-render.md):

- **The GUI owns the window, event loop, and the wgpu `Device`/`Queue`** (windowing belongs to `3dam-gui`, never to `3dam-render` — [01](01-architecture-and-crates.md) rule 2, ADR 0002). It passes the device/queue and the target rect/texture.
- **`3dam-render` owns what is drawn** — render graph, camera, mesh upload, the software-raster fallback — and records draw commands into the surface it is handed. It never opens a window or a device of its own.
- **Input** (drag→orbit, scroll→zoom) is captured by the GUI over the rect and forwarded to the renderer's camera; the camera/pick math lives in the render/geometry layer ([06](06-3d-render.md), [01](01-architecture-and-crates.md) Open questions).
- **Lifecycle.** The viewer is instantiated when a 3D asset is selected and torn down (GPU resources released) when selection changes, so only the visible model holds mesh VRAM. Under Iced the same handoff uses a custom `wgpu` primitive/shader widget instead of a paint callback — the *contract* (GUI hands surface + input, render owns pixels) is identical; only the embedding call differs.

### 6.2 Image preview

Image previews are drawn by the GUI directly (no external renderer): the cached thumbnail (from the [02](02-data-model-and-storage.md) cache, §4.2) escalates to a full-resolution decode on zoom, uploaded as a GPU texture with pan/zoom over the rect. Decode happens on a worker, never the UI thread (§2.3).

### 6.3 Audio preview

The waveform is a cached derivative (§4.2, [02](02-data-model-and-storage.md)) drawn into the preview rect with a scrubbable playhead. **Playback** is a native concern the GUI owns via the audio output crate (`cpal`/`rodio`, PRODUCT_SPEC §7) — decode/stream on a worker, transport controls (play/scrub/stop) in the inspector, the playhead synced to the audio clock. This is the one preview with a real-time output device the web client (file 09) handles via the browser instead; on desktop it is native audio out.

---

## 7. Keyboard navigation, shortcuts & dark mode

### 7.1 Keyboard-first

Full keyboard navigation with a shortcut for every high-frequency action (DESIGN_GUIDELINES §3.5) — game-dev tooling is used at speed (DESIGN_GUIDELINES §1.1). Focus moves between the three regions and within the browser grid/table; keyboard drives selection, the inspector, and previews without the mouse.

| Action | Binding (indicative) | Notes |
|--------|----------------------|-------|
| Focus search | `/` or `Ctrl/Cmd+F` | jump to the query box |
| Move selection | arrows / `Tab` | grid = 2-D nav; table = row nav |
| Range / multi-select | `Shift`+arrow, `Ctrl/Cmd`+click | drives bulk actions (§3.2) |
| Toggle grid/table | `Ctrl/Cmd+\` | preserves selection+filter (§3.2) |
| Find similar | `S` | on the selected asset (§5.3) |
| Play/pause (audio) | `Space` | when an audio asset is selected (§6.3) |
| Accept / reject suggestion | `Y` / `N` | one-action on the focused suggestion (§5.3) |
| Focus nav / inspector | `Ctrl/Cmd+1` / `3` | jump between regions |
| Cancel / clear | `Esc` | dismiss overlay, clear a facet, cancel op |

Bindings are indicative and cross-platform (`Ctrl` on Linux/Windows, `Cmd` on macOS). Accessibility integration (`accesskit` for both toolkits) exposes the tree to assistive tech — the weaker side for immediate-mode egui and a spike input (§1). Keyboard focus uses the single restrained accent (DESIGN_GUIDELINES §4).

### 7.2 Dark-first, OS light/dark

**Dark is the default** — the working context for game-dev tooling (DESIGN_GUIDELINES §3.5, §4) — and the GUI **honours the OS light/dark preference**, following it live when the system toggles. Both toolkits expose OS appearance and support custom themes; the shell resolves a `Theme` at startup and on OS-appearance-change events, restyling without a restart.

The visual language is **dark-first, low-chrome** (DESIGN_GUIDELINES §4): content (thumbnails, waveforms, models) is the bright part; the UI recedes; **one restrained accent** for selection, focus, and primary actions. Information-dense but calm — tight grids and tables over whitespace, consistent legible typography. A user override (force dark / force light / follow OS) is persisted alongside window/layout state.

### 7.3 Repaint gating (immediate-mode note)

An immediate-mode toolkit (egui) redraws every frame unless told otherwise; naive it burns CPU at idle, breaking the "UI recedes" calm and wasting battery. The shell must **gate repaints**: paint on input, on a `UiEvent` arriving (worker result, live update), on an active animation (scroll fling, playhead, viewer orbit), and otherwise idle at ~0% CPU. `spawn_*` completions call `request_repaint()` to wake exactly the frame that has new state to show. A retained toolkit (Iced) gets this for free from its message-driven redraw — one axis where the toolkit choice (§1) changes the shell's obligations, called out so the spike measures idle CPU under both.

---

## Open questions

> **Mostly resolved 2026-07-06 in [ADR 0009 §9](../adr/0009-v1-scope-decisions.md)** — view-logic
> sharing (each frontend owns its presentation; no shared view crate), windowed-rows (periodic
> coalesced refresh), and the native-GUI/no-webview stance; GUI toolkit + frontend crate were
> settled earlier ([ADR 0005](../adr/0005-gui-toolkit-egui.md)). **Still open:** native audio
> latency & device handling (`cpal`/`rodio`) — its own small spike.

- ~~**GUI toolkit final choice**~~ — **Decided: egui/eframe** ([ADR 0005](../adr/0005-gui-toolkit-egui.md), 2026-07-06). The §1 rendering/perf spike (virtualised grid+table at 100k+ over the real derivative cache, embedded [06](06-3d-render.md) viewer, sustained scroll frame time *and* idle CPU §7.3) remains as a **validation** follow-up, not a gate on the choice.
- **View-logic sharing between web and desktop** — carried from PRODUCT_SPEC §10 open questions. The web client (file 09) ships first and settles the interaction patterns; how much *presentation* logic (query/facet modelling, virtual-window bookkeeping, selection/keyboard semantics) is worth sharing across the React web UI and this native GUI — versus letting each own its presentation over the shared [03](03-library-service-and-api.md) API — is open. The `LibraryService` seam is shared by construction; the view layer above it may or may not be.
- ~~**Where the frontend-shared helper lives**~~ — **Decided: a small `3dam-frontend` crate**
  (2026-07-06) holds `open_backend`/`Backend` and `classify`/`Role`, so `3dam-gui` reaches the
  backend constructor without linking the whole clap tree. Owned across
  [01](01-architecture-and-crates.md)/[13-cli.md](13-cli.md).
- **Windowed-rows reconciliation with live updates.** When the §2.4 stream splices new assets into a sorted, windowed result set, how insertions/removals reconcile against the engine's ordering without a full re-query (stable positions vs periodic window refresh) needs pinning down against [03](03-library-service-and-api.md)'s pagination/stream contract.
- **Native audio latency & device handling.** §6.3 owns playback via `cpal`/`rodio`; device selection, sample-rate conversion, and scrub latency on the three OSes are a native-only concern with no web-client precedent to inherit — needs its own small spike.

---

See also: [00-overview.md](00-overview.md) · [01-architecture-and-crates.md](01-architecture-and-crates.md) · [03-library-service-and-api.md](03-library-service-and-api.md) · [06-3d-render.md](06-3d-render.md) · [09-server-and-web-client.md](09-server-and-web-client.md) · [14-concurrency-performance-reliability.md](14-concurrency-performance-reliability.md) · [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) · [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md)
