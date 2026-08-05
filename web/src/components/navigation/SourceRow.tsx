// One source row in the sidebar (issue #168, parent #96) — extracted from `Navigation.tsx`.
//
// A source row is the rail's densest single unit: a disclosure that lazily reveals the folder tree,
// a status dot that carries state by shape as well as colour, and three hover-revealed actions
// (share/rescan/remove). It also owns the one piece of local state in the sources list — whether its
// folder tree is expanded — which is transient UI, deliberately not part of the linkable view state.
//
// Everything else stays a prop: `Navigation` decides what selecting, rescanning, sharing, and
// removing mean, so this file never touches view state or mutations.

import { useState } from "react";
import { ChevronRight, RefreshCw, Share2, Trash2 } from "lucide-react";
import type { SourceInfo } from "@/api/types";
import { sourceStateLabel } from "@/lib/format";
import type { WriteGate } from "@/lib/write-gate";
import { FolderTree } from "../FolderTree";

export function SourceRow({
  source,
  gate,
  active,
  removing,
  onSelect,
  onRescan,
  onRemove,
  onShare,
  onNavigate,
}: {
  source: SourceInfo;
  gate: WriteGate["gate"];
  active: boolean;
  removing: boolean;
  onSelect: () => void;
  onRescan: () => void;
  onRemove: () => void;
  /** Sharing (issue #42) — present only for admins with the accounts flag on. */
  onShare?: () => void;
  onNavigate?: () => void;
}) {
  const scanning = source.state === "scanning";
  const errored = typeof source.state === "object";
  // A federated peer (issue #39) is another 3DAM instance, not a file tree: no folders to expand,
  // nothing to rescan (its catalog is queried live via the fan-out), and its asset_count stays 0.
  const peer = source.kind === "federated";
  // Folder navigation (issue #66): a source row is expandable to reveal its directory tree, lazily
  // loaded. Kept local — the choice is transient UI, not part of the linkable view state.
  const [open, setOpen] = useState(false);
  return (
    <>
    <div
      className="group flex items-center gap-1 pr-3 pl-1 py-1 text-xs"
      style={{
        background: active ? "var(--color-accent-muted)" : "transparent",
        color: active ? "var(--color-accent)" : "var(--color-fg-muted)",
      }}
    >
      {/* Disclosure — expand the source's folder tree (issue #66). Separate from selecting the
          source, so a user can browse into folders without first scoping to the whole source. */}
      {peer ? (
        <span className="w-3 shrink-0" aria-hidden="true" />
      ) : (
        <button
          onClick={() => setOpen((o) => !o)}
          aria-expanded={open}
          aria-label={open ? `Collapse ${source.name} folders` : `Expand ${source.name} folders`}
          className="flex items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
        >
          <ChevronRight
            size={12}
            className="transition-transform"
            style={{ transform: open ? "rotate(90deg)" : "none" }}
          />
        </button>
      )}
      <button
        className="flex min-w-0 flex-1 items-center gap-2 text-left coarse:min-h-11"
        onClick={onSelect}
      >
        {/* State is also carried by shape (filled circle / hollow ring / square), not colour alone,
            and exposed to assistive tech — issue #28. */}
        <span
          role="img"
          aria-label={`Source status: ${sourceStateLabel(source.state)}`}
          title={sourceStateLabel(source.state)}
          className={`h-2 w-2 shrink-0 ${errored ? "rounded-[1px]" : "rounded-full"} ${
            scanning ? "animate-pulse border" : ""
          }`}
          style={{
            background: errored
              ? "var(--color-danger)"
              : scanning
                ? "transparent"
                : "var(--color-lic-permissive)",
            borderColor: scanning ? "var(--color-warn)" : undefined,
          }}
        />
        <span className="truncate" title={source.uri}>
          {source.name}
        </span>
        {/* kind tag — a federated source is a live peer, not scanned files, so mark it and skip
            the (always-0) asset count that would misread as an empty source. */}
        {peer ? (
          <span
            className="shrink-0 rounded bg-surface-2 px-1 text-[10px] tracking-wide text-fg-dim uppercase"
            title="Federated peer — searched live"
          >
            peer
          </span>
        ) : (
          <span className="text-[10px] text-fg-dim tabular-nums">{source.stats.asset_count}</span>
        )}
      </button>
      {/* Hover-reveal under a mouse; always visible on touch, where there is no hover. */}
      <div className="hidden items-center gap-1 group-hover:flex group-focus-within:flex coarse:flex">
        {/* Sharing (issue #42): admin-only, so it bypasses the write gate — an admin always may. */}
        {onShare && (
          <button
            className="flex items-center justify-center text-fg-dim hover:text-accent coarse:min-h-11 coarse:min-w-11"
            aria-label={`Share source ${source.name}`}
            title="Share…"
            onClick={onShare}
          >
            <Share2 size={12} />
          </button>
        )}
        {!peer && (
          <button
            className="flex items-center justify-center text-fg-dim hover:text-accent disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:text-fg-dim coarse:min-h-11 coarse:min-w-11"
            aria-label="Quick rescan this source (changed files only)"
            onClick={onRescan}
            {...gate({ title: "Quick rescan — changed files only (full re-scan lives in Administration)" })}
          >
            <RefreshCw size={12} className={scanning ? "animate-spin" : ""} />
          </button>
        )}
        <button
          className="flex items-center justify-center text-fg-dim hover:text-danger disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:text-fg-dim coarse:min-h-11 coarse:min-w-11"
          aria-label="Remove source"
          onClick={onRemove}
          {...gate({ disabled: removing, title: "Remove" })}
        >
          <Trash2 size={12} className={removing ? "animate-pulse" : ""} />
        </button>
      </div>
    </div>
    {open && <FolderTree source={source.id} prefix="" depth={0} onNavigate={onNavigate} />}
    </>
  );
}
