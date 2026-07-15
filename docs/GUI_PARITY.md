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
| Inspector: preview (3D/audio/image) | ✅ | 🟡 | image + interactive 3D viewer (egui-wgpu, orbit/zoom, **base-colour-textured** lambert) + audio waveform + play/stop. Owed vs web: audio pause/seek/playhead/time readout (egui is play/stop only, no position tracking); full-res image zoom/pan viewer (#17 — egui paints the cached thumbnail, no zoom/1:1); tile preview cube/grid (#58) |
| Double-click to activate / play audio (#52) | ✅ | ✅ | double-click a grid card or table row → inspector focus; audio starts playing immediately |
| Library stats | ✅ | ✅ | totals + by-media counts in the left rail |
| Sources list + manage | ✅ | ✅ | list/scope + add-local / remove (confirm) / rescan in the rail (SFTP/SMB owed) |
| Folder-tree navigation (#66) | ✅ | ✅ | left-rail source tree, lazy `list_folders`, path-scoped browse + breadcrumb over the Browser (click to re-scope up) |
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
| Front-door auth (token gate) | ✅ | ✅ | one gate at the connection, permissions after it: web blocks the whole UI behind a full-screen token sign-in on token-mode servers (anonymous mode = read-only browse + StatusBar "Sign in" chip; auth off = no gate) and flips back to the gate on a mid-session 401; native verifies a remote connect with a stats probe and reopens the Connect dialog with a "token required/rejected" notice on 401, at connect time and mid-session |
| Server-provided waveform peaks (#73) | ✅ | ✅ | both draw the inspector waveform from the analysis pass's stored peak array (no client-side audio decode); fall back to a local decode when an asset isn't analysed yet |
| Prefetch hint (#72) | ✅ | ✅ | each loaded grid page calls `LibraryService::prefetch` to warm the server's thumbnail/preview cache ahead of the per-tile HTTP fetch (ADR 0012) |
| Settings / admin | ✅ | ▫ | flags/tokens/audit live in `server.db` (owned by `dam-server`). With hosted-mode connect (#70) the native GUI *can* now reach a running server, so a connected-mode admin surface is a genuine follow-up — but in the default embedded mode there is still nothing to administer. |
| Duplicates page / blocklist page | ✅ | ✅ | Duplicate-review view + a Blocklist management view (unblock); remove / remove+block actions in the inspector & context menus |
| Theme (light/dark) | ✅ | ✅ | rail selector cycles System / Dark / Light (`ThemePreference`; System follows the OS) |
| Theme palette matches web (#68) | ✅ | ✅ | custom egui `Visuals` built from the web `@theme` tokens (`theme.rs`) — bg/surface ladder, sky accent, text tiers; visual tuning may follow the sweep |
| Responsive / touch pass | ✅ | ▫ | The web pass targets the mobile/`coarse:` case (single-column collapse, drawers, 44px touch targets) — not applicable to a resizable desktop window (the three-region layout uses resizable panels instead). |
| Accessibility (#44) | ✅ | ✅ | AccessKit enabled (`accesskit` eframe feature → AT-SPI/UIA/NSAccessibility): custom-painted rows/cards/badges report name+role+state via `widget_info`, icon-glyph buttons carry explicit names, selects/inputs are labelled, painted widgets draw the accent focus ring. See `docs/a11y-contrast.md` §Native GUI. |
| 3D viewer on-canvas controls (#65) | ✅ | ✅ | orbit/zoom + control bar: auto-orbit · wireframe · lighting cycle (studio/soft/flat) · reset · fullscreen (Esc to exit) |
| Federation (#39): add a federated peer source | ✅ | ✅ | web AddSourceDialog kind + native rail add form (Folder/Peer toggle): endpoint (`3dam://host:7878` or `http(s)://…`) + optional bearer token (rides in `options.password`); engine validates the peer at add time, errors surface inline. No scan/rescan/folder-tree for peers — they contribute merged catalog rows, not bytes; the sources list renders them with a globe glyph + neutral "peer" chip |
| Federation (#39): peer-origin attribution | ✅ | ✅ | small neutral chip naming the peer on table rows (web `PeerBadge` / native painted chip in the name column) + inspector title chip and an "Origin" detail row; local assets stay chrome-free. Grid tiles skipped in both clients (too noisy) |
| Federation (#39): partial-results notice | ✅ | ✅ | when a query page comes back `partial.complete == false`, a slim warn-tinted strip above the browser — "Some sources didn't answer — results may be partial", naming the `peer_dropped` peers. A degradation notice, not an error |

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
