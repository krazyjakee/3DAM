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

Legend: ✅ done · 🟡 partial · ⬜ owed (not yet in egui) · ▫ n/a (out of scope for the native embedded desktop app)

| Capability | Web | egui | Notes |
|---|---|---|---|
| Window / app shell | ✅ | ✅ | eframe window, three-region layout + toolbar |
| Text search | ✅ | ✅ | search box → `QueryRequest.text` (Enter / Search button) |
| Media-type filter (audio/image/3D) | ✅ | ✅ | toolbar toggle → `FacetField::MediaType` |
| Browse grid + list | ✅ | ✅ | thumbnail grid + flat list, toggle in the toolbar |
| Thumbnails | ✅ | ✅ | `read_thumbnail` → decode off-thread → egui texture cache; lazy, visible-only; incl. 3D turntable renders (Vulkan) |
| Inspector: core metadata | ✅ | ✅ | name, type, format, size, path, tags |
| Inspector: media attributes | ✅ | ✅ | audio/image/model attr rows + a FEATURES block: image seamlessness bar + tile-class badge, audio brightness/harmonicity 0–1 bars |
| Inspector: preview (3D/audio/image) | ✅ | ✅ | image + interactive 3D viewer (egui-wgpu, orbit/zoom, **base-colour-textured** lambert) + audio waveform + play/stop |
| Library stats | ✅ | ✅ | totals + by-media counts in the left rail |
| Sources list + manage | ✅ | ✅ | list/scope + add-local / remove (confirm) / rescan in the rail (SFTP/SMB owed) |
| Folder-tree navigation (#66) | ✅ | ✅ | left-rail source tree, lazy `list_folders`, path-scoped browse (breadcrumb TBD) |
| Search mode (lexical/hybrid/semantic) | ✅ | ✅ | toolbar combo, shown with a text query |
| License facet | ✅ | ✅ | left-rail Permissive/Attribution/Restricted/Unknown |
| Advanced Search (structured attr + tag filters) | ✅ | ✅ | toolbar "Filters" popover: media-contextual enum/numEnum/bool dropdowns + numeric ranges + free tag filter, AND-ed onto the query |
| Quick class facet (audio/image/model class) | ✅ | ✅ | contextual chips in the left rail when a media type is active |
| Favorites / Recently added | ✅ | ✅ | favourites facet + a "Recently added" rail shortcut (newest-first sort) |
| Collections & smart folders | ✅ | ✅ | list + browse-by + "+ New" create (manual / smart-from-search) + right-click rename/delete + inspector add/remove membership |
| Grid ⇄ list view toggle | ✅ | ✅ | grid + a responsive multi-column table (Name·Format·License·Detail·Size); clickable Name/Size sort headers, columns drop as the panel narrows |
| Sort control | ✅ | ✅ | toolbar combo (name/size/scanned, both directions) |
| Tag review (reject/restore) | ✅ | ✅ | reject-only lifecycle in the inspector, refreshes on review |
| Similar / duplicates | ✅ | ✅ | "Find similar" + exact-dup group in the inspector, plus a dedicated Duplicate-review view (exact/near + media filters, per-group keep marker, member→library nav) |
| Convert / export | ✅ | ✅ | export-manifest modal + per-asset convert modal (image/audio transcode) |
| Per-asset actions (reanalyze, regen thumbnail) | ✅ | ✅ | inspector buttons over `submit_analyze` + `regenerate_thumbnails` |
| Multi-select + batch actions | ✅ | ✅ | ctrl/shift-click selection + batch bar (analyze / export / clear) |
| Context menus | ✅ | ✅ | right-click grid/list → Analyze / Regen thumbnail / Convert / Export (media-aware) |
| Live updates | ✅ | ✅ | subscribes to the engine event stream; coalesced/throttled refreshes |
| Connect to a remote server (hosted mode, #70/#74) | ✅ | ✅ | both clients can point at an arbitrary `3dam serve`: native via `--connect`/`--token` **and** an in-app Connect dialog (status chip, recent servers, runtime backend switch, offline/reconnect); web via the status-bar Connect dialog (base URL + token, persisted). Same-origin / embedded defaults unchanged. |
| Server-provided waveform peaks (#73) | ✅ | ✅ | both draw the inspector waveform from the analysis pass's stored peak array (no client-side audio decode); fall back to a local decode when an asset isn't analysed yet |
| Prefetch hint (#72) | ✅ | ✅ | each loaded grid page calls `LibraryService::prefetch` to warm the server's thumbnail/preview cache ahead of the per-tile HTTP fetch (ADR 0012) |
| Settings / admin | ✅ | ▫ | flags/tokens/audit live in `server.db` (owned by `dam-server`). With hosted-mode connect (#70) the native GUI *can* now reach a running server, so a connected-mode admin surface is a genuine follow-up — but in the default embedded mode there is still nothing to administer. |
| Duplicates page / blocklist page | ✅ | ✅ | Duplicate-review view + a Blocklist management view (unblock); remove / remove+block actions in the inspector & context menus |
| Theme (light/dark) | ✅ | ✅ | rail selector cycles System / Dark / Light (`ThemePreference`; System follows the OS) |
| Theme palette matches web (#68) | ✅ | ✅ | custom egui `Visuals` built from the web `@theme` tokens (`theme.rs`) — bg/surface ladder, sky accent, text tiers; visual tuning may follow the sweep |
| Responsive / touch / a11y pass | ✅ | ▫ | The web pass targets the mobile/`coarse:` case (single-column collapse, drawers, 44px touch targets) — not applicable to a resizable desktop window (the three-region layout uses resizable panels instead). a11y: eframe ships AccessKit screen-reader support (enable the `accesskit` feature to turn it on); widgets already carry text labels / hover text. |
| 3D viewer on-canvas controls (#65) | ✅ | ✅ | orbit/zoom + control bar: auto-orbit · wireframe · lighting cycle (studio/soft/flat) · reset · fullscreen (Esc to exit) |

## State of the egui client

`crates/3dam-gui` (issue #34) is now a comprehensive eframe client over the embedded
`LibraryService`, at parity with the web app on every substantive surface: the three-region
workspace (toolbar · left rail · browser · inspector), text/media/mode search, browse as a
thumbnail grid **or** a sortable multi-column table, the full inspector (metadata + analysis
feature bars + tags with reject/restore + an interactive **base-colour-textured** 3D viewer with
orbit/zoom and the #65 control bar + audio waveform & playback), Advanced Search, license/class/
favorites/recently-added facets, folder-tree navigation, Collections CRUD + membership, per-asset
actions (reanalyze / regen-thumbnail / convert / export / remove / remove+block), multi-select +
batch actions, context menus, a duplicate-review view + a blocklist view, live updates over the
engine event stream, and a web-matched theme (`theme.rs`) with a System/Dark/Light selector.

The async service runs on a background Tokio runtime; results reach the frame loop over a channel
(no I/O on the UI thread — golden rule 5), and click-driven mutations are gathered under the panel
borrow then applied after (collect-then-apply). The rows marked ▫ above are out of scope for the
native embedded desktop app (server-admin, mobile-responsive/touch); everything else is ✅.
