// Per-item context menu for asset tiles/rows (issue #20). Opened by right-click or touch long-press
// (see useLongPress) from the Browser, which owns the open state and passes the target asset here.
// The action set lives in ONE place: open, analyze, convert (#5), add-to-collection (submenu),
// copy path, and remove / remove + block (#21). Actions honour the whole target set (the clicked
// asset, or the multi-selection when the clicked item is part of it — #22).

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  FileCog,
  FolderPlus,
  Search,
  Sparkles,
  RefreshCw,
  ClipboardCopy,
  Trash2,
  Ban,
} from "lucide-react";
import { api } from "@/api/client";
import {
  useAnalyze,
  useCollections,
  useCollectionMembers,
  useRegenerateThumbnail,
  useRemoveAsset,
} from "@/api/queries";
import type { AssetSummary } from "@/api/types";
import { useViewState } from "@/lib/view-state";

export interface MenuState {
  x: number;
  y: number;
  /** The target(s): a single right-clicked asset, or the whole multi-selection when the clicked
   *  item is part of it (issue #22). Length ≥ 1. */
  assets: AssetSummary[];
}

/** Long-press (touch) helper — returns pointer handlers that fire `onLongPress` after ~500ms of a
 *  stationary press, so coarse pointers get the same menu as a right-click. Cancels on move/lift. */
export function useLongPress(onLongPress: (x: number, y: number) => void) {
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const start = useRef<{ x: number; y: number } | null>(null);
  const clear = () => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = undefined;
    start.current = null;
  };
  return {
    onPointerDown: (e: React.PointerEvent) => {
      if (e.pointerType === "mouse") return; // right-click handles mouse
      start.current = { x: e.clientX, y: e.clientY };
      timer.current = setTimeout(() => onLongPress(e.clientX, e.clientY), 500);
    },
    onPointerMove: (e: React.PointerEvent) => {
      if (!start.current) return;
      if (Math.hypot(e.clientX - start.current.x, e.clientY - start.current.y) > 10) clear();
    },
    onPointerUp: clear,
    onPointerCancel: clear,
  };
}

