// The Inspector's "Find similar" panel (issue #166, parent #96) — extracted from `Inspector.tsx`.
// One query (`useSimilar`), one mutation (`useAnalyze`), and one `asset` prop. It also owns the
// `find-similar` shortcut, scoped to whichever of the responsive rail/drawer copies is visible.

import { useEffect, useRef, useState } from "react";
import { Sparkles } from "lucide-react";
import { useAnalyze, useSimilar } from "@/api/queries";
import type { Asset, SimilarHit } from "@/api/types";
import { peerReadOnlyTitle } from "@/lib/origin";
import { shortcutLabel, SHORTCUT_EVENT, type ShortcutId } from "@/lib/shortcuts";
import { useViewState } from "@/lib/view-state";
import { Thumbnail } from "../Thumbnail";
import { Group } from "./primitives";

/** "Find similar" (tech-spec 05 §3): an opt-in ranked strip of neighbours by embedding cosine.
 *  Un-analyzed assets have no vector, so we offer to analyze first rather than query into the void.
 *  Each hit is selectable — clicking swaps the Inspector to that asset (and back/forward works, since
 *  selection lives in the URL). */
export function SimilarSection({ asset }: { asset: Asset }) {
  const { patch } = useViewState();
  const [open, setOpen] = useState(false);
  const analyzed = asset.timestamps.analyzed != null;
  const analyze = useAnalyze();
  const peerTitle = peerReadOnlyTitle(asset.summary.origin);
  const similar = useSimilar(asset.summary.id, open && analyzed);
  const sectionRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onShortcut = (event: Event) => {
      if ((event as CustomEvent<ShortcutId>).detail !== "find-similar") return;
      // Inspector content exists twice responsively; act only in the visible rail/drawer instance.
      if (!sectionRef.current || sectionRef.current.getClientRects().length === 0) return;
      if (analyzed) setOpen(true);
      else if (!analyze.isPending && !peerTitle) analyze.mutate({ assets: [asset.summary.id] });
    };
    window.addEventListener(SHORTCUT_EVENT, onShortcut);
    return () => window.removeEventListener(SHORTCUT_EVENT, onShortcut);
  }, [analyze, analyzed, asset.summary.id, peerTitle]);

  if (!analyzed) {
    return (
      <div ref={sectionRef}>
        <Group title="Similar">
          <p className="text-[11px] text-fg-dim italic">
            Analyze this asset to find visually similar ones.
          </p>
          <button
            className="btn mt-2 disabled:cursor-not-allowed disabled:opacity-40"
            disabled={analyze.isPending || !!peerTitle}
            title={peerTitle ?? `Analyze and find similar (${shortcutLabel("find-similar")})`}
            aria-keyshortcuts="S"
            onClick={() => analyze.mutate({ assets: [asset.summary.id] })}
          >
            <Sparkles size={12} />
            {analyze.isPending ? "Analyzing…" : "Analyze now"}
          </button>
        </Group>
      </div>
    );
  }

  if (!open) {
    return (
      <div ref={sectionRef}>
        <Group title="Similar">
          <button
            className="btn"
            onClick={() => setOpen(true)}
            title={`Find similar (${shortcutLabel("find-similar")})`}
            aria-keyshortcuts="S"
          >
            <Sparkles size={12} />
            Find similar
          </button>
        </Group>
      </div>
    );
  }

  const hits = similar.data?.items ?? [];
  return (
    <div ref={sectionRef}>
      <Group title="Similar">
        {similar.isLoading ? (
          <p className="text-[11px] text-fg-dim">Searching…</p>
        ) : similar.isError ? (
          <p className="text-[11px] text-danger">Could not search for similar assets.</p>
        ) : hits.length === 0 ? (
          <p className="text-[11px] text-fg-dim italic">No similar assets found.</p>
        ) : (
          <div className="grid grid-cols-3 gap-1.5">
            {hits.map((hit) => (
              <SimilarTile
                key={hit.asset.id}
                hit={hit}
                onOpen={() => patch({
                  selected: hit.asset.id,
                  owner: typeof hit.asset.origin === "object" ? hit.asset.source_id : null,
                })}
              />
            ))}
          </div>
        )}
      </Group>
    </div>
  );
}

function SimilarTile({ hit, onOpen }: { hit: SimilarHit; onOpen: () => void }) {
  const pct = Math.round(hit.score * 100);
  return (
    <button
      className="group flex flex-col overflow-hidden rounded border border-border bg-bg text-left transition-colors hover:border-border-strong coarse:min-h-11"
      title={`${hit.asset.name} · ${pct}% similar · ${hit.space}`}
      onClick={onOpen}
    >
      <span className="aspect-square w-full">
        <Thumbnail asset={hit.asset} size={32} />
      </span>
      <span className="flex items-center justify-between gap-1 px-1 py-0.5">
        <span className="min-w-0 truncate text-[10px] text-fg-muted">{hit.asset.name}</span>
        <span className="shrink-0 text-[10px] font-medium text-accent">{pct}%</span>
      </span>
    </button>
  );
}
