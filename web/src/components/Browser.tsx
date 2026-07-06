import type React from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { FileDown, LayoutGrid, Menu, Rows3, Search, Sparkles, X } from "lucide-react";
import { useAnalyze, useAssets, useCollectionMembers, useCollections } from "@/api/queries";
import type { AssetSummary, SortField } from "@/api/types";
import { useViewState } from "@/lib/view-state";
import { bytes } from "@/lib/format";
import { Thumbnail } from "./Thumbnail";
import { LicenseBadge } from "./LicenseBadge";
import { MediaIcon } from "./MediaIcon";
import { ContextMenu, useLongPress, type MenuState } from "./ContextMenu";
import { ExportDialog } from "./ExportDialog";

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

export function Browser({ onOpenNav }: { onOpenNav?: () => void }) {
  const { state, patch, request } = useViewState();
  const assets = useAssets(request, state.collection);
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

  const clearSelection = useCallback(() => setSelection(new Set()), []);
  const selectAll = useCallback(() => setSelection(new Set(items.map((a) => a.id))), [items]);

  // Click semantics: plain = single-select + inspect; ctrl/cmd = toggle; shift = extend range from
  // the anchor. Every click also sets the Inspector focus so the detail panel tracks the last click.
  const onItemClick = useCallback(
    (asset: AssetSummary, mods: ClickMods) => {
      const id = asset.id;
      patch({ selected: id });
      if (mods.shift && anchor) {
        const ids = items.map((a) => a.id);
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
    [anchor, items, patch],
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
    onItemClick,
    onContext: openMenu,
    hasMore: assets.hasNextPage,
    loadMore: () => assets.fetchNextPage(),
    loading: assets.isFetchingNextPage,
  };

  return (
    <section className="flex h-full min-w-0 flex-1 flex-col bg-bg">
      <Toolbar count={items.length} total={total} onOpenNav={onOpenNav} />
      {selection.size > 1 && (
        <SelectionBar
          count={selection.size}
          ids={[...selection]}
          onSelectAll={selectAll}
          onClear={clearSelection}
        />
      )}
      <div className="min-h-0 flex-1">
        {assets.isLoading ? (
          <Centered>Loading…</Centered>
        ) : assets.isError ? (
          <Centered tone="danger">Failed to load — is `3dam serve` running?</Centered>
        ) : items.length === 0 ? (
          <Centered>
            No assets match. Add a source and scan, or clear the filters.
          </Centered>
        ) : state.view === "grid" ? (
          <Grid items={items} {...listProps} />
        ) : (
          <Table items={items} {...listProps} />
        )}
      </div>
      <ContextMenu menu={menu} onClose={() => setMenu(null)} />
    </section>
  );
}

/** Batch-action affordance for a multi-selection (issue #10 enabler). Feature actions hang off here;
 *  today: analyze all, add all to a collection, select-all, clear. */
function SelectionBar({
  count,
  ids,
  onSelectAll,
  onClear,
}: {
  count: number;
  ids: string[];
  onSelectAll: () => void;
  onClear: () => void;
}) {
  const analyze = useAnalyze();
  const collections = useCollections();
  const members = useCollectionMembers();
  const manual = (collections.data ?? []).filter((c) => c.kind === "manual");
  const [showExport, setShowExport] = useState(false);

  return (
    <div className="flex items-center gap-2 border-b border-border bg-surface px-3 py-1.5 text-xs">
      <span className="font-medium text-fg tabular-nums">{count} selected</span>
      <button
        className="btn coarse:min-h-11"
        onClick={() => analyze.mutate({ assets: ids })}
        disabled={analyze.isPending}
      >
        <Sparkles size={12} /> Analyze
      </button>
      <button className="btn coarse:min-h-11" onClick={() => setShowExport(true)}>
        <FileDown size={12} /> Export
      </button>
      {showExport && (
        <ExportDialog scope={{ assets: ids }} onClose={() => setShowExport(false)} />
      )}
      <select
        className="field w-auto"
        value=""
        disabled={manual.length === 0 || members.isPending}
        onChange={(e) => {
          if (e.target.value) members.mutate({ id: e.target.value, members: { add: ids } });
          e.currentTarget.value = "";
        }}
        title={manual.length === 0 ? "No manual collections yet" : "Add selection to a collection"}
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
}: {
  count: number;
  total: number | null;
  onOpenNav?: () => void;
}) {
  const { state, patch, request } = useViewState();
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
        {state.q && (
          <button
            className="absolute top-1/2 right-2 -translate-y-1/2 text-fg-dim hover:text-fg"
            onClick={() => patch({ q: "" })}
          >
            <X size={13} />
          </button>
        )}
      </div>

      <select
        className="field w-auto"
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
        className="btn shrink-0 px-1.5 py-1 coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
        title="Export manifest for the current view"
        aria-label="Export manifest"
        onClick={() => setShowExport(true)}
      >
        <FileDown size={14} />
      </button>

      <div className="flex overflow-hidden rounded border border-border">
        <ViewBtn active={state.view === "grid"} onClick={() => patch({ view: "grid" })}>
          <LayoutGrid size={14} />
        </ViewBtn>
        <ViewBtn active={state.view === "table"} onClick={() => patch({ view: "table" })}>
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
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
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
  onItemClick: (asset: AssetSummary, mods: ClickMods) => void;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
  hasMore: boolean;
  loadMore: () => void;
  loading: boolean;
}

/** Normalise a mouse click into our modifier model (cmd on macOS, ctrl elsewhere). */
function mods(e: React.MouseEvent): ClickMods {
  return { meta: e.metaKey || e.ctrlKey, shift: e.shiftKey };
}

/** Windowed grid — a 100k+ library scrolls at 60fps (DESIGN_GUIDELINES §1.1, §3.1). */
function Grid({ items, selection, onItemClick, onContext, hasMore, loadMore, loading }: ListProps) {
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

  return (
    <div ref={parentRef} className="h-full overflow-y-auto">
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
              {row.map((a) => (
                <GridCell
                  key={a.id}
                  asset={a}
                  active={selection.has(a.id)}
                  onClick={onItemClick}
                  onContext={onContext}
                />
              ))}
            </div>
          );
        })}
      </div>
    </div>
  );
}

