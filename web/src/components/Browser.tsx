import type React from "react";
import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ChevronRight,
  CloudOff,
  FileCog,
  FileDown,
  Layers,
  LayoutGrid,
  Loader2,
  Menu,
  Rows3,
  Search,
  Sparkles,
  Star,
  X,
} from "lucide-react";
import {
  useAnalyze,
  useAssets,
  useCan,
  useCollectionMembers,
  useCollections,
  useDuplicates,
  useSetFavorite,
  useSources,
} from "@/api/queries";
import { api } from "@/api/client";
import type { AssetSummary, DupGroup, SearchMode, SortField } from "@/api/types";
import { AUTH_COPY } from "@/lib/auth";
import { useWriteGate } from "@/lib/write-gate";
import { useViewState } from "@/lib/view-state";
import { useDebounced } from "@/lib/use-debounced";
import { bytes } from "@/lib/format";
import { requestAutoplay } from "@/lib/audio-intent";
import { Thumbnail } from "./Thumbnail";
import { LicenseBadge } from "./LicenseBadge";
import { MediaBadge } from "./MediaBadge";
import { PeerBadge } from "./PeerBadge";
import { ContextMenu, useLongPress, type MenuState } from "./ContextMenu";
import { ExportDialog } from "./ExportDialog";
import { ConvertDialog } from "./ConvertDialog";
import { AdvancedSearch } from "./AdvancedSearch";
import { Centered } from "@/lib/ui";

/** Modifier keys that change what a click does to the multi-selection (issue #10/#22). */
export interface ClickMods {
  meta: boolean; // ctrl/cmd → toggle one
  shift: boolean; // shift → extend a range from the anchor
}

const CELL_W = 150; // grid cell target width (px); actual columns computed from container
const CELL_H = 132;
// Table row height. Virtualization drives the height in JS, so `coarse:` CSS can't reach it — bump
// to a 44px touch target on coarse pointers instead (issue #31). Pointer type is stable per session.
const COARSE_POINTER =
  typeof window !== "undefined" && window.matchMedia("(pointer: coarse)").matches;
const ROW_H = COARSE_POINTER ? 44 : 30;
// Pull enough exact-duplicate groups to collapse the whole loaded library (default server cap is 100).
const DUP_LIMIT = 10_000;

/** Collapse byte-identical duplicates in the browse list (issue: dedup in grid/table). Each exact
 *  group renders once — the first member that appears in the current sort/filter represents it, so a
 *  group never vanishes and the visible order is preserved — carrying a badge count of the *other*
 *  copies (library-wide). Near-duplicates are deliberately left expanded: they're merely similar
 *  (surfaced via "Find similar" / the Duplicates page), so collapsing them would hide distinct assets. */
function collapseExactDuplicates(
  items: AssetSummary[],
  groups: DupGroup[] | undefined,
): { visible: AssetSummary[]; dupCounts: Map<string, number> } {
  const memberToGroup = new Map<string, DupGroup>();
  for (const g of groups ?? []) {
    if (g.members.length < 2) continue;
    for (const m of g.members) memberToGroup.set(m.id, g);
  }
  if (memberToGroup.size === 0) return { visible: items, dupCounts: new Map() };

  const seen = new Set<DupGroup>();
  const visible: AssetSummary[] = [];
  const dupCounts = new Map<string, number>();
  for (const a of items) {
    const g = memberToGroup.get(a.id);
    if (!g) {
      visible.push(a);
      continue;
    }
    if (seen.has(g)) continue; // an earlier member already represents this group
    seen.add(g);
    visible.push(a);
    dupCounts.set(a.id, g.members.length - 1);
  }
  return { visible, dupCounts };
}

