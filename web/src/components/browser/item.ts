import type React from "react";
import type { AssetSummary } from "@/api/types";
import type { ClickMods } from "./types";

/** Per-item presentation helpers shared by the grid cell and the table row (issue #165) — the two
 *  renderers differ in layout, not in what a click means or what an item says about itself. */

/** Normalise a mouse click into our modifier model (cmd on macOS, ctrl elsewhere). */
export function mods(e: React.MouseEvent): ClickMods {
  return { meta: e.metaKey || e.ctrlKey, shift: e.shiftKey };
}

/** Screen-reader label for a cell/row — name, media, and any collapsed-duplicate count, so the
 *  selected item is announced by more than colour (issue #27). */
export function itemAriaLabel(asset: AssetSummary, dupCount?: number): string {
  const dup = dupCount != null && dupCount > 0 ? `, ${dupCount} duplicate${dupCount === 1 ? "" : "s"}` : "";
  return `${asset.name}, ${asset.media}${dup}`;
}

/** The media-specific "detail" from the store's `key_attrs` — image dimensions, audio duration
 *  (with the analysis type: loop / music / one-shot / sfx), model triangle count (issue #55), video
 *  resolution + running time, or document page/word count. One value per media type so a mixed
 *  grid/table has a single meaningful column. */
export function detailAttr(asset: AssetSummary): string | null {
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