/** The one cheap media attribute worth showing on a grid row (from the store's `key_attrs`):
 *  image dimensions, audio duration, or model triangle count. */
function primaryAttr(asset: AssetSummary): string | null {
  const k = asset.key_attrs;
  if (k.dimensions) return k.dimensions;
  if (k.duration) return k.duration;
  if (k.tris) return `${k.tris} tris`;
  return null;
}

function GridCell({
  asset,
  active,
  onClick,
  onContext,
}: {
  asset: AssetSummary;
  active: boolean;
  onClick: (asset: AssetSummary, mods: ClickMods) => void;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
}) {
  const longPress = useLongPress((x, y) => onContext(asset, x, y));
  return (
    <button
      onClick={(e) => onClick(asset, mods(e))}
      onContextMenu={(e) => {
        e.preventDefault();
        onContext(asset, e.clientX, e.clientY);
      }}
      {...longPress}
      className="flex flex-col overflow-hidden rounded border text-left transition-colors"
      style={{
        height: CELL_H - 8,
        borderColor: active ? "var(--color-accent)" : "var(--color-border)",
        background: "var(--color-surface)",
      }}
    >
      <div className="min-h-0 flex-1 overflow-hidden">
        <Thumbnail asset={asset} />
      </div>
      <div className="flex items-center justify-between gap-1 border-t border-border px-1.5 py-1">
        <span className="truncate text-[11px] text-fg" title={asset.name}>
          {asset.name}
        </span>
      </div>
      <div className="flex items-center justify-between px-1.5 pb-1">
        <LicenseBadge badge={asset.license} />
        <span className="text-[10px] text-fg-dim tabular-nums">
          {primaryAttr(asset) ? `${primaryAttr(asset)} · ${bytes(asset.size)}` : bytes(asset.size)}
        </span>
      </div>
    </button>
  );
}

/** Windowed table — same query, toggle preserves selection + filter (tech-spec 09 §B.1). */
function Table({ items, selection, onItemClick, onContext, hasMore, loadMore, loading }: ListProps) {
  const parentRef = useRef<HTMLDivElement>(null);
  const virt = useVirtualizer({
    count: items.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_H,
    overscan: 12,
  });
  useInfinite(virt.getVirtualItems(), items.length, hasMore, loading, loadMore);

  return (
    <div ref={parentRef} className="h-full overflow-y-auto">
      <div className="sticky top-0 z-10 grid grid-cols-[1fr_90px_110px_90px] gap-2 border-b border-border bg-surface px-3 py-1.5 text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
        <span>Name</span>
        <span>Format</span>
        <span>License</span>
        <span className="text-right">Size</span>
      </div>
      <div style={{ height: virt.getTotalSize(), position: "relative" }}>
        {virt.getVirtualItems().map((vr) => {
          const a = items[vr.index];
          return (
            <TableRow
              key={vr.key}
              asset={a}
              active={selection.has(a.id)}
              top={vr.start}
              onClick={onItemClick}
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
  active,
  top,
  onClick,
  onContext,
}: {
  asset: AssetSummary;
  active: boolean;
  top: number;
  onClick: (asset: AssetSummary, mods: ClickMods) => void;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
}) {
  const longPress = useLongPress((x, y) => onContext(asset, x, y));
  return (
    <button
      onClick={(e) => onClick(asset, mods(e))}
      onContextMenu={(e) => {
        e.preventDefault();
        onContext(asset, e.clientX, e.clientY);
      }}
      {...longPress}
      className="absolute top-0 left-0 grid w-full grid-cols-[1fr_90px_110px_90px] items-center gap-2 px-3 text-left text-xs"
      style={{
        height: ROW_H,
        transform: `translateY(${top}px)`,
        background: active ? "var(--color-accent-muted)" : "transparent",
        color: active ? "var(--color-accent)" : "var(--color-fg-muted)",
      }}
    >
      <span className="flex min-w-0 items-center gap-2">
        <MediaIcon media={asset.media} size={13} />
        <span className="truncate text-fg" title={asset.name}>
          {asset.name}
        </span>
      </span>
      <span className="truncate uppercase">{asset.format}</span>
      <LicenseBadge badge={asset.license} />
      <span className="text-right tabular-nums">{bytes(asset.size)}</span>
    </button>
  );
}

// ── helpers ──────────────────────────────────────────────────────────────────

function Centered({ children, tone }: { children: React.ReactNode; tone?: "danger" }) {
  return (
    <div
      className="flex h-full items-center justify-center px-6 text-center text-xs"
      style={{ color: tone === "danger" ? "var(--color-danger)" : "var(--color-fg-dim)" }}
    >
      {children}
    </div>
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
