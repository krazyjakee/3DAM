import type React from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { CloudOff, AlertTriangle, Star } from "lucide-react";
import {
  useAsset,
  useAssets,
  useCan,
  useCollections,
  useDuplicateMembership,
  useSetFavorite,
  useSources,
} from "@/api/queries";
import { api } from "@/api/client";
import type { AssetSummary } from "@/api/types";
import { isLocal } from "@/lib/origin";
import { useViewState } from "@/lib/view-state";
import { useDebounced } from "@/lib/use-debounced";
import { bytes } from "@/lib/format";
import { requestAutoplay } from "@/lib/audio-intent";
import {
  dispatchShortcut,
  isEditableTarget,
  shortcutLabel,
  SHORTCUT_EVENT,
  type ShortcutId,
} from "@/lib/shortcuts";
import { Thumbnail } from "./Thumbnail";
import { LicenseBadge } from "./LicenseBadge";
import { MediaBadge } from "./MediaBadge";
import { PeerBadge } from "./PeerBadge";
import { ContextMenu, useLongPress, type MenuState } from "./ContextMenu";
import { ConvertDialog } from "./ConvertDialog";
import { ActiveFilters } from "./ActiveFilters";
import { Breadcrumb } from "./browser/Breadcrumb";
import { SelectionBar } from "./browser/SelectionBar";
import { Toolbar } from "./browser/Toolbar";
import { Centered } from "@/lib/ui";
import { assetSelectionKey, useSelection, type ResultSelector } from "@/lib/selection";
import {
  browseWindowMetrics,
  flattenBrowsePages,
  type BrowsePageParam,
} from "@/lib/browse-window";
import { collapseExactDuplicates } from "@/lib/duplicate-membership";
import { summarizeQuery } from "@/lib/query-summary";

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

