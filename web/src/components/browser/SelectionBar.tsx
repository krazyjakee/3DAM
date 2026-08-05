import { useState } from "react";
import { FileCog, FileDown, ScrollText, Sparkles, Tags, X } from "lucide-react";
import { useAnalyze, useCollectionMembers, useCollections } from "@/api/queries";
import type { AssetSummary } from "@/api/types";
import { AUTH_COPY } from "@/lib/auth";
import { localOnly, PEER_READONLY_SET } from "@/lib/origin";
import { useWriteGate } from "@/lib/write-gate";
import type { ResultSelector, SelectionModel } from "@/lib/selection";
import { ConvertDialog } from "../ConvertDialog";
import { ExportDialog, type ExportScope } from "../ExportDialog";
import { LicenseDialog, type LicenseScope } from "../LicenseDialog";
import { RetagDialog, type RetagScope } from "../RetagDialog";

/** Batch-action affordance for a multi-selection (issue #10 enabler). Feature actions hang off here;
 *  today: analyze all, add all to a collection, select-all, clear. */
export function SelectionBar({
  selection,
  loaded,
  onSelectVisible,
  resultSelector,
  total,
  resultComplete,
}: {
  selection: SelectionModel;
  loaded: AssetSummary[];
  onSelectVisible: () => void;
  resultSelector: ResultSelector;
  total: number | null;
  resultComplete: boolean;
}) {
  const analyze = useAnalyze();
  const collections = useCollections();
  const members = useCollectionMembers();
  const { canWrite, gate } = useWriteGate();
  const manual = (collections.data ?? []).filter((c) => c.kind === "manual");
  const [showExport, setShowExport] = useState(false);
  const [showConvert, setShowConvert] = useState(false);
  const [showRetag, setShowRetag] = useState(false);
  const [showLicense, setShowLicense] = useState(false);
  // Federated selections are read-only references (tech-spec 07 §7.4): batch actions run against
  // the local subset, and disable when nothing selected is ours.
  const resultWide = selection.selected.kind === "results";
  const assets = selection.explicitAssets;
  const locals = localOnly(assets);
  const localIds = locals.map((a) => a.id);
  const peerCount = assets.length - locals.length;
  const peerOnly = locals.length === 0;
  const explicitOnlyTitle = resultWide
    ? "This action needs explicit loaded assets; clear result-wide selection first"
    : undefined;
  const resultLabel = resultSelector.kind === "collection" ? "collection" : "query";
  const summary = resultWide
    ? `${selection.count.toLocaleString()} ${resultLabel} results selected`
    : `${selection.count.toLocaleString()} selected`;
  const exportScope: ExportScope = selection.selected.kind === "results"
    ? selection.selected.selector.kind === "collection"
      ? { collection: selection.selected.selector.collection }
      : { query: selection.selected.selector.query }
    : { assets: localIds };
  const retagScope: RetagScope = selection.selected.kind === "results"
    ? selection.selected.selector.kind === "collection"
      ? { collection: selection.selected.selector.collection }
      : { query: selection.selected.selector.query }
    : { assets: localIds };
  // Same target resolution as retag — licence edits are the other bulk catalog write (issue #106).
  const licenseScope: LicenseScope = retagScope;

  return (
    <div className="browser-selection-bar border-b border-border bg-surface px-3 py-1.5 text-xs">
      <div className="browser-selection-summary flex min-w-0 items-center gap-2">
        <span className="shrink-0 font-medium text-fg tabular-nums">{summary}</span>
        {peerCount > 0 && (
          <span className="truncate text-fg-dim" title={PEER_READONLY_SET}>
            {peerCount} federated (read-only)
          </span>
        )}
      </div>
      <div className="browser-selection-actions flex min-w-0 flex-wrap items-center gap-2">
        <button
          className="btn disabled:cursor-not-allowed disabled:opacity-40"
          onClick={() => analyze.mutate({ assets: localIds })}
          {...gate({
            disabled: analyze.isPending || peerOnly || resultWide,
            title: explicitOnlyTitle ?? (peerOnly ? PEER_READONLY_SET : undefined),
          })}
        >
          <Sparkles size={12} /> Analyze
        </button>
        <button
          className="btn disabled:cursor-not-allowed disabled:opacity-40"
          onClick={() => setShowConvert(true)}
          {...gate({
            disabled: peerOnly || resultWide,
            title: explicitOnlyTitle ?? (peerOnly ? PEER_READONLY_SET : undefined),
          })}
        >
          <FileCog size={12} /> Convert
        </button>
        <button
          className="btn disabled:cursor-not-allowed disabled:opacity-40"
          onClick={() => setShowExport(true)}
          {...gate({ disabled: !resultWide && peerOnly, title: peerOnly && !resultWide ? PEER_READONLY_SET : undefined })}
        >
          <FileDown size={12} /> Export
        </button>
        <button
          className="btn disabled:cursor-not-allowed disabled:opacity-40"
          onClick={() => setShowRetag(true)}
          {...gate({ disabled: !resultWide && peerOnly, title: peerOnly && !resultWide ? PEER_READONLY_SET : undefined })}
        >
          <Tags size={12} /> Retag
        </button>
        <button
          className="btn disabled:cursor-not-allowed disabled:opacity-40"
          onClick={() => setShowLicense(true)}
          {...gate({ disabled: !resultWide && peerOnly, title: peerOnly && !resultWide ? PEER_READONLY_SET : undefined })}
        >
          <ScrollText size={12} /> Licence
        </button>
        {showConvert && (
          <ConvertDialog assets={locals} onClose={() => setShowConvert(false)} />
        )}
        {showExport && (
          <ExportDialog scope={exportScope} onClose={() => setShowExport(false)} />
        )}
        {showRetag && (
          <RetagDialog
            scope={retagScope}
            excludedPeers={peerCount}
            onClose={() => setShowRetag(false)}
          />
        )}
        {showLicense && (
          <LicenseDialog
            scope={licenseScope}
            excludedPeers={peerCount}
            onClose={() => setShowLicense(false)}
          />
        )}
        <select
          className="field max-w-full w-auto disabled:cursor-not-allowed disabled:opacity-40"
          aria-label="Add selection to collection"
          value=""
          disabled={manual.length === 0 || members.isPending || !canWrite || peerOnly || resultWide}
          onChange={(e) => {
            if (e.target.value) members.mutate({ id: e.target.value, members: { add: localIds } });
            e.currentTarget.value = "";
          }}
          title={
            explicitOnlyTitle ?? (peerOnly
              ? PEER_READONLY_SET
              : !canWrite
                ? AUTH_COPY.needsWrite
                : manual.length === 0
                  ? "No manual collections yet"
                  : "Add selection to a collection")
          }
        >
          <option value="" disabled>
            Add to collection…
          </option>
          {manual.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
        </select>
        <button
          className="text-fg-dim hover:text-fg coarse:min-h-11"
          onClick={onSelectVisible}
        >
          Select visible
        </button>
        <button
          className="text-fg-dim hover:text-fg coarse:min-h-11"
          onClick={() => selection.selectExplicit(loaded)}
        >
          Select loaded ({loaded.length.toLocaleString()})
        </button>
        {total !== null && total > loaded.length && (
          <button
            className="text-fg-dim hover:text-fg disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11"
            disabled={!resultComplete}
            title={
              resultComplete
                ? `Use the server-side ${resultLabel} selector; IDs are not materialized in the browser`
                : "Results are partial because a peer did not answer; retry before selecting all"
            }
            onClick={() => selection.selectResults(resultSelector, total, loaded)}
          >
            Select all {total.toLocaleString()} {resultLabel} results
          </button>
        )}
      </div>
      <button
        className="browser-selection-clear flex shrink-0 items-center gap-1 text-fg-dim hover:text-fg coarse:min-h-11"
        onClick={selection.clear}
      >
        <X size={13} /> Clear
      </button>
    </div>
  );
}
