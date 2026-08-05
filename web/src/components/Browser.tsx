import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { CloudOff, AlertTriangle } from "lucide-react";
import {
  useAsset,
  useAssets,
  useCollections,
  useDuplicateMembership,
  useSources,
} from "@/api/queries";
import { api } from "@/api/client";
import type { AssetSummary } from "@/api/types";
import { isLocal } from "@/lib/origin";
import { useViewState } from "@/lib/view-state";
import { useDebounced } from "@/lib/use-debounced";
import { requestAutoplay } from "@/lib/audio-intent";
import {
  dispatchShortcut,
  isEditableTarget,
  SHORTCUT_EVENT,
  type ShortcutId,
} from "@/lib/shortcuts";
import { ContextMenu, type MenuState } from "./ContextMenu";
import { ConvertDialog } from "./ConvertDialog";
import { ActiveFilters } from "./ActiveFilters";
import { Breadcrumb } from "./browser/Breadcrumb";
import { SelectionBar } from "./browser/SelectionBar";
import { Toolbar } from "./browser/Toolbar";
import { Grid, GridSkeleton } from "./browser/GridList";
import { Table, TableSkeleton } from "./browser/TableList";
import type { ClickMods, ListProps } from "./browser/types";
import { Centered } from "@/lib/ui";
import { assetSelectionKey, useSelection, type ResultSelector } from "@/lib/selection";
import {
  browseWindowMetrics,
  flattenBrowsePages,
  type BrowsePageParam,
} from "@/lib/browse-window";
import { collapseExactDuplicates } from "@/lib/duplicate-membership";
import { summarizeQuery } from "@/lib/query-summary";

/** The browse region: it owns the query, the retained page window, the selection reconciliation,
 *  and the keyboard/context-menu wiring — then hands the result to one of the two renderers in
 *  `browser/` through `ListProps` (issue #165). Nothing here knows how a list is virtualised. */
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

  const listProps: Omit<ListProps, "items"> = {
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