export function Browser({ onOpenNav }: { onOpenNav?: () => void }) {
  const { state, patch, request } = useViewState();
  // Debounce the *derived* search text so the field stays instant but `/query` only refetches once
  // typing settles (issue #33). The other facets apply immediately; only free-text is debounced.
  const debouncedText = useDebounced(state.q, 300);
  const searchReq = useMemo(
    () => ({ ...request, text: debouncedText.trim() || null }),
    [request, debouncedText],
  );
  const assets = useAssets(searchReq, state.collection);
  // A search is pending while the field's text hasn't yet been applied to the query.
  const searching = state.q.trim() !== debouncedText.trim();
  const [menu, setMenu] = useState<MenuState | null>(null);
  // Multi-selection lives here (Browser-local, not the URL): the ids to batch-act on. Distinct from
  // the single Inspector focus (`state.selected`). `anchor` is the pivot for shift-range.
  const [selection, setSelection] = useState<Set<string>>(new Set());
  const [anchor, setAnchor] = useState<string | null>(null);

  const items = useMemo(
    () => assets.data?.pages.flatMap((p) => p.items) ?? [],
    [assets.data],
  );
  const total = assets.data?.pages[0]?.total ?? null;
  const byId = useMemo(() => new Map(items.map((a) => [a.id, a])), [items]);

  // Federation fan-out (issue #39): a query page comes back `partial.complete === false` when a
  // peer missed the merge deadline — the list under-represents the federated library, a degradation
  // rather than an error. Any loaded page being partial flags the view; the dropped peer names come
  // from the per-page `peer_dropped` warnings. Null ⇒ every page was complete, no notice.
  const droppedPeers = useMemo(() => {
    let incomplete = false;
    const names = new Set<string>();
    for (const p of assets.data?.pages ?? []) {
      if (p.partial?.complete === false) {
        incomplete = true;
        for (const w of p.partial.warnings ?? [])
          if (w.code === "peer_dropped") names.add(w.subject);
      }
    }
    return incomplete ? [...names] : null;
  }, [assets.data]);

  // Prefetch hint (issue #72): as each page loads, ask the server to warm that page's thumbnails +
  // preview meshes so the grid's HTTP fetches hit cache. Fire-and-forget — bytes still come over
  // HTTP/2 (ADR 0012); this only moves generation ahead of render. Keyed on page count so it fires
  // once per fetched page, not on every cache invalidation.
  const pageCount = assets.data?.pages.length ?? 0;
  useEffect(() => {
    const page = assets.data?.pages[pageCount - 1];
    if (!page || page.items.length === 0) return;
    void api.prefetch({ assets: page.items.map((a) => a.id) }).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pageCount]);

  // Collapse byte-identical duplicates into one row each, badged with the hidden-copy count; the
  // full group is listed in the Inspector. Whole-library groups, cached + shared with the Inspector
  // and the Duplicates page under `qk.duplicates`.
  const dups = useDuplicates({ kind: "exact", limit: DUP_LIMIT });
  const { visible, dupCounts } = useMemo(
    () => collapseExactDuplicates(items, dups.data),
    [items, dups.data],
  );

  const clearSelection = useCallback(() => setSelection(new Set()), []);
  const selectAll = useCallback(() => setSelection(new Set(visible.map((a) => a.id))), [visible]);
  const selectedAssets = useMemo(
    () => [...selection].map((id) => byId.get(id)).filter((a): a is AssetSummary => !!a),
    [selection, byId],
  );
  // Convert targets set from the context menu (single/multi); rendered as a dialog at Browser root.
  const [convertTargets, setConvertTargets] = useState<AssetSummary[] | null>(null);

  // Click semantics: plain = single-select + inspect; ctrl/cmd = toggle; shift = extend range from
  // the anchor. Every click also sets the Inspector focus so the detail panel tracks the last click.
  const onItemClick = useCallback(
    (asset: AssetSummary, mods: ClickMods) => {
      const id = asset.id;
      patch({ selected: id });
      if (mods.shift && anchor) {
        const ids = visible.map((a) => a.id);
        const a = ids.indexOf(anchor);
        const b = ids.indexOf(id);
        if (a >= 0 && b >= 0) {
          const [lo, hi] = a < b ? [a, b] : [b, a];
          const range = ids.slice(lo, hi + 1);
          setSelection((prev) => new Set([...prev, ...range]));
        }
      } else if (mods.meta) {
        setSelection((prev) => {
          const next = new Set(prev);
          next.has(id) ? next.delete(id) : next.add(id);
          return next;
        });
        setAnchor(id);
      } else {
        setSelection(new Set([id]));
        setAnchor(id);
      }
    },
    [anchor, visible, patch],
  );

  // Double-click / double-tap = activate: focus the asset in the Inspector and, for audio, start
  // playback immediately (issue #52). Non-audio just opens in the Inspector's viewer.
  const onItemActivate = useCallback(
    (asset: AssetSummary) => {
      patch({ selected: asset.id });
      if (asset.media === "audio") requestAutoplay(asset.id);
    },
    [patch],
  );

  // Right-click / long-press targets the whole selection when the clicked item is part of a
  // multi-selection; otherwise just that item (issue #22).
  const openMenu = useCallback(
    (asset: AssetSummary, x: number, y: number) => {
      const targetIds =
        selection.has(asset.id) && selection.size > 1 ? [...selection] : [asset.id];
      const targets = targetIds.map((id) => byId.get(id)).filter((a): a is AssetSummary => !!a);
      setMenu({ assets: targets.length ? targets : [asset], x, y });
    },
    [selection, byId],
  );

  // Keyboard: Ctrl/Cmd+A selects all, Escape clears — but never while typing in the search box.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const el = document.activeElement;
      if (el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT"))
        return;
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "a") {
        e.preventDefault();
        selectAll();
      } else if (e.key === "Escape" && selection.size > 0) {
        clearSelection();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [selectAll, clearSelection, selection.size]);

  const listProps = {
    selection,
    dupCounts,
    onItemClick,
    onItemActivate,
    onContext: openMenu,
    hasMore: assets.hasNextPage,
    loadMore: () => assets.fetchNextPage(),
    loading: assets.isFetchingNextPage,
  };

  return (
    // The centre browse region is the page's main landmark (a11y hardening, issue #44).
    <main className="flex h-full min-w-0 flex-1 flex-col bg-bg" aria-label="Asset browser">
      <Toolbar count={visible.length} total={total} onOpenNav={onOpenNav} searching={searching} />
      {/* Folder breadcrumb (issue #66) — the current source + path segments, each clickable to jump
          up the tree. Only shown when browsing a source (not a collection view). */}
      <Breadcrumb />
      {/* Partial-results strip (issue #39): one warn-tinted line, non-blocking — the results below
          are real, just possibly missing a slow peer's contribution. */}
      {droppedPeers && (
        <div
          role="status"
          className="flex items-center gap-2 border-b border-warn/40 bg-warn/10 px-3 py-1 text-[11px] text-warn"
        >
          <CloudOff size={12} className="shrink-0" />
          <span className="truncate">
            Some sources didn’t answer — results may be partial
            {droppedPeers.length > 0 && (
              <span className="opacity-80"> ({droppedPeers.join(", ")})</span>
            )}
          </span>
        </div>
      )}
      {selection.size > 1 && (
        <SelectionBar
          assets={selectedAssets}
          onSelectAll={selectAll}
          onClear={clearSelection}
        />
      )}
      <div className="min-h-0 flex-1">
        {assets.isLoading ? (
          state.view === "grid" ? (
            <GridSkeleton />
          ) : (
            <TableSkeleton />
          )
        ) : assets.isError ? (
          <Centered tone="danger">Failed to load — is `3dam serve` running?</Centered>
        ) : items.length === 0 ? (
          <Centered>
            No assets match. Add a source and scan, or clear the filters.
          </Centered>
        ) : state.view === "grid" ? (
          <Grid items={visible} {...listProps} />
        ) : (
          <Table items={visible} {...listProps} />
        )}
      </div>
      <ContextMenu
        menu={menu}
        onClose={() => setMenu(null)}
        onConvert={(assets) => setConvertTargets(assets)}
      />
      {convertTargets && (
        <ConvertDialog assets={convertTargets} onClose={() => setConvertTargets(null)} />
      )}
    </main>
  );
}

