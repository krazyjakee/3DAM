// A registered source contributes to the combined library. This row shows its status and
// management actions; source and folder filtering live in Advanced Search.

import { RefreshCw, Share2, Trash2 } from "lucide-react";
import type { SourceInfo } from "@/api/types";
import { sourceStateLabel } from "@/lib/format";
import type { WriteGate } from "@/lib/write-gate";

export function SourceRow({
  source,
  gate,
  removing,
  onRescan,
  onRemove,
  onShare,
}: {
  source: SourceInfo;
  gate: WriteGate["gate"];
  removing: boolean;
  onRescan: () => void;
  onRemove: () => void;
  /** Sharing (issue #42) — present only for admins with the accounts flag on. */
  onShare?: () => void;
}) {
  const scanning = source.state === "scanning";
  const errored = typeof source.state === "object";
  // A federated peer (issue #39) is another 3DAM instance, not a file tree: no folders to expand,
  // nothing to rescan (its catalog is queried live via the fan-out), and its asset_count stays 0.
  const peer = source.kind === "federated";
  return (
    <div className="group flex items-center gap-1 px-3 py-1 text-xs text-fg-muted">
      <div className="flex min-w-0 flex-1 items-center gap-2">
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
      </div>
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
  );
}
