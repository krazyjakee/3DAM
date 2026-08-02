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
  useCan,
  useCollections,
  useCollectionMembers,
  useRegenerateThumbnail,
  useRemoveAsset,
} from "@/api/queries";
import type { AssetSummary } from "@/api/types";
import { AUTH_COPY } from "@/lib/auth";
import { copyText } from "@/lib/clipboard";
import { localOnly, PEER_READONLY_SET, peerReadOnlyTitle } from "@/lib/origin";
import { errorMessage, toast } from "@/lib/toast";
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
  const canWrite = useCan("write");
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
    requestAnimationFrame(() =>
      ref.current?.querySelector<HTMLElement>("[role='menuitem']:not(:disabled)")?.focus(),
    );
  }, [menu]);

  if (!menu) return null;
  const assets = menu.assets;
  const ids = assets.map((a) => a.id);
  const single = assets.length === 1;
  const heading = single ? assets[0].name : `${assets.length} items`;
  // Federated targets are read-only references (tech-spec 07 §7.4) — no local write path mutates
  // them, so mutations act on the local subset only, and disable entirely (with the standard
  // origin-gate tooltip) when everything clicked lives on a peer.
  const localTargets = localOnly(assets);
  const localIds = localTargets.map((a) => a.id);
  const peerOnly = localIds.length === 0;
  const peerExcluded = assets.length - localTargets.length;
  const peerTitle = single ? peerReadOnlyTitle(assets[0].origin) : PEER_READONLY_SET;
  // Only image, 3D and video assets have a server thumbnail to rebuild (audio and documents use the
  // honest typed tile); hide "Regenerate thumbnail" when nothing in the target set can produce one.
  const thumbableIds = localTargets
    .filter((a) => a.media === "image" || a.media === "model" || a.media === "video")
    .map((a) => a.id);

  const run = (fn: () => void) => {
    fn();
    onClose();
  };

  const manualCollections = (collections.data ?? []).filter((c) => c.kind === "manual");

  const copyPath = async () => {
    try {
      const target = assets[0];
      const full = await api.getAsset(
        ids[0],
        typeof target.origin === "object" ? target.source_id : null,
      );
      await copyText(full.path, "Path");
    } catch (error) {
      // The clipboard helper owns clipboard failures. Reaching here means the path itself could not
      // be loaded, so there is no value to expose for manual copying.
      toast.error(`Couldn’t load path: ${errorMessage(error)}`);
    }
  };

  // Remove the target set from the catalog; `block` also blocks each content hash from re-import.
  // The source files are never touched — only catalog rows (issue #21).
  const removeAll = (block: boolean) =>
    run(() => localIds.forEach((id) => remove.mutate({ id, block })));

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
        <Item icon={<Search size={13} />} label="Open" onClick={() => run(() => patch({
          selected: ids[0],
          owner: typeof assets[0].origin === "object" ? assets[0].source_id : null,
        }))} />
      )}
      {/* Reanalyze forces a re-run (`force: true`) so a deliberate per-asset click is never a silent
          no-op on an already-up-to-date asset; it still analyses never-analysed targets too. */}
      <Item
        icon={<Sparkles size={13} />}
        label={single ? "Reanalyze" : `Reanalyze ${localIds.length}`}
        disabled={!canWrite || peerOnly}
        title={peerOnly ? peerTitle : !canWrite ? AUTH_COPY.needsWrite : undefined}
        onClick={() => run(() => analyze.mutate({ assets: localIds, force: true }))}
      />
      {thumbableIds.length > 0 && (
        <Item
          icon={<RefreshCw size={13} />}
          label={
            thumbableIds.length === 1 ? "Regenerate thumbnail" : `Regenerate ${thumbableIds.length} thumbnails`
          }
          disabled={!canWrite}
          title={!canWrite ? AUTH_COPY.needsWrite : undefined}
          onClick={() => run(() => regenerateThumbnail.mutate(thumbableIds))}
        />
      )}
      <Item
        icon={<FileCog size={13} />}
        label={single ? "Convert…" : `Convert ${localTargets.length}…`}
        disabled={!canWrite || peerOnly}
        title={peerOnly ? peerTitle : !canWrite ? AUTH_COPY.needsWrite : undefined}
        onClick={() => run(() => onConvert(localTargets))}
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
          disabled={!canWrite || peerOnly}
          title={peerOnly ? peerTitle : !canWrite ? AUTH_COPY.needsWrite : undefined}
          onClick={() => setSubmenu((s) => !s)}
        />
        {/* Hover opens the submenu even when the trigger is disabled — keep it shut for a
            peer-only target set (there is nothing local to add). */}
        {submenu && !peerOnly && (
          <div className="absolute top-0 left-full -mt-1 ml-0.5 min-w-40 rounded-md border border-border bg-surface py-1 shadow-xl">
            {manualCollections.length === 0 ? (
              <div className="px-3 py-1.5 text-[11px] text-fg-dim italic">No collections yet</div>
            ) : (
              manualCollections.map((c) => (
                <Item
                  key={c.id}
                  label={c.name}
                  onClick={() => run(() => members.mutate({ id: c.id, members: { add: localIds } }))}
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
            Remove {localTargets.length === 1 ? "this asset" : `${localTargets.length} assets`} from
            the catalog? The source file{localTargets.length === 1 ? "" : "s"} won’t be deleted.{" "}
            <span className="text-fg-muted">Block</span> also removes every byte-identical copy and
            skips those bytes on future scans.
            {peerExcluded > 0 && (
              <>
                {" "}
                {peerExcluded === 1
                  ? "1 selected item lives"
                  : `${peerExcluded} selected items live`}{" "}
                on a federated peer and won’t be touched.
              </>
            )}
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
          disabled={!canWrite || peerOnly}
          title={peerOnly ? peerTitle : !canWrite ? AUTH_COPY.needsWrite : undefined}
          onClick={() => setConfirming(true)}
          className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-danger hover:bg-danger/10 disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent coarse:min-h-11"
        >
          <span className="flex w-4 shrink-0 justify-center">
            <Trash2 size={13} />
          </span>
          <span className="flex-1 truncate">
            {single ? "Remove…" : `Remove ${localTargets.length}…`}
          </span>
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
  disabled,
  title,
}: {
  icon?: React.ReactNode;
  label: string;
  onClick: () => void;
  chevron?: boolean;
  disabled?: boolean;
  title?: string;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      onClick={onClick}
      disabled={disabled}
      title={title}
      className="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-surface-2 hover:text-fg disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-fg-muted coarse:min-h-11"
    >
      <span className="flex w-4 shrink-0 justify-center text-fg-dim">{icon}</span>
      <span className="flex-1 truncate">{label}</span>
      {chevron && <span className="text-fg-dim">›</span>}
    </button>
  );
}
