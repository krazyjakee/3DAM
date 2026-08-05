import type React from "react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { bytes } from "@/lib/format";
import { assetSelectionKey } from "@/lib/selection";
import { shortcutLabel } from "@/lib/shortcuts";
import { LicenseBadge } from "../LicenseBadge";
import { MediaBadge } from "../MediaBadge";
import { Thumbnail } from "../Thumbnail";
import { useLongPress } from "../ContextMenu";
import { DupBadge } from "./DupBadge";
import { FavoriteStar } from "./FavoriteStar";
import { detailAttr, itemAriaLabel, mods } from "./item";
import type { ItemProps, ListProps } from "./types";
import { useBrowseWindowLoading } from "./useBrowseWindowLoading";
import { useRovingFocus } from "./useRovingFocus";

const CELL_W = 150; // grid cell target width (px); actual columns computed from container
const CELL_H = 132;

/** Windowed grid — a 100k+ library scrolls at 60fps (DESIGN_GUIDELINES §1.1, §3.1). */
export function Grid({
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
}: ItemProps) {
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

/** Skeleton placeholders while the first page loads — lightweight pulsing blocks that mirror the
 *  grid layout so the region doesn't pop from blank to content (issue #30). */
export function GridSkeleton() {
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
