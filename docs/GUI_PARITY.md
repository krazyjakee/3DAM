# Web ↔ native-GUI parity checklist

3DAM ships two user-facing clients (golden rule 1): the **web** client (`web/`, React) and the
**native** GUI (`crates/3dam-gui`, egui/eframe). The web client leads (web-first phasing,
PRODUCT_SPEC §9); this file tracks what the egui shell has reached vs. what it still owes, so a new
user-facing feature lands in **both** — or the gap is recorded here rather than silently forgotten.

**When you add or change a user-facing web feature, update the matching row here** and either
implement the egui equivalent or move it to "Owed" with a note.

Both clients are thin front-ends over the same `LibraryService` seam (`crates/3dam-api`), so parity
is about UI surface, not engine work — the capability already exists behind the trait.

## Status

Legend: ✅ done · 🟡 partial · ⬜ owed (not yet in egui)

| Capability | Web | egui | Notes |
|---|---|---|---|
| Window / app shell | ✅ | ✅ | eframe window, three-region layout + toolbar |
| Text search | ✅ | ✅ | search box → `QueryRequest.text` (Enter / Search button) |
| Media-type filter (audio/image/3D) | ✅ | ✅ | toolbar toggle → `FacetField::MediaType` |
| Browse grid + list | ✅ | ✅ | thumbnail grid + flat list, toggle in the toolbar |
| Thumbnails | ✅ | ✅ | `read_thumbnail` → decode off-thread → egui texture cache; lazy, visible-only; incl. 3D turntable renders (Vulkan) |
| Inspector: core metadata | ✅ | ✅ | name, type, format, size, path, tags |
| Inspector: media attributes | ✅ | 🟡 | audio/image/model attr rows (+ class, loudness, tiling); seamlessness/feature bars not yet visual |
| Inspector: preview (3D/audio/image) | ✅ | ✅ | image + interactive 3D viewer (egui-wgpu, orbit/zoom) + audio play/stop; 3D textures + waveform owed |
| Library stats | ✅ | ✅ | totals + by-media counts in the left rail |
| Sources list + manage | ✅ | ✅ | list/scope + add-local / remove (confirm) / rescan in the rail (SFTP/SMB owed) |
| Folder-tree navigation (#66) | ✅ | ✅ | left-rail source tree, lazy `list_folders`, path-scoped browse (breadcrumb TBD) |
| Search mode (lexical/hybrid/semantic) | ✅ | ✅ | toolbar combo, shown with a text query |
| License facet | ✅ | ✅ | left-rail Permissive/Attribution/Restricted/Unknown |
| Advanced Search (structured attr + tag filters) | ✅ | ✅ | toolbar "Filters" popover: media-contextual enum/numEnum/bool dropdowns + numeric ranges + free tag filter, AND-ed onto the query |
| Quick class facet (audio/image/model class) | ✅ | ✅ | contextual chips in the left rail when a media type is active |
| Favorites / Recently added | ✅ | 🟡 | favourites facet done; "recently added" = the Newest sort |
| Collections & smart folders | ✅ | ✅ | list + browse-by + "+ New" create (manual / smart-from-search) + right-click rename/delete + inspector add/remove membership |
| Grid ⇄ list view toggle | ✅ | 🟡 | grid + flat list; the web "table" (sortable columns) is richer |
| Sort control | ✅ | ✅ | toolbar combo (name/size/scanned, both directions) |
| Tag review (reject/restore) | ✅ | ✅ | reject-only lifecycle in the inspector, refreshes on review |
| Similar / duplicates | ✅ | 🟡 | "Find similar" + exact-dup group (keep marker) in the inspector; dedicated dedup page owed |
| Convert / export | ✅ | ✅ | export-manifest modal + per-asset convert modal (image/audio transcode) |
| Per-asset actions (reanalyze, regen thumbnail) | ✅ | ✅ | inspector buttons over `submit_analyze` + `regenerate_thumbnails` |
| Multi-select + batch actions | ✅ | ✅ | ctrl/shift-click selection + batch bar (analyze / export / clear) |
| Context menus | ✅ | ✅ | right-click grid/list → Analyze / Regen thumbnail / Convert / Export (media-aware) |
| Live updates | ✅ | ✅ | subscribes to the engine event stream; coalesced/throttled refreshes |
| Settings / admin | ✅ | ⬜ | flags, tokens, audit |
| Duplicates page / blocklist page | ✅ | ⬜ | |
| Theme (light/dark) | ✅ | ✅ | dark/light toggle at the foot of the rail (System mode owed) |
| Responsive / touch / a11y pass | ✅ | ⬜ | |
| 3D viewer on-canvas controls (#65) | ✅ | ✅ | orbit/zoom + control bar: auto-orbit · wireframe · lighting cycle (studio/soft/flat) · reset |

## Current egui slice

`crates/3dam-gui` (issue #34): a real eframe window over the embedded `LibraryService` —
**browse** (thumbnail grid or flat list, toggle in the toolbar), **search** (text + media-type
filter, live re-query), **inspect** (per-asset detail), plus library stats and a sources list.
Thumbnails load off-thread (`read_thumbnail` → PNG decode → egui texture), lazily and visible-only,
falling back to a typed tile for audio / un-rendered 3D. The async service runs on a background Tokio
runtime; results reach the frame loop over a channel (no I/O on the UI thread — golden rule 5).
Everything marked ⬜/🟡 above is the follow-up backlog.
