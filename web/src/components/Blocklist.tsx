// Rescan blocklist management surface (issue #21). Lists the content hashes removed with
// "Remove + block" — the bytes a scan/watch/auto-rescan will refuse to re-import. Lifting a block
// here lets the next scan pick the content back up. Read-mostly: the only action is Unblock; the
// blocking itself happens from the Browser/Inspector context menu on a live asset.

import { Link } from "react-router-dom";
import { Ban, RotateCcw } from "lucide-react";
import { useBlocklist, useUnblock } from "@/api/queries";
import { relTime } from "@/lib/format";
import { CenteredCard } from "@/lib/ui";
import type { BlockEntry } from "@/api/types";

export function Blocklist() {
  const list = useBlocklist();
  const unblock = useUnblock();
  const data = list.data ?? [];

  return (
    <div className="mx-auto flex min-h-dvh max-w-3xl flex-col gap-5 p-6 text-sm">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2">
          <Ban size={18} className="text-accent" />
          <h1 className="text-lg font-semibold text-fg">Rescan blocklist</h1>
        </div>
        <Link to="/" className="text-accent hover:underline">
          ← Back to library
        </Link>
      </header>

      <p className="text-xs text-fg-dim">
        Content hashes removed with <span className="text-fg-muted">Remove + block</span>. A scan,
        watch, or auto-rescan skips these bytes, so the same file never returns to the catalog.
        Lifting a block lets the next scan re-import it.
      </p>

      <div className="text-xs text-fg-dim tabular-nums">
        {data.length} blocked hash{data.length === 1 ? "" : "es"}
      </div>

      {list.isLoading ? (
        <CenteredCard>Loading blocklist…</CenteredCard>
      ) : list.isError ? (
        <CenteredCard tone="danger">Failed to load — is `3dam serve` running?</CenteredCard>
      ) : data.length === 0 ? (
        <CenteredCard>
          Nothing blocked. Use <span className="text-fg-muted">Remove + block</span> from an asset’s
          context menu to keep specific content out of future scans.
        </CenteredCard>
      ) : (
        <ul className="flex flex-col divide-y divide-border rounded border border-border">
          {data.map((e) => (
            <Row key={e.hash} entry={e} onUnblock={() => unblock.mutate(e.hash)} busy={unblock.isPending} />
          ))}
        </ul>
      )}
    </div>
  );
}

function Row({
  entry,
  onUnblock,
  busy,
}: {
  entry: BlockEntry;
  onUnblock: () => void;
  busy: boolean;
}) {
  return (
    <li className="flex items-center gap-3 px-3 py-2">
      <div className="min-w-0 flex-1">
        <p className="truncate text-fg" title={entry.label ?? undefined}>
          {entry.label || <span className="text-fg-dim italic">unnamed</span>}
        </p>
        <p className="truncate font-mono text-[10px] text-fg-dim" title={entry.hash}>
          {entry.hash}
        </p>
      </div>
      <span className="shrink-0 text-[10px] text-fg-dim tabular-nums">
        {relTime(entry.blocked_at / 1000)}
      </span>
      <button
        type="button"
        onClick={onUnblock}
        disabled={busy}
        className="btn flex shrink-0 items-center gap-1.5"
        title="Lift the block so a later scan can re-import this content"
      >
        <RotateCcw size={13} /> Unblock
      </button>
    </li>
  );
}

