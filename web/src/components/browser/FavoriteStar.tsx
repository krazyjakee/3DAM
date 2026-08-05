import { Star } from "lucide-react";
import { useCan, useSetFavorite } from "@/api/queries";
import type { AssetSummary } from "@/api/types";
import { isLocal } from "@/lib/origin";

/** Favourite toggle for a grid tile / table row (issue #63). The cell itself is a `<button>`, so a
 *  nested interactive control here would be an a11y violation (axe `nested-interactive`, issue #44) —
 *  this is therefore a *presentational* click target: `aria-hidden`, no role, no tab stop. It's a
 *  pointer convenience; the keyboard/AT-accessible favourite toggle is the real `<button>` in the
 *  Inspector title. `stopPropagation` keeps a star click from selecting/opening the asset. Hidden
 *  until hover/focus (always shown once starred, or on touch) so the dense grid stays low-chrome. */
export function FavoriteStar({ asset }: { asset: AssetSummary }) {
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