/** Folder breadcrumb (issue #66): when the browse is scoped to a source, show the source name
 *  followed by each path segment, `/`-separated. Clicking a crumb re-scopes to that level (the
 *  source name clears the folder path entirely); the trailing crumb is the current folder and is
 *  inert. Hidden in collection views and when no source is active. Reads/writes the same `source` +
 *  `path` view state the sidebar tree drives, so the two surfaces stay in lockstep. */
function Breadcrumb() {
  const { state, patch } = useViewState();
  const sources = useSources();
  if (!state.source || state.collection) return null;

  const sourceName = sources.data?.find((s) => s.id === state.source)?.name ?? "Source";
  const segs = (state.path ?? "").split("/").filter(Boolean);
  const crumbs: { label: string; path: string | null }[] = [{ label: sourceName, path: null }];
  let acc = "";
  for (const seg of segs) {
    acc += `${seg}/`;
    crumbs.push({ label: seg, path: acc });
  }

  return (
    <nav
      aria-label="Folder path"
      className="flex items-center gap-1 overflow-x-auto border-b border-border bg-surface px-3 py-1 text-[11px] text-fg-dim"
    >
      <Layers size={12} className="mr-0.5 shrink-0 text-fg-dim" />
      {crumbs.map((c, i) => {
        const last = i === crumbs.length - 1;
        return (
          <Fragment key={i}>
            {i > 0 && <ChevronRight size={11} className="shrink-0 opacity-60" />}
            <button
              className={`max-w-[10rem] shrink-0 truncate coarse:min-h-11 ${
                last ? "font-medium text-fg-muted" : "hover:text-accent"
              }`}
              disabled={last}
              onClick={() => patch({ path: c.path })}
              title={c.label}
            >
              {c.label}
            </button>
          </Fragment>
        );
      })}
    </nav>
  );
}

/** Batch-action affordance for a multi-selection (issue #10 enabler). Feature actions hang off here;
 *  today: analyze all, add all to a collection, select-all, clear. */
function SelectionBar({
  assets,
  onSelectAll,
  onClear,
}: {
  assets: AssetSummary[];
  onSelectAll: () => void;
  onClear: () => void;
}) {
  const analyze = useAnalyze();
  const collections = useCollections();
  const members = useCollectionMembers();
  const { canWrite, gate } = useWriteGate();
  const manual = (collections.data ?? []).filter((c) => c.kind === "manual");
  const [showExport, setShowExport] = useState(false);
  const [showConvert, setShowConvert] = useState(false);
  const ids = assets.map((a) => a.id);

  return (
    <div className="flex items-center gap-2 border-b border-border bg-surface px-3 py-1.5 text-xs">
      <span className="font-medium text-fg tabular-nums">{assets.length} selected</span>
      <button
        className="btn disabled:cursor-not-allowed disabled:opacity-40"
        onClick={() => analyze.mutate({ assets: ids })}
        {...gate({ disabled: analyze.isPending })}
      >
        <Sparkles size={12} /> Analyze
      </button>
      <button
        className="btn disabled:cursor-not-allowed disabled:opacity-40"
        onClick={() => setShowConvert(true)}
        {...gate()}
      >
        <FileCog size={12} /> Convert
      </button>
      <button
        className="btn disabled:cursor-not-allowed disabled:opacity-40"
        onClick={() => setShowExport(true)}
        {...gate()}
      >
        <FileDown size={12} /> Export
      </button>
      {showConvert && (
        <ConvertDialog assets={assets} onClose={() => setShowConvert(false)} />
      )}
      {showExport && (
        <ExportDialog scope={{ assets: ids }} onClose={() => setShowExport(false)} />
      )}
      <select
        className="field w-auto disabled:cursor-not-allowed disabled:opacity-40"
        aria-label="Add selection to collection"
        value=""
        disabled={manual.length === 0 || members.isPending || !canWrite}
        onChange={(e) => {
          if (e.target.value) members.mutate({ id: e.target.value, members: { add: ids } });
          e.currentTarget.value = "";
        }}
        title={
          !canWrite
            ? AUTH_COPY.needsWrite
            : manual.length === 0
              ? "No manual collections yet"
              : "Add selection to a collection"
        }
      >
        <option value="" disabled>
          Add to collection…
        </option>
        {manual.map((c) => (
          <option key={c.id} value={c.id}>
            {c.name}
          </option>
        ))}
      </select>
      <button className="text-fg-dim hover:text-fg" onClick={onSelectAll}>
        Select all
      </button>
      <button
        className="ml-auto flex items-center gap-1 text-fg-dim hover:text-fg coarse:min-h-11"
        onClick={onClear}
      >
        <X size={13} /> Clear
      </button>
    </div>
  );
}