export function ContextMenu({
  menu,
  onClose,
  onConvert,
}: {
  menu: MenuState | null;
  onClose: () => void;
  /** Hand the target asset(s) up to the Browser, which hosts the convert dialog (issue #5). */
  onConvert: (assets: AssetSummary[]) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ x: 0, y: 0 });
  const { patch } = useViewState();
  const analyze = useAnalyze();
  const regenerateThumbnail = useRegenerateThumbnail();
  const collections = useCollections();
  const members = useCollectionMembers();
  const remove = useRemoveAsset();
  const [submenu, setSubmenu] = useState(false);
  // Removal is a two-step, in-menu confirm (issue #21): the first click reveals Remove / Remove+block.
  const [confirming, setConfirming] = useState(false);

  // Close on outside pointer, Escape, scroll, or resize.
  useEffect(() => {
    if (!menu) return;
    const onDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", onKey);
    window.addEventListener("scroll", onClose, true);
    window.addEventListener("resize", onClose);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("scroll", onClose, true);
      window.removeEventListener("resize", onClose);
    };
  }, [menu, onClose]);

  // Clamp within the viewport once the menu size is known.
  useLayoutEffect(() => {
    if (!menu || !ref.current) return;
    const r = ref.current.getBoundingClientRect();
    const pad = 6;
    setPos({
      x: Math.min(menu.x, window.innerWidth - r.width - pad),
      y: Math.min(menu.y, window.innerHeight - r.height - pad),
    });
    setSubmenu(false);
    setConfirming(false);
  }, [menu]);

  if (!menu) return null;
  const assets = menu.assets;
  const ids = assets.map((a) => a.id);
  const single = assets.length === 1;
  const heading = single ? assets[0].name : `${assets.length} items`;
  // Only image + 3D assets have a server thumbnail to rebuild (audio uses the honest typed tile);
  // hide "Regenerate thumbnail" when nothing in the target set can produce one.
  const thumbableIds = assets
    .filter((a) => a.media === "image" || a.media === "model")
    .map((a) => a.id);

  const run = (fn: () => void) => {
    fn();
    onClose();
  };

  const manualCollections = (collections.data ?? []).filter((c) => c.kind === "manual");

  const copyPath = async () => {
    try {
      const full = await api.getAsset(ids[0]);
      await navigator.clipboard?.writeText(full.path);
    } catch {
      /* clipboard blocked / fetch failed — fail-soft, no crash */
    }
  };

  // Remove the target set from the catalog; `block` also blocks each content hash from re-import.
  // The source files are never touched — only catalog rows (issue #21).
  const removeAll = (block: boolean) =>
    run(() => ids.forEach((id) => remove.mutate({ id, block })));

  return (
    <div
      ref={ref}
      role="menu"
      className="fixed z-50 min-w-44 rounded-md border border-border bg-surface py-1 text-xs text-fg-muted shadow-xl"
      style={{ left: pos.x, top: pos.y }}
    >
      <div className="truncate px-3 py-1 text-[10px] text-fg-dim" title={heading}>
        {heading}
      </div>
      <div className="my-1 border-t border-border" />

      {/* Open + Copy path are single-asset only; the rest apply to the whole target set. */}
      {single && (
        <Item icon={<Search size={13} />} label="Open" onClick={() => run(() => patch({ selected: ids[0] }))} />
      )}
      {/* Reanalyze forces a re-run (`force: true`) so a deliberate per-asset click is never a silent
          no-op on an already-up-to-date asset; it still analyses never-analysed targets too. */}
      <Item
        icon={<Sparkles size={13} />}
        label={single ? "Reanalyze" : `Reanalyze ${assets.length}`}
        onClick={() => run(() => analyze.mutate({ assets: ids, force: true }))}
      />
      {thumbableIds.length > 0 && (
        <Item
          icon={<RefreshCw size={13} />}
          label={
            thumbableIds.length === 1 ? "Regenerate thumbnail" : `Regenerate ${thumbableIds.length} thumbnails`
          }
          onClick={() => run(() => regenerateThumbnail.mutate(thumbableIds))}
        />
      )}
      <Item
        icon={<FileCog size={13} />}
        label={single ? "Convert…" : `Convert ${assets.length}…`}
        onClick={() => run(() => onConvert(assets))}
      />

      {/* Add to collection — submenu of manual collections (smart folders are query-driven). */}
      <div
        className="relative"
        onMouseEnter={() => setSubmenu(true)}
        onMouseLeave={() => setSubmenu(false)}
      >
        <Item
          icon={<FolderPlus size={13} />}
          label="Add to collection"
          chevron
          onClick={() => setSubmenu((s) => !s)}
        />
        {submenu && (
          <div className="absolute top-0 left-full -mt-1 ml-0.5 min-w-40 rounded-md border border-border bg-surface py-1 shadow-xl">
            {manualCollections.length === 0 ? (
              <div className="px-3 py-1.5 text-[11px] text-fg-dim italic">No collections yet</div>
            ) : (
              manualCollections.map((c) => (
                <Item
                  key={c.id}
                  label={c.name}
                  onClick={() => run(() => members.mutate({ id: c.id, members: { add: ids } }))}
                />
              ))
            )}
          </div>
        )}
      </div>

      {single && (
        <Item
          icon={<ClipboardCopy size={13} />}
          label="Copy path"
          onClick={() => run(() => void copyPath())}
        />
      )}

      {/* Remove / remove + block (issue #21) — destructive to the catalog row (never the source
          file), so it takes an explicit second click to reveal the confirm choices. */}
      <div className="my-1 border-t border-border" />
      {confirming ? (
        <div className="px-3 py-1.5">
          <p className="mb-2 text-[10px] text-fg-dim">
            Remove {single ? "this asset" : `${assets.length} assets`} from the catalog? The source
            file{single ? "" : "s"} won’t be deleted. <span className="text-fg-muted">Block</span>{" "}
            also removes every byte-identical copy and skips those bytes on future scans.
          </p>
          <div className="flex flex-col gap-1">
            <button
              type="button"
              onClick={() => removeAll(false)}
              className="flex items-center gap-2 rounded px-2 py-1.5 text-left text-danger hover:bg-danger/10 coarse:min-h-11"
            >
              <Trash2 size={13} /> Remove
            </button>
            <button
              type="button"
              onClick={() => removeAll(true)}
              className="flex items-center gap-2 rounded px-2 py-1.5 text-left text-danger hover:bg-danger/10 coarse:min-h-11"
            >
              <Ban size={13} /> Remove + block from rescan
            </button>
            <button
              type="button"
              onClick={() => setConfirming(false)}
              className="rounded px-2 py-1.5 text-left hover:bg-surface-2 hover:text-fg coarse:min-h-11"
            >
              Cancel
            </button>
          </div>
        </div>
      ) : (
        <button
          type="button"
          role="menuitem"
          onClick={() => setConfirming(true)}
          className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-danger hover:bg-danger/10 coarse:min-h-11"
        >
          <span className="flex w-4 shrink-0 justify-center">
            <Trash2 size={13} />
          </span>
          <span className="flex-1 truncate">{single ? "Remove…" : `Remove ${assets.length}…`}</span>
        </button>
      )}
    </div>
  );
}

function Item({
  icon,
  label,
  onClick,
  chevron,
}: {
  icon?: React.ReactNode;
  label: string;
  onClick: () => void;
  chevron?: boolean;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      onClick={onClick}
      className="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-surface-2 hover:text-fg coarse:min-h-11"
    >
      <span className="flex w-4 shrink-0 justify-center text-fg-dim">{icon}</span>
      <span className="flex-1 truncate">{label}</span>
      {chevron && <span className="text-fg-dim">›</span>}
    </button>
  );
}
