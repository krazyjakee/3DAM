import type { AssetSummary } from "@/api/types";

/** Modifier keys that change what a click does to the multi-selection (issue #10/#22). */
export interface ClickMods {
  meta: boolean; // ctrl/cmd → toggle one
  shift: boolean; // shift → extend a range from the anchor
}

/** The contract between the Browser (data + orchestration) and its two renderers (issue #165).
 *  Everything the grid and the table need arrives through here, so nothing outside `browser/` has
 *  to know that `@tanstack/react-virtual` — or a retained page window — exists. */
export interface ListProps {
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

/** Props shared by one grid cell and one table row — the same asset, the same roving-focus seat,
 *  and the same three pointer gestures. Each renderer adds only its own layout extras. */
export interface ItemProps {
  asset: AssetSummary;
  /** Flat index into the visible list — the roving-focus key and virtualiser scroll target. */
  index: number;
  active: boolean;
  /** True for the single cell/row that is the list's tab stop (roving tabindex, issue #27). */
  focusable: boolean;
  /** Re-seat the roving tab stop when this cell/row is focused by mouse/tab. */
  onFocusIndex: (index: number) => void;
  /** Count of hidden byte-identical copies; undefined ⇒ not a duplicate, no badge. */
  dupCount?: number;
  onClick: (asset: AssetSummary, mods: ClickMods) => void;
  onActivate: (asset: AssetSummary) => void;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
}