function Toolbar({
  count,
  total,
  onOpenNav,
  searching,
}: {
  count: number;
  total: number | null;
  onOpenNav?: () => void;
  searching?: boolean;
}) {
  const { state, patch, request } = useViewState();
  const { gate } = useWriteGate();
  const [showExport, setShowExport] = useState(false);
  return (
    <div className="flex items-center gap-2 border-b border-border px-3 py-2">
      {/* Menu — opens the Navigation drawer once the layout collapses (responsive + touch pass). */}
      <button
        className="btn -ml-1 shrink-0 px-1.5 py-1 lg:hidden coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
        title="Menu"
        aria-label="Open navigation"
        onClick={onOpenNav}
      >
        <Menu size={16} />
      </button>
      <div className="relative min-w-0 flex-1">
        <Search size={13} className="absolute top-1/2 left-2 -translate-y-1/2 text-fg-dim" />
        <input
          className="field pr-6 pl-7"
          placeholder="Search assets…"
          value={state.q}
          // Searching is a faceted query — it can't compose with a collection view, so typing
          // exits collection mode (mirrors the sidebar's mutual-exclusion).
          onChange={(e) => patch({ q: e.target.value, collection: null })}
        />
        {searching ? (
          <Loader2
            size={13}
            className="absolute top-1/2 right-2 -translate-y-1/2 animate-spin text-fg-dim"
            aria-label="Searching…"
          />
        ) : (
          state.q && (
            <button
              className="absolute top-1/2 right-2 -translate-y-1/2 text-fg-dim hover:text-fg"
              onClick={() => patch({ q: "" })}
              aria-label="Clear search"
            >
              <X size={13} />
            </button>
          )
        )}
      </div>

      {/* Search-mode selector (semantic-search M5): only meaningful with a text query, so it appears
          alongside the box when one is active. Hybrid/Semantic widen results with embedding
          neighbours of the matches. */}
      {state.q && (
        <select
          className="field w-auto"
          value={state.mode}
          title="How the search text is matched"
          aria-label="Search mode"
          onChange={(e) => patch({ mode: e.target.value as SearchMode, collection: null })}
        >
          <option value="lexical">Keywords</option>
          <option value="hybrid">Keywords + similar</option>
          <option value="semantic">Most similar</option>
        </select>
      )}

      {/* Advanced Search: typed structured-attribute filters (dropdowns / ranges / toggles over the
          per-media attr columns) + tag filters. Contextual to the active media type. */}
      <AdvancedSearch />

      <select
        className="field w-auto"
        aria-label="Sort order"
        title="Sort order"
        value={`${state.sort}:${state.dir}`}
        onChange={(e) => {
          const [sort, dir] = e.target.value.split(":") as [SortField, "asc" | "desc"];
          // Sort applies to the faceted grid; a collection view has its own order, so re-sorting
          // exits collection mode.
          patch({ sort, dir, collection: null });
        }}
      >
        {/* Relevance only ranks a text search — offer it when a query is active (or already picked,
            so the control never falls to a blank value after the query is cleared). */}
        {(state.q || state.sort === "relevance") && (
          <option value="relevance:desc">Best match</option>
        )}
        <option value="name:asc">Name ↑</option>
        <option value="name:desc">Name ↓</option>
        <option value="size:desc">Largest</option>
        <option value="size:asc">Smallest</option>
        <option value="scanned:desc">Newest</option>
        <option value="scanned:asc">Oldest</option>
      </select>

      <span className="hidden text-[11px] whitespace-nowrap text-fg-dim tabular-nums sm:inline">
        {count.toLocaleString()}
        {total != null && total > count ? ` / ${total.toLocaleString()}` : ""}
      </span>

      {/* Export the current view — a collection when one is active, else the faceted query. */}
      <button
        className="btn shrink-0 px-1.5 py-1 disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
        aria-label="Export manifest"
        onClick={() => setShowExport(true)}
        {...gate({ title: "Export manifest for the current view" })}
      >
        <FileDown size={14} />
      </button>

      <div className="flex overflow-hidden rounded border border-border">
        <ViewBtn
          active={state.view === "grid"}
          onClick={() => patch({ view: "grid" })}
          label="Grid view"
        >
          <LayoutGrid size={14} />
        </ViewBtn>
        <ViewBtn
          active={state.view === "table"}
          onClick={() => patch({ view: "table" })}
          label="Table view"
        >
          <Rows3 size={14} />
        </ViewBtn>
      </div>

      {showExport && (
        <ExportDialog
          scope={state.collection ? { collection: state.collection } : { query: request }}
          onClose={() => setShowExport(false)}
        />
      )}
    </div>
  );
}

