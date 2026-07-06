// Per-item context menu for asset tiles/rows (issue #20). Opened by right-click or touch long-press
// (see useLongPress) from the Browser, which owns the open state and passes the target asset here.
// The action set lives in ONE place so future actions (convert #5, remove/block #21) slot in as
// entries — today: open, analyze, add-to-collection (submenu), copy path. Actions honour the clicked
// asset; batch-over-selection lands with multi-select (#22).

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { FolderPlus, Search, Sparkles, ClipboardCopy } from "lucide-react";
import { api } from "@/api/client";
import { useAnalyze, useCollections, useCollectionMembers } from "@/api/queries";
import type { AssetSummary } from "@/api/types";
import { useViewState } from "@/lib/view-state";

export interface MenuState {
  x: number;
  y: number;
  asset: AssetSummary;
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

export function ContextMenu({ menu, onClose }: { menu: MenuState | null; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ x: 0, y: 0 });
  const { patch } = useViewState();
  const analyze = useAnalyze();
  const collections = useCollections();
  const members = useCollectionMembers();
  const [submenu, setSubmenu] = useState(false);

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
  }, [menu]);

  if (!menu) return null;
  const asset = menu.asset;

  const run = (fn: () => void) => {
    fn();
    onClose();
  };

  const manualCollections = (collections.data ?? []).filter((c) => c.kind === "manual");

  const copyPath = async () => {
    try {
      const full = await api.getAsset(asset.id);
      await navigator.clipboard?.writeText(full.path);
    } catch {
      /* clipboard blocked / fetch failed — fail-soft, no crash */
    }
  };

  return (
    <div
      ref={ref}
      role="menu"
      className="fixed z-50 min-w-44 rounded-md border border-border bg-surface py-1 text-xs text-fg-muted shadow-xl"
      style={{ left: pos.x, top: pos.y }}
    >
      <div className="truncate px-3 py-1 text-[10px] text-fg-dim" title={asset.name}>
        {asset.name}
      </div>
      <div className="my-1 border-t border-border" />

      <Item icon={<Search size={13} />} label="Open" onClick={() => run(() => patch({ selected: asset.id }))} />
      <Item
        icon={<Sparkles size={13} />}
        label="Analyze"
        onClick={() => run(() => analyze.mutate({ assets: [asset.id] }))}
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
                  onClick={() =>
                    run(() => members.mutate({ id: c.id, members: { add: [asset.id] } }))
                  }
                />
              ))
            )}
          </div>
        )}
      </div>

      <div className="my-1 border-t border-border" />
      <Item
        icon={<ClipboardCopy size={13} />}
        label="Copy path"
        onClick={() => run(() => void copyPath())}
      />
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
