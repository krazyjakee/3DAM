import { useCallback, useRef } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { bytes } from "@/lib/format";
import { assetSelectionKey } from "@/lib/selection";
import { shortcutLabel } from "@/lib/shortcuts";
import { LicenseBadge } from "../LicenseBadge";
import { MediaBadge } from "../MediaBadge";
import { PeerBadge } from "../PeerBadge";
import { useLongPress } from "../ContextMenu";
import { DupBadge } from "./DupBadge";
import { FavoriteStar } from "./FavoriteStar";
import { detailAttr, itemAriaLabel, mods } from "./item";
import type { ItemProps, ListProps } from "./types";
import { useBrowseWindowLoading } from "./useBrowseWindowLoading";
import { useRovingFocus } from "./useRovingFocus";

// Table row height. Virtualization drives the height in JS, so `coarse:` CSS can't reach it — bump
// to a 44px touch target on coarse pointers instead (issue #31). Pointer type is stable per session.
const COARSE_POINTER =
  typeof window !== "undefined" && window.matchMedia("(pointer: coarse)").matches;
const ROW_H = COARSE_POINTER ? 44 : 30;

/** Windowed table — same query, toggle preserves selection + filter (tech-spec 09 §B.1). */
export function Table({
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
}: ItemProps & {
  /** Absolute offset of this row inside the virtualiser's spacer. */
  top: number;
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

/** Skeleton placeholders while the first page loads — lightweight pulsing blocks that mirror the
 *  table layout so the region doesn't pop from blank to content (issue #30). */
export function TableSkeleton() {
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