function ViewBtn({
  active,
  onClick,
  label,
  children,
}: {
  active: boolean;
  onClick: () => void;
  /** Accessible name for the icon-only toggle (a11y — axe button-name, issue #44). */
  label: string;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      title={label}
      aria-label={label}
      aria-pressed={active}
      className="flex items-center justify-center px-2 py-1 coarse:min-h-11 coarse:min-w-11"
      style={{
        background: active ? "var(--color-accent)" : "var(--color-surface-2)",
        color: active ? "var(--color-accent-fg)" : "var(--color-fg-muted)",
      }}
    >
      {children}
    </button>
  );
}

interface ListProps {
  items: AssetSummary[];
  selection: Set<string>;
  /** assetId → count of hidden byte-identical copies, for the red duplicate badge. */
  dupCounts: Map<string, number>;
  onItemClick: (asset: AssetSummary, mods: ClickMods) => void;
  onItemActivate: (asset: AssetSummary) => void;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
  hasMore: boolean;
  loadMore: () => void;
  loading: boolean;
}

/** Normalise a mouse click into our modifier model (cmd on macOS, ctrl elsewhere). */
function mods(e: React.MouseEvent): ClickMods {
  return { meta: e.metaKey || e.ctrlKey, shift: e.shiftKey };
}

/** Screen-reader label for a cell/row — name, media, and any collapsed-duplicate count, so the
 *  selected item is announced by more than colour (issue #27). */
function itemAriaLabel(asset: AssetSummary, dupCount?: number): string {
  const dup = dupCount != null && dupCount > 0 ? `, ${dupCount} duplicate${dupCount === 1 ? "" : "s"}` : "";
  return `${asset.name}, ${asset.media}${dup}`;
}

/** Keyboard roving-focus for the virtualised grid/table (issue #27). Tabbing through a 100k-item
 *  list is impractical, so exactly one cell is a tab stop (roving `tabindex`); arrow keys move it,
 *  Home/End jump to the ends, and native `<button>` semantics turn Enter/Space into selection.
 *  `cols` is the row stride — 1 for the table (horizontal arrows are ignored), the live column
 *  count for the grid. `scrollToItem` pulls the target into the virtual window before we hand it
 *  DOM focus. Returns the focused index, a setter (so a mouse click/focus can re-seat the tab stop),
 *  and the container key handler. */
function useRovingFocus(
  itemCount: number,
  cols: number,
  parentRef: React.RefObject<HTMLDivElement | null>,
  scrollToItem: (index: number) => void,
) {
  const [focusIndex, setFocusIndex] = useState(0);
  const moveFocus = useRef(false);

  // Keep the roving index in range as the list grows (infinite scroll) or shrinks (new query).
  useEffect(() => {
    if (itemCount > 0) setFocusIndex((i) => Math.min(i, itemCount - 1));
  }, [itemCount]);

  // Once a key moves the index, pull the target into the virtual window and give it real DOM focus.
  // A large jump can render a frame late, so retry once on the next frame.
  useEffect(() => {
    if (!moveFocus.current) return;
    moveFocus.current = false;
    const focus = () =>
      parentRef.current?.querySelector<HTMLElement>(`[data-index="${focusIndex}"]`)?.focus();
    focus();
    const raf = requestAnimationFrame(focus);
    return () => cancelAnimationFrame(raf);
  }, [focusIndex, parentRef]);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      let next: number | null = null;
      switch (e.key) {
        case "ArrowRight":
          if (cols === 1) return;
          next = focusIndex + 1;
          break;
        case "ArrowLeft":
          if (cols === 1) return;
          next = focusIndex - 1;
          break;
        case "ArrowDown":
          next = focusIndex + cols;
          break;
        case "ArrowUp":
          next = focusIndex - cols;
          break;
        case "Home":
          next = 0;
          break;
        case "End":
          next = itemCount - 1;
          break;
        default:
          return;
      }
      // Out of range → swallow the key so the scroll container doesn't also pan, but don't move.
      if (next < 0 || next >= itemCount) {
        e.preventDefault();
        return;
      }
      e.preventDefault();
      moveFocus.current = true;
      setFocusIndex(next);
      scrollToItem(next);
    },
    [focusIndex, cols, itemCount, scrollToItem],
  );

  return { focusIndex, setFocusIndex, onKeyDown };
}