export function Browser({
  onOpenNav,
  onShowShortcuts,
}: {
  onOpenNav?: () => void;
  onShowShortcuts?: () => void;
}) {
  const { state, patch, request } = useViewState();
  const selection = useSelection();
  const {
    reconcile,
    selectExplicit,
    selectAsset,
    focusAsset,
    isSelected,
    clear,
    count: selectionCount,
    selected: selectionScope,
    explicitAssets,
  } = selection;
  const browserRef = useRef<HTMLElement>(null);
  // Debounce the *derived* search text so the field stays instant but `/query` only refetches once
  // typing settles (issue #33). The other facets apply immediately; only free-text is debounced.
  const debouncedText = useDebounced(state.q, 300);
  const searchReq = useMemo(
    () => ({ ...request, text: debouncedText.trim() || null }),
    [request, debouncedText],
  );
  const assets = useAssets(searchReq, state.collection);
  const collections = useCollections();
  const sources = useSources();
  const smartFolderWarning = useMemo(() => {
    if (!state.collection) return null;
    const collection = collections.data?.find((item) => item.id === state.collection);
    if (collection?.kind !== "smart") return null;
    return summarizeQuery(collection.query, sources.data).warnings[0] ?? null;
  }, [state.collection, collections.data, sources.data]);
  // A search is pending while the field's text hasn't yet been applied to the query.
  const searching = state.q.trim() !== debouncedText.trim();
  const [menu, setMenu] = useState<MenuState | null>(null);
  const selectedAsset = useAsset(selection.focused?.id ?? null, selection.focused?.owner);

  // `useAssets` retains only a small page LRU. Everything derived here is consequently bounded by
  // that window instead of growing with the lifetime scroll history.
  const items = useMemo(() => flattenBrowsePages(assets.data?.pages), [assets.data?.pages]);
  const total = assets.data?.pages.find((page) => page.total !== null)?.total ?? null;
  const bySelectionKey = useMemo(
    () => new Map(items.map((asset) => [assetSelectionKey(asset), asset])),
    [items],
  );

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
  // HTTP/2 (ADR 0012); this only moves generation ahead of render. The remembered key set is itself
  // bounded to the current LRU; returning to an evicted page may harmlessly warm it again.
  const prefetchedPages = useRef<Set<string>>(new Set());
  useEffect(() => {
    const pages = assets.data?.pages ?? [];
    const params = (assets.data?.pageParams ?? []) as BrowsePageParam[];
    const current = new Set<string>();
    pages.forEach((page, index) => {
      if (page.items.length === 0) return;
      const key = `${params[index]?.index ?? index}:${page.items[0]?.id ?? "empty"}`;
      current.add(key);
      if (!prefetchedPages.current.has(key))
        void api.prefetch({ assets: page.items.map((a) => a.id) }).catch(() => {});
    });
    prefetchedPages.current = current;
  }, [assets.data]);

  // IDs from peers are only meaningful to their owning peer. Never submit them to this local
  // lookup: a colliding UUID must not acquire a local duplicate badge or reveal a local group.
  const localAssetIds = useMemo(
    () => items.filter((asset) => isLocal(asset.origin)).map((asset) => asset.id),
    [items],
  );
  const dups = useDuplicateMembership(localAssetIds);
  const { visible, dupCounts } = useMemo(
    () => collapseExactDuplicates(items, dups.data),
    [items, dups.data],
  );

  const resultSelector = useMemo<ResultSelector>(
    () =>
      state.collection
        ? { kind: "collection", collection: state.collection }
        : { kind: "query", query: searchReq },
    [state.collection, searchReq],
  );
  // View mode is deliberately absent: grid/table is presentation, not a new result set.
  const browseSelectionScope = useMemo(
    () => JSON.stringify([state.collection, request]),
    [state.collection, request],
  );
  useEffect(() => {
    reconcile({
      browseScope: browseSelectionScope,
      loaded: items,
      visible,
      selector: resultSelector,
      total,
    });
  }, [browseSelectionScope, items, visible, resultSelector, total, reconcile]);

  const firstPageParam = assets.data?.pageParams[0] as BrowsePageParam | undefined;
  const windowMetrics = browseWindowMetrics(
    firstPageParam,
    visible.length,
    assets.hasNextPage,
  );

  const selectVisible = useCallback(() => {
    const nodes = browserRef.current?.querySelectorAll<HTMLElement>("[data-selection-key]") ?? [];
    const keys = new Set(
      [...nodes]
        .filter((node) => {
          const viewport = node.closest<HTMLElement>("[role='group'][aria-label='Assets']");
          if (!viewport) return false;
          const cell = node.getBoundingClientRect();
          const frame = viewport.getBoundingClientRect();
          return cell.bottom > frame.top && cell.top < frame.bottom;
        })
        .map((node) => node.dataset.selectionKey ?? ""),
    );
    selectExplicit(visible.filter((asset) => keys.has(assetSelectionKey(asset))));
  }, [visible, selectExplicit]);
  // Convert targets set from the context menu (single/multi); rendered as a dialog at Browser root.
  const [convertTargets, setConvertTargets] = useState<AssetSummary[] | null>(null);

  // Click semantics: plain = single-select + inspect; ctrl/cmd = toggle; shift = extend range from
  // the anchor. Every click also sets the Inspector focus so the detail panel tracks the last click.
  const onItemClick = useCallback(
    (asset: AssetSummary, mods: ClickMods) => {
      selectAsset(asset, mods, visible);
    },
    [selectAsset, visible],
  );

  // Double-click / double-tap = activate: focus the asset in the Inspector and, for audio, start
  // playback immediately (issue #52). Non-audio just opens in the Inspector's viewer.
  const onItemActivate = useCallback(
    (asset: AssetSummary) => {
      focusAsset(asset);
      if (asset.media === "audio") requestAutoplay(asset.id);
    },
    [focusAsset],
  );

  // Right-click / long-press targets the whole selection when the clicked item is part of a
  // multi-selection; otherwise just that item (issue #22).
  const openMenu = useCallback(
    (asset: AssetSummary, x: number, y: number) => {
      const targets =
        selectionScope.kind === "explicit" &&
        isSelected(asset) &&
        selectionCount > 1
          ? explicitAssets
          : [asset];
      setMenu({ assets: targets.length ? targets : [asset], x, y });
    },
    [selectionScope, isSelected, selectionCount, explicitAssets],
  );

  useEffect(() => {
    const onShortcut = (event: Event) => {
      const id = (event as CustomEvent<ShortcutId>).detail;
      const asset = selectedAsset.data?.summary;
      if (!asset) return;
      if (id === "action-menu") {
        const cell = document.querySelector<HTMLElement>(
          `[data-selection-key="${CSS.escape(assetSelectionKey(asset))}"]`,
        );
        const rect = cell?.getBoundingClientRect();
        openMenu(
          asset,
          rect ? rect.left + Math.min(24, rect.width / 2) : window.innerWidth / 2,
          rect ? rect.top + Math.min(24, rect.height / 2) : window.innerHeight / 2,
        );
      }
    };
    window.addEventListener(SHORTCUT_EVENT, onShortcut);
    return () => window.removeEventListener(SHORTCUT_EVENT, onShortcut);
  }, [openMenu, selectedAsset.data?.summary]);

  // Space retains native button activation unless the current selection is playable audio. This
  // context-sensitive registration avoids swallowing Space while a user is merely browsing cells.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (document.getElementById("shortcut-help-title")) return;
      const asset = selectedAsset.data?.summary;
      const focusedCell =
        event.target instanceof Element
          ? event.target.closest<HTMLElement>("[data-asset-id]")
          : null;
      const canPlay =
        asset?.media === "audio" &&
        (!focusedCell || focusedCell.dataset.assetId === asset.id);
      const focusedAsset = focusedCell
        ? bySelectionKey.get(focusedCell.dataset.selectionKey ?? "")
        : undefined;
      const handled = dispatchShortcut(event, {
        "play-pause":
          canPlay
            ? () =>
                window.dispatchEvent(
                  new CustomEvent<ShortcutId>(SHORTCUT_EVENT, { detail: "play-pause" }),
                )
            : undefined,
        "action-menu": focusedAsset
          ? () => {
              const rect = focusedCell?.getBoundingClientRect();
              openMenu(
                focusedAsset,
                rect ? rect.left + Math.min(24, rect.width / 2) : window.innerWidth / 2,
                rect ? rect.top + Math.min(24, rect.height / 2) : window.innerHeight / 2,
              );
            }
          : undefined,
      });
      if (handled) event.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [bySelectionKey, openMenu, selectedAsset.data?.summary]);

  // Keyboard: Ctrl/Cmd+A selects all, Escape clears — but never while typing in the search box.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (document.getElementById("shortcut-help-title")) return;
      if (isEditableTarget(e.target)) return;
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "a") {
        e.preventDefault();
        selectExplicit(visible);
      } else if (e.key === "Escape" && selectionCount > 0) {
        clear();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [selectExplicit, clear, selectionCount, visible]);

  const listProps = {
    isSelected,
    dupCounts,
    onItemClick,
    onItemActivate,
    onContext: openMenu,
    hasMore: assets.hasNextPage,
    loadMore: () => assets.fetchNextPage(),
    loading: assets.isFetchingNextPage,
    hasPrevious: assets.hasPreviousPage,
    loadPrevious: () => assets.fetchPreviousPage(),
    loadingPrevious: assets.isFetchingPreviousPage,
    windowStart: windowMetrics.start,
    virtualCount: windowMetrics.virtualCount,
  };

  return (
    // The centre browse region is the page's main landmark (a11y hardening, issue #44).
    <main
      ref={browserRef}
      className="browser-shell flex h-full min-w-0 flex-1 flex-col bg-bg"
      aria-label="Asset browser"
      data-shortcut-region="browser"
      tabIndex={-1}
    >
      <Toolbar
        count={windowMetrics.start + items.length}
        total={total}
        onOpenNav={onOpenNav}
        onShowShortcuts={onShowShortcuts}
        searching={searching}
      />
      <ActiveFilters
        onClearAll={() => {
          selection.clear();
          patch({
            q: "",
            media: null,
            source: null,
            license: null,
            tag: null,
            collection: null,
            fav: false,
            path: null,
            subfolders: true,
            adv: [],
            sort: "name",
            dir: "asc",
            mode: "lexical",
            selected: null,
          });
        }}
      />
      {/* Folder breadcrumb (issue #66) — the current source + path segments, each clickable to jump
          up the tree. Only shown when browsing a source (not a collection view). */}
      <Breadcrumb />
      {smartFolderWarning && (
        <div
          role="alert"
          className="flex items-start gap-2 border-b border-warn/40 bg-warn/10 px-3 py-1.5 text-[11px] text-warn"
        >
          <AlertTriangle size={13} className="mt-0.5 shrink-0" />
          <span>{smartFolderWarning} Replace this smart folder’s query from a compatible search.</span>
        </div>
      )}
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
      {(selection.count > 1 || selection.selected.kind === "results") && (
        <SelectionBar
          selection={selection}
          loaded={visible}
          onSelectVisible={selectVisible}
          resultSelector={resultSelector}
          total={total}
          resultComplete={droppedPeers === null}
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
          <Centered tone="danger">
            {smartFolderWarning
              ? "This smart folder’s saved query could not be loaded. Replace it from a compatible search."
              : "Failed to load — is `3dam serve` running?"}
          </Centered>
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

interface ListProps {
  items: AssetSummary[];
  isSelected: (asset: AssetSummary) => boolean;
  /** assetId → count of hidden byte-identical copies, for the red duplicate badge. */
  dupCounts: Map<string, number>;
  onItemClick: (asset: AssetSummary, mods: ClickMods) => void;
  onItemActivate: (asset: AssetSummary) => void;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
  hasMore: boolean;
  loadMore: () => void;
  loading: boolean;
  hasPrevious: boolean;
  loadPrevious: () => void;
  loadingPrevious: boolean;
  /** Absolute logical slot of `items[0]`; earlier slots are evicted page height. */
  windowStart: number;
  /** Absolute virtual length, including the next-page loading runway. */
  virtualCount: number;
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
  isSelected,
  dupCounts,
  onItemClick,
  onItemActivate,
  onContext,
  hasMore,
  loadMore,
  loading,
  hasPrevious,
  loadPrevious,
  loadingPrevious,
  windowStart,
  virtualCount,
}: ListProps) {
  const parentRef = useRef<HTMLDivElement>(null);
  const cols = useColumns(parentRef, CELL_W);
  const rowCount = Math.ceil(virtualCount / cols);

  const virt = useVirtualizer({
    count: rowCount,
    getScrollElement: () => parentRef.current,
    estimateSize: () => CELL_H,
    // Grid/table switches remount the virtualizer. Resume at the retained window instead of
    // starting in its evicted leading spacer and accidentally refetching the entire history.
    initialOffset: Math.floor(windowStart / cols) * CELL_H,
    overscan: 4,
  });

  const virtualRows = virt.getVirtualItems();
  useBrowseWindowLoading(
    (virtualRows[0]?.index ?? 0) * cols,
    ((virtualRows.at(-1)?.index ?? 0) + 1) * cols - 1,
    windowStart,
    windowStart + items.length,
    hasPrevious,
    loadingPrevious,
    loadPrevious,
    hasMore,
    loading,
    loadMore,
  );

  const scrollToItem = useCallback(
    (index: number) => virt.scrollToIndex(Math.floor((windowStart + index) / cols)),
    [virt, cols, windowStart],
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
        {virtualRows.map((vr) => {
          const start = vr.index * cols;
          const row = Array.from({ length: cols }, (_, column) => {
            const absoluteIndex = start + column;
            const localIndex = absoluteIndex - windowStart;
            return { absoluteIndex, localIndex, asset: items[localIndex] };
          });
          if (row.every(({ asset }) => !asset)) return null;
          return (
            <div
              key={vr.key}
              className="absolute top-0 left-0 grid w-full gap-2 px-3"
              style={{
                transform: `translateY(${vr.start}px)`,
                gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))`,
              }}
            >
              {row.map(({ asset: a, absoluteIndex, localIndex }) => {
                if (!a) return <div key={`empty-${absoluteIndex}`} aria-hidden="true" />;
                return (
                  <GridCell
                    key={assetSelectionKey(a)}
                    asset={a}
                    index={localIndex}
                    active={isSelected(a)}
                    focusable={localIndex === focusIndex}
                    onFocusIndex={setFocusIndex}
                    dupCount={dupCounts.get(assetSelectionKey(a))}
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
      {loadingPrevious && (
        <div className="sticky top-0 z-20 py-1 text-center text-[11px] text-fg-dim">
          Loading earlier…
        </div>
      )}
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
      <div className="asset-table-columns grid gap-2 border-b border-border bg-surface px-3 py-1.5 text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
        <span>Name</span>
        <span>Format</span>
        <span className="asset-table-license">License</span>
        <span className="asset-table-detail">Detail</span>
        <span className="text-right">Size</span>
      </div>
      {Array.from({ length: 16 }).map((_, i) => (
        <div
          key={i}
          className="asset-table-columns grid items-center gap-2 px-3"
          style={{ height: ROW_H }}
        >
          <div className="flex items-center gap-2">
            <div className="h-3.5 w-8 shrink-0 animate-pulse rounded bg-surface-2" />
            <div className="h-2.5 w-40 animate-pulse rounded bg-surface-2" />
          </div>
          <div className="h-2.5 w-8 animate-pulse rounded bg-surface-2" />
          <div className="asset-table-license h-2.5 w-14 animate-pulse rounded bg-surface-2" />
          <div className="asset-table-detail h-2.5 w-16 animate-pulse rounded bg-surface-2" />
          <div className="ml-auto h-2.5 w-10 animate-pulse rounded bg-surface-2" />
        </div>
      ))}
    </div>
  );
}

/** The media-specific "detail" from the store's `key_attrs` — image dimensions, audio duration
 *  (with the analysis type: loop / music / one-shot / sfx), model triangle count (issue #55), video
 *  resolution + running time, or document page/word count. One value per media type so a mixed
 *  grid/table has a single meaningful column. */
function detailAttr(asset: AssetSummary): string | null {
  const k = asset.key_attrs;
  if (asset.media === "image") return k.dimensions ?? null;
  if (asset.media === "audio")
    return k.duration ? (k.type ? `${k.duration} · ${k.type}` : k.duration) : (k.type ?? null);
  if (asset.media === "model") return k.tris ? `${k.tris} tris` : null;
  // Video is the one type that fills both slots — neither resolution nor length implies the other.
  if (asset.media === "video")
    return [k.dimensions, k.duration].filter(Boolean).join(" · ") || null;
  // Pages are meaningless for plaintext, so the store only sets them when the container paginates.
  if (asset.media === "document")
    return k.pages ? `${k.pages} pp` : k.words ? `${k.words} words` : null;
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
      data-asset-id={asset.id}
      data-selection-key={assetSelectionKey(asset)}
      aria-pressed={active}
      aria-label={itemAriaLabel(asset, dupCount)}
      aria-keyshortcuts="Shift+F10"
      title={`Open actions (${shortcutLabel("action-menu")})`}
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
  isSelected,
  dupCounts,
  onItemClick,
  onItemActivate,
  onContext,
  hasMore,
  loadMore,
  loading,
  hasPrevious,
  loadPrevious,
  loadingPrevious,
  windowStart,
  virtualCount,
}: ListProps) {
  const parentRef = useRef<HTMLDivElement>(null);
  const virt = useVirtualizer({
    count: virtualCount,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_H,
    initialOffset: windowStart * ROW_H,
    overscan: 12,
  });
  const virtualRows = virt.getVirtualItems();
  useBrowseWindowLoading(
    virtualRows[0]?.index ?? 0,
    virtualRows.at(-1)?.index ?? 0,
    windowStart,
    windowStart + items.length,
    hasPrevious,
    loadingPrevious,
    loadPrevious,
    hasMore,
    loading,
    loadMore,
  );

  const scrollToItem = useCallback(
    (index: number) => virt.scrollToIndex(windowStart + index),
    [virt, windowStart],
  );
  const { focusIndex, setFocusIndex, onKeyDown } = useRovingFocus(
    items.length,
    1,
    parentRef,
    scrollToItem,
  );

  return (
    <div
      ref={parentRef}
      className="h-full overflow-x-hidden overflow-y-auto"
      role="group"
      aria-label="Assets"
      onKeyDown={onKeyDown}
    >
      {/* Presentational column-label strip. Not `role="row"`: the list is a roving-focus group of
          labelled row buttons (issue #27), not a full ARIA grid, so an orphaned row role here just
          triggers aria-required-parent/children (a11y, issue #44). */}
      <div
        aria-hidden="true"
        className="asset-table-columns sticky top-0 z-10 grid gap-2 border-b border-border bg-surface px-3 py-1.5 text-[10px] font-semibold tracking-wider text-fg-dim uppercase"
      >
        <span>Name</span>
        <span>Format</span>
        <span className="asset-table-license">License</span>
        <span className="asset-table-detail">Detail</span>
        <span className="text-right">Size</span>
      </div>
      <div style={{ height: virt.getTotalSize(), position: "relative" }}>
        {virtualRows.map((vr) => {
          const localIndex = vr.index - windowStart;
          const a = items[localIndex];
          if (!a) return null;
          return (
            <TableRow
              key={assetSelectionKey(a)}
              asset={a}
              index={localIndex}
              active={isSelected(a)}
              focusable={localIndex === focusIndex}
              onFocusIndex={setFocusIndex}
              dupCount={dupCounts.get(assetSelectionKey(a))}
              top={vr.start}
              onClick={onItemClick}
              onActivate={onItemActivate}
              onContext={onContext}
            />
          );
        })}
      </div>
      {loadingPrevious && (
        <div className="sticky top-6 z-20 py-1 text-center text-[11px] text-fg-dim">
          Loading earlier…
        </div>
      )}
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
      data-asset-id={asset.id}
      data-selection-key={assetSelectionKey(asset)}
      aria-pressed={active}
      aria-label={itemAriaLabel(asset, dupCount)}
      aria-keyshortcuts="Shift+F10"
      title={`Open actions (${shortcutLabel("action-menu")})`}
      tabIndex={focusable ? 0 : -1}
      onFocus={() => onFocusIndex(index)}
      onClick={(e) => onClick(asset, mods(e))}
      onDoubleClick={() => onActivate(asset)}
      onContextMenu={(e) => {
        e.preventDefault();
        onContext(asset, e.clientX, e.clientY);
      }}
      {...longPress}
      className="asset-table-columns group absolute top-0 left-0 grid w-full items-center gap-2 px-3 text-left text-xs"
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
      <span className="asset-table-license min-w-0">
        <LicenseBadge badge={asset.license} />
      </span>
      <span className="asset-table-detail truncate tabular-nums text-fg-dim" title={detailAttr(asset) ?? undefined}>
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
  // Read-only — the caller lacks write scope, or the asset is a peer-owned reference (tech-spec 07
  // §7.4): a starred asset still shows its (non-clickable) marker so the state is visible, but
  // there's no toggle affordance — an unstarred tile shows nothing to click.
  if (!canWrite || !isLocal(asset.origin)) {
    if (!on) return null;
    return (
      <span
        aria-hidden="true"
        title={
          !isLocal(asset.origin)
            ? "Favourite (read-only — lives on a federated peer)"
            : "Favourite (read-only — needs write access to change)"
        }
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

/** Refill either edge of the retained page LRU as its loading runway enters the viewport. */
function useBrowseWindowLoading(
  firstVisible: number,
  lastVisible: number,
  windowStart: number,
  windowEnd: number,
  hasPrevious: boolean,
  loadingPrevious: boolean,
  loadPrevious: () => void,
  hasMore: boolean,
  loading: boolean,
  loadMore: () => void,
) {
  useEffect(() => {
    if (hasPrevious && !loadingPrevious && firstVisible <= windowStart + 8) loadPrevious();
  }, [firstVisible, windowStart, hasPrevious, loadingPrevious, loadPrevious]);
  useEffect(() => {
    if (hasMore && !loading && lastVisible >= windowEnd - 8) loadMore();
  }, [lastVisible, windowEnd, hasMore, loading, loadMore]);
}
