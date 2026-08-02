// Per-item context menu for asset tiles/rows (issue #20). Opened by right-click or touch long-press
// (see useLongPress) from the Browser, which owns the open state and passes the target asset here.
// The action set lives in ONE place: open, analyze, convert (#5), add-to-collection (submenu),
// copy path, and remove / remove + block (#21). Actions honour the whole target set (the clicked
// asset, or the multi-selection when the clicked item is part of it — #22).

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
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
import { menuItemIndex, menuKeyAction } from "@/lib/menu-keyboard";

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
  const submenuRef = useRef<HTMLDivElement>(null);
  const returnFocusRef = useRef<HTMLElement | null>(null);
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

  const close = useCallback(
    (restoreFocus: boolean) => {
      const returnFocus = returnFocusRef.current;
      onClose();
      if (restoreFocus) requestAnimationFrame(() => returnFocus?.focus());
    },
    [onClose],
  );

  // Close on outside pointer, scroll, or resize. Menu-local keyboard handling below owns Escape
  // so an open submenu can close independently before the root menu is dismissed.
  useEffect(() => {
    if (!menu) return;
    const onDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) close(false);
    };
    const onScroll = () => close(true);
    const onResize = () => close(true);
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onResize);
    };
  }, [menu, close]);

  // Clamp within the viewport once the menu size is known.
  useLayoutEffect(() => {
    if (!menu || !ref.current) return;
    returnFocusRef.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
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

  useLayoutEffect(() => {
    if (!menu || !confirming) return;
    requestAnimationFrame(() =>
      ref.current?.querySelector<HTMLElement>("[data-remove-choice]")?.focus(),
    );
  }, [confirming, menu]);

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

  const run = (fn: () => void, restoreFocus = true) => {
    fn();
    close(restoreFocus);
  };

  const manualCollections = (collections.data ?? []).filter((c) => c.kind === "manual");

  const directItems = (menuElement: HTMLElement): HTMLElement[] =>
    [...menuElement.querySelectorAll<HTMLElement>("[role='menuitem']")].filter(
      (item) => item.closest("[role='menu']") === menuElement,
    );

  const focusSubmenuTrigger = () =>
    ref.current?.querySelector<HTMLElement>("[aria-haspopup='menu']")?.focus();

  const openSubmenu = (moveFocus: boolean) => {
    setSubmenu(true);
    if (moveFocus) {
      requestAnimationFrame(() => {
        const submenuElement = submenuRef.current;
        if (submenuElement) directItems(submenuElement).at(0)?.focus();
      });
    }
  };

  const closeSubmenu = () => {
    setSubmenu(false);
    requestAnimationFrame(focusSubmenuTrigger);
  };

  const cancelRemove = () => {
    setConfirming(false);
    requestAnimationFrame(() =>
      ref.current?.querySelector<HTMLElement>("[data-remove-trigger]")?.focus(),
    );
  };

  const onMenuKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const target = event.target instanceof HTMLElement ? event.target : null;
    const menuElement = target?.closest<HTMLElement>("[role='menu']");
    if (!target || !menuElement || !ref.current?.contains(menuElement)) return;
    const inSubmenu = menuElement !== ref.current;
    const hasSubmenu =
      target.getAttribute("aria-haspopup") === "menu" &&
      target.getAttribute("aria-disabled") !== "true";
    const action = menuKeyAction(event.key, { inSubmenu, hasSubmenu });
    if (!action) return;

    if (action === "tab-away") {
      // Put focus back on the invoking cell before allowing native Tab/Shift+Tab to continue from
      // that meaningful location. No menu item is added to the page's tab sequence.
      returnFocusRef.current?.focus();
      onClose();
      return;
    }

    event.preventDefault();
    event.stopPropagation();
    if (action === "close-menu") {
      close(true);
      return;
    }
    if (action === "open-submenu") {
      openSubmenu(true);
      return;
    }
    if (action === "close-submenu") {
      closeSubmenu();
      return;
    }

    const items = directItems(menuElement);
    const current = items.indexOf(target);
    const next = menuItemIndex(current, items.length, action);
    if (next != null) items[next]?.focus();
  };

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
      aria-label={`Actions for ${heading}`}
      onKeyDown={onMenuKeyDown}
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
        onClick={() => {
          // Seed the dialog focus trap with the invoking asset, not this soon-to-unmount menu item,
          // so closing Convert returns to a stable cell/row.
          returnFocusRef.current?.focus();
          run(() => onConvert(localTargets), false);
        }}
      />

      {/* Add to collection — submenu of manual collections (smart folders are query-driven). */}
      <div
        className="relative"
        onMouseEnter={() => {
          if (canWrite && !peerOnly) setSubmenu(true);
        }}
        onMouseLeave={(event) => {
          if (!event.currentTarget.contains(document.activeElement)) setSubmenu(false);
        }}
      >
        <Item
          icon={<FolderPlus size={13} />}
          label="Add to collection"
          chevron
          expanded={submenu}
          disabled={!canWrite || peerOnly}
          title={peerOnly ? peerTitle : !canWrite ? AUTH_COPY.needsWrite : undefined}
          onClick={() => (submenu ? closeSubmenu() : openSubmenu(true))}
        />
        {/* Hover opens the submenu for available local actions; keyboard focus moves in with
            ArrowRight or activation. */}
        {submenu && canWrite && !peerOnly && (
          <div
            ref={submenuRef}
            role="menu"
            aria-label="Collections"
            className="absolute top-0 left-full -mt-1 ml-0.5 min-w-40 rounded-md border border-border bg-surface py-1 shadow-xl"
          >
            {manualCollections.length === 0 ? (
              <div
                role="menuitem"
                aria-disabled="true"
                tabIndex={-1}
                className="px-3 py-1.5 text-[11px] text-fg-dim italic"
              >
                No collections yet
              </div>
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
              role="menuitem"
              tabIndex={-1}
              data-remove-choice
              onClick={() => removeAll(false)}
              className="flex items-center gap-2 rounded px-2 py-1.5 text-left text-danger hover:bg-danger/10 coarse:min-h-11"
            >
              <Trash2 size={13} /> Remove
            </button>
            <button
              type="button"
              role="menuitem"
              tabIndex={-1}
              data-remove-choice
              onClick={() => removeAll(true)}
              className="flex items-center gap-2 rounded px-2 py-1.5 text-left text-danger hover:bg-danger/10 coarse:min-h-11"
            >
              <Ban size={13} /> Remove + block from rescan
            </button>
            <button
              type="button"
              role="menuitem"
              tabIndex={-1}
              data-remove-choice
              onClick={cancelRemove}
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
          tabIndex={-1}
          data-remove-trigger
          aria-disabled={!canWrite || peerOnly || undefined}
          title={peerOnly ? peerTitle : !canWrite ? AUTH_COPY.needsWrite : undefined}
          onClick={() => {
            if (canWrite && !peerOnly) setConfirming(true);
          }}
          className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-danger hover:bg-danger/10 aria-disabled:cursor-not-allowed aria-disabled:opacity-40 aria-disabled:hover:bg-transparent coarse:min-h-11"
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
  expanded,
  disabled,
  title,
}: {
  icon?: React.ReactNode;
  label: string;
  onClick: () => void;
  chevron?: boolean;
  expanded?: boolean;
  disabled?: boolean;
  title?: string;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      tabIndex={-1}
      onClick={() => {
        if (!disabled) onClick();
      }}
      aria-disabled={disabled || undefined}
      aria-haspopup={chevron ? "menu" : undefined}
      aria-expanded={chevron ? expanded : undefined}
      title={title}
      className="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-surface-2 hover:text-fg aria-disabled:cursor-not-allowed aria-disabled:opacity-40 aria-disabled:hover:bg-transparent aria-disabled:hover:text-fg-muted coarse:min-h-11"
    >
      <span className="flex w-4 shrink-0 justify-center text-fg-dim">{icon}</span>
      <span className="flex-1 truncate">{label}</span>
      {chevron && <span className="text-fg-dim">›</span>}
    </button>
  );
}