/** Windowed grid — a 100k+ library scrolls at 60fps (DESIGN_GUIDELINES §1.1, §3.1). */
function Grid({
  items,
  selection,
  dupCounts,
  onItemClick,
  onItemActivate,
  onContext,
  hasMore,
  loadMore,
  loading,
}: ListProps) {
  const parentRef = useRef<HTMLDivElement>(null);
  const cols = useColumns(parentRef, CELL_W);
  const rowCount = Math.ceil(items.length / cols);

  const virt = useVirtualizer({
    count: rowCount,
    getScrollElement: () => parentRef.current,
    estimateSize: () => CELL_H,
    overscan: 4,
  });

  useInfinite(virt.getVirtualItems(), rowCount, hasMore, loading, loadMore);

  const scrollToItem = useCallback(
    (index: number) => virt.scrollToIndex(Math.floor(index / cols)),
    [virt, cols],
  );
  const { focusIndex, setFocusIndex, onKeyDown } = useRovingFocus(
    items.length,
    cols,
    parentRef,
    scrollToItem,
  );

  return (
    <div
      ref={parentRef}
      className="h-full overflow-y-auto"
      role="group"
      aria-label="Assets"
      onKeyDown={onKeyDown}
    >
      <div style={{ height: virt.getTotalSize(), position: "relative" }}>
        {virt.getVirtualItems().map((vr) => {
          const start = vr.index * cols;
          const row = items.slice(start, start + cols);
          return (
            <div
              key={vr.key}
              className="absolute top-0 left-0 grid w-full gap-2 px-3"
              style={{
                transform: `translateY(${vr.start}px)`,
                gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
              }}
            >
              {row.map((a, ci) => {
                const index = start + ci;
                return (
                  <GridCell
                    key={a.id}
                    asset={a}
                    index={index}
                    active={selection.has(a.id)}
                    focusable={index === focusIndex}
                    onFocusIndex={setFocusIndex}
                    dupCount={dupCounts.get(a.id)}
                    onClick={onItemClick}
                    onActivate={onItemActivate}
                    onContext={onContext}
                  />
                );
              })}
            </div>
          );
        })}
      </div>
      {loading && <div className="py-2 text-center text-[11px] text-fg-dim">Loading more…</div>}
    </div>
  );
}

/** Skeleton placeholders while the first page loads — lightweight pulsing blocks that mirror the
 *  grid/table layout so regions don't pop from blank to content (issue #30). */
function GridSkeleton() {
  return (
    <div className="h-full overflow-hidden p-3">
      <div
        className="grid gap-2"
        style={{ gridTemplateColumns: `repeat(auto-fill, minmax(${CELL_W}px, 1fr))` }}
      >
        {Array.from({ length: 18 }).map((_, i) => (
          <div
            key={i}
            className="flex flex-col overflow-hidden rounded border border-border"
            style={{ height: CELL_H - 8 }}
          >
            <div className="min-h-0 flex-1 animate-pulse bg-surface-2" />
            <div className="flex flex-col gap-1 p-1.5">
              <div className="h-2.5 w-3/4 animate-pulse rounded bg-surface-2" />
              <div className="h-2 w-1/2 animate-pulse rounded bg-surface-2" />
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

function TableSkeleton() {
  return (
    <div className="h-full overflow-hidden">
      <div className="grid grid-cols-[1fr_64px_104px_112px_84px] gap-2 border-b border-border bg-surface px-3 py-1.5 text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
        <span>Name</span>
        <span>Format</span>
        <span>License</span>
        <span>Detail</span>
        <span className="text-right">Size</span>
      </div>
      {Array.from({ length: 16 }).map((_, i) => (
        <div
          key={i}
          className="grid grid-cols-[1fr_64px_104px_112px_84px] items-center gap-2 px-3"
          style={{ height: ROW_H }}
        >
          <div className="flex items-center gap-2">
            <div className="h-3.5 w-8 shrink-0 animate-pulse rounded bg-surface-2" />
            <div className="h-2.5 w-40 animate-pulse rounded bg-surface-2" />
          </div>
          <div className="h-2.5 w-8 animate-pulse rounded bg-surface-2" />
          <div className="h-2.5 w-14 animate-pulse rounded bg-surface-2" />
          <div className="h-2.5 w-16 animate-pulse rounded bg-surface-2" />
          <div className="ml-auto h-2.5 w-10 animate-pulse rounded bg-surface-2" />
        </div>
      ))}
    </div>
  );
}

/** The media-specific "detail" from the store's `key_attrs` — image dimensions, audio duration
 *  (with the analysis type: loop / music / one-shot / sfx), or model triangle count (issue #55). One
 *  value per media type so a mixed grid/table has a single meaningful column. */
function detailAttr(asset: AssetSummary): string | null {
  const k = asset.key_attrs;
  if (asset.media === "image") return k.dimensions ?? null;
  if (asset.media === "audio")
    return k.duration ? (k.type ? `${k.duration} · ${k.type}` : k.duration) : (k.type ?? null);
  if (asset.media === "model") return k.tris ? `${k.tris} tris` : null;
  return null;
}

/** A red count badge for a collapsed duplicate group — the top-right circle showing how many
 *  byte-identical copies are folded behind this card/row (the set is listed in the Inspector). Danger
 *  tone flags the redundant storage; capped at 99+ so a large group can't blow out the layout. */
function DupBadge({ count, className = "" }: { count: number; className?: string }) {
  return (
    <span
      className={`flex h-4 min-w-4 items-center justify-center rounded-full px-1 text-[10px] font-semibold tabular-nums ${className}`}
      style={{ background: "var(--color-danger)", color: "var(--color-bg)" }}
      title={`${count} duplicate${count === 1 ? "" : "s"} — listed in the Inspector`}
      aria-label={`${count} duplicate${count === 1 ? "" : "s"}`}
    >
      {count > 99 ? "99+" : count}
    </span>
  );
}

function GridCell({
  asset,
  index,
  active,
  focusable,
  onFocusIndex,
  dupCount,
  onClick,
  onActivate,
  onContext,
}: {
  asset: AssetSummary;
  /** Flat index into the visible list — the roving-focus key and virtualiser scroll target. */
  index: number;
  active: boolean;
  /** True for the single cell that is the list's tab stop (roving tabindex, issue #27). */
  focusable: boolean;
  /** Re-seat the roving tab stop when this cell is focused by mouse/tab. */
  onFocusIndex: (index: number) => void;
  /** Count of hidden byte-identical copies; undefined ⇒ not a duplicate, no badge. */
  dupCount?: number;
  onClick: (asset: AssetSummary, mods: ClickMods) => void;
  onActivate: (asset: AssetSummary) => void;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
}) {
  const longPress = useLongPress((x, y) => onContext(asset, x, y));
  return (
    <button
      data-index={index}
      aria-pressed={active}
      aria-label={itemAriaLabel(asset, dupCount)}
      tabIndex={focusable ? 0 : -1}
      onFocus={() => onFocusIndex(index)}
      onClick={(e) => onClick(asset, mods(e))}
      onDoubleClick={() => onActivate(asset)}
      onContextMenu={(e) => {
        e.preventDefault();
        onContext(asset, e.clientX, e.clientY);
      }}
      {...longPress}
      className="group flex flex-col overflow-hidden rounded border text-left transition-colors"
      style={{
        height: CELL_H - 8,
        borderColor: active ? "var(--color-accent)" : "var(--color-border)",
        background: "var(--color-surface)",
      }}
    >
      <div className="relative min-h-0 flex-1 overflow-hidden">
        <Thumbnail asset={asset} />
        {/* media-type tag so a mixed grid reads at a glance (DESIGN_GUIDELINES — media badges) */}
        <MediaBadge media={asset.media} className="absolute top-1.5 left-1.5" />
        {/* collapsed-duplicate count, top-right (the requested red circle) */}
        {dupCount != null && dupCount > 0 && (
          <DupBadge count={dupCount} className="absolute top-1.5 right-1.5" />
        )}
      </div>
      <div className="flex items-center justify-between gap-1 border-t border-border px-1.5 py-1">
        <span className="truncate text-[11px] text-fg" title={asset.name}>
          {asset.name}
        </span>
        <FavoriteStar asset={asset} />
      </div>
      <div className="flex items-center justify-between px-1.5 pb-1">
        <LicenseBadge badge={asset.license} />
        <span className="text-[10px] text-fg-dim tabular-nums">
          {detailAttr(asset) ? `${detailAttr(asset)} · ${bytes(asset.size)}` : bytes(asset.size)}
        </span>
      </div>
    </button>
  );
}

/** Windowed table — same query, toggle preserves selection + filter (tech-spec 09 §B.1). */
function Table({
  items,
  selection,
  dupCounts,
  onItemClick,
  onItemActivate,
  onContext,
  hasMore,
  loadMore,
  loading,
}: ListProps) {
  const parentRef = useRef<HTMLDivElement>(null);
  const virt = useVirtualizer({
    count: items.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_H,
    overscan: 12,
  });
  useInfinite(virt.getVirtualItems(), items.length, hasMore, loading, loadMore);

  const scrollToItem = useCallback((index: number) => virt.scrollToIndex(index), [virt]);
  const { focusIndex, setFocusIndex, onKeyDown } = useRovingFocus(
    items.length,
    1,
    parentRef,
    scrollToItem,
  );

  return (
    <div
      ref={parentRef}
      className="h-full overflow-y-auto"
      role="group"
      aria-label="Assets"
      onKeyDown={onKeyDown}
    >
      {/* Presentational column-label strip. Not `role="row"`: the list is a roving-focus group of
          labelled row buttons (issue #27), not a full ARIA grid, so an orphaned row role here just
          triggers aria-required-parent/children (a11y, issue #44). */}
      <div
        aria-hidden="true"
        className="sticky top-0 z-10 grid grid-cols-[1fr_64px_104px_112px_84px] gap-2 border-b border-border bg-surface px-3 py-1.5 text-[10px] font-semibold tracking-wider text-fg-dim uppercase"
      >
        <span>Name</span>
        <span>Format</span>
        <span>License</span>
        <span>Detail</span>
        <span className="text-right">Size</span>
      </div>
      <div style={{ height: virt.getTotalSize(), position: "relative" }}>
        {virt.getVirtualItems().map((vr) => {
          const a = items[vr.index];
          return (
            <TableRow
              key={vr.key}
              asset={a}
              index={vr.index}
              active={selection.has(a.id)}
              focusable={vr.index === focusIndex}
              onFocusIndex={setFocusIndex}
              dupCount={dupCounts.get(a.id)}
              top={vr.start}
              onClick={onItemClick}
              onActivate={onItemActivate}
              onContext={onContext}
            />
          );
        })}
      </div>
      {loading && <div className="py-2 text-center text-[11px] text-fg-dim">Loading more…</div>}
    </div>
  );
}

function TableRow({
  asset,
  index,
  active,
  focusable,
  onFocusIndex,
  dupCount,
  top,
  onClick,
  onActivate,
  onContext,
}: {
  asset: AssetSummary;
  /** Flat index into the visible list — the roving-focus key and virtualiser scroll target. */
  index: number;
  active: boolean;
  /** True for the single row that is the list's tab stop (roving tabindex, issue #27). */
  focusable: boolean;
  /** Re-seat the roving tab stop when this row is focused by mouse/tab. */
  onFocusIndex: (index: number) => void;
  /** Count of hidden byte-identical copies; undefined ⇒ not a duplicate, no badge. */
  dupCount?: number;
  top: number;
  onClick: (asset: AssetSummary, mods: ClickMods) => void;
  onActivate: (asset: AssetSummary) => void;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
}) {
  const longPress = useLongPress((x, y) => onContext(asset, x, y));
  return (
    <button
      data-index={index}
      aria-pressed={active}
      aria-label={itemAriaLabel(asset, dupCount)}
      tabIndex={focusable ? 0 : -1}
      onFocus={() => onFocusIndex(index)}
      onClick={(e) => onClick(asset, mods(e))}
      onDoubleClick={() => onActivate(asset)}
      onContextMenu={(e) => {
        e.preventDefault();
        onContext(asset, e.clientX, e.clientY);
      }}
      {...longPress}
      className="group absolute top-0 left-0 grid w-full grid-cols-[1fr_64px_104px_112px_84px] items-center gap-2 px-3 text-left text-xs"
      style={{
        height: ROW_H,
        transform: `translateY(${top}px)`,
        background: active ? "var(--color-accent-muted)" : "transparent",
        color: active ? "var(--color-accent)" : "var(--color-fg-muted)",
      }}
    >
      <span className="flex min-w-0 items-center gap-2">
        <MediaBadge media={asset.media} className="shrink-0" />
        <span className="truncate text-fg" title={asset.name}>
          {asset.name}
        </span>
        {/* federated-origin attribution (issue #39) — renders nothing for local assets */}
        <PeerBadge origin={asset.origin} />
        {/* collapsed-duplicate count — a row has no "top right", so the red badge sits by the name */}
        {dupCount != null && dupCount > 0 && <DupBadge count={dupCount} className="shrink-0" />}
        <FavoriteStar asset={asset} />
      </span>
      <span className="truncate uppercase">{asset.format}</span>
      <LicenseBadge badge={asset.license} />
      <span className="truncate tabular-nums text-fg-dim" title={detailAttr(asset) ?? undefined}>
        {detailAttr(asset) ?? "—"}
      </span>
      <span className="text-right tabular-nums">{bytes(asset.size)}</span>
    </button>
  );
}

// ── helpers ──────────────────────────────────────────────────────────────────

/** Favourite toggle for a grid tile / table row (issue #63). The cell itself is a `<button>`, so a
 *  nested interactive control here would be an a11y violation (axe `nested-interactive`, issue #44) —
 *  this is therefore a *presentational* click target: `aria-hidden`, no role, no tab stop. It's a
 *  pointer convenience; the keyboard/AT-accessible favourite toggle is the real `<button>` in the
 *  Inspector title. `stopPropagation` keeps a star click from selecting/opening the asset. Hidden
 *  until hover/focus (always shown once starred, or on touch) so the dense grid stays low-chrome. */
function FavoriteStar({ asset }: { asset: AssetSummary }) {
  const setFavorite = useSetFavorite();
  const canWrite = useCan("write");
  const on = asset.favorite;
  // Read-only: a starred asset still shows its (non-clickable) marker so the state is visible, but
  // there's no toggle affordance — an unstarred tile shows nothing to click.
  if (!canWrite) {
    if (!on) return null;
    return (
      <span
        aria-hidden="true"
        title="Favourite (read-only — needs write access to change)"
        className="flex shrink-0 items-center justify-center rounded coarse:min-h-11 coarse:min-w-11"
        style={{ color: "var(--color-accent)" }}
      >
        <Star size={12} className="fill-current" />
      </span>
    );
  }
  return (
    <span
      aria-hidden="true"
      title={on ? "Remove from favourites" : "Add to favourites"}
      onClick={(e) => {
        e.stopPropagation();
        if (!setFavorite.isPending) setFavorite.mutate({ asset: asset.id, favorite: !on });
      }}
      className={`flex shrink-0 cursor-pointer items-center justify-center rounded transition-opacity coarse:min-h-11 coarse:min-w-11 ${
        on
          ? "opacity-100"
          : "opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 coarse:opacity-100"
      }`}
      style={{ color: on ? "var(--color-accent)" : "var(--color-fg-dim)" }}
    >
      <Star size={12} className={on ? "fill-current" : ""} />
    </span>
  );
}

/** Responsive column count from the container width (re-measured on resize). */
function useColumns(ref: React.RefObject<HTMLDivElement | null>, target: number) {
  const [cols, setCols] = useState(4);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = () =>
      setCols(Math.max(1, Math.floor((el.clientWidth - 24) / target)));
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [ref, target]);
  return cols;
}

/** Trigger the next page when the last window row nears the end. */
function useInfinite(
  visible: { index: number }[],
  count: number,
  hasMore: boolean,
  loading: boolean,
  loadMore: () => void,
) {
  const last = visible.at(-1)?.index ?? 0;
  useEffect(() => {
    if (hasMore && !loading && last >= count - 8) loadMore();
  }, [last, count, hasMore, loading, loadMore]);
}
