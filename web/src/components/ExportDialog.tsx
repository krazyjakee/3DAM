// Export / manifest dialog (issues #9/#114). Submits a background export job and closes; the
// persistent status/history surfaces own progress, cancellation, and the terminal report. Pick a
// output path, scoped to either the current multi-selection or the current search. Destinations are
// server-side paths — for a local-first server that is the user's own disk, so this is "write a
// manifest to <path>", not a browser download. Shows the returned report (files written + count).

import { useState } from "react";
import { FileDown } from "lucide-react";
import { useExport } from "@/api/queries";
import { ApiError } from "@/api/client";
import { Modal } from "@/lib/dialogs";
import type { ExportFormat, QueryRequest } from "@/api/types";

/** What the export is scoped to: an explicit multi-selection, a collection, or the faceted query. */
export type ExportScope =
  | { assets: string[] }
  | { collection: string }
  | { query: QueryRequest };

const FORMATS: { value: ExportFormat; label: string; hint: string; dir: boolean }[] = [
  { value: "json", label: "JSON", hint: "single { assets: […] } document", dir: false },
  { value: "csv", label: "CSV", hint: "one row per asset", dir: false },
  { value: "sidecar", label: "Sidecar", hint: "one <name>.json per asset in a folder", dir: true },
];

export function ExportDialog({ scope, onClose }: { scope: ExportScope; onClose: () => void }) {
  const run = useExport();
  const [format, setFormat] = useState<ExportFormat>("json");
  const [output, setOutput] = useState("");
  const [attributionOnly, setAttributionOnly] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const isDir = FORMATS.find((f) => f.value === format)?.dir;
  const scopeLabel =
    "assets" in scope
      ? `${scope.assets.length} selected asset${scope.assets.length === 1 ? "" : "s"}`
      : "collection" in scope
        ? "the selected collection"
        : "the current search";

  const submit = () => {
    setErr(null);
    if (!output.trim()) return setErr("An output path is required.");
    run.mutate(
      { ...scope, format, output: output.trim(), attribution_only: attributionOnly },
      {
        onSuccess: onClose,
        onError: (e) => setErr(e instanceof ApiError ? e.message : String(e)),
      },
    );
  };

  return (
    <Modal
      title="Export manifest"
      icon={<FileDown size={15} />}
      labelledBy="export-dialog-title"
      onClose={onClose}
    >
      <div className="flex flex-col gap-3">
            <p className="text-[11px] text-fg-dim">
              Exporting <span className="text-fg-muted">{scopeLabel}</span> to a path on the server.
            </p>

            <div>
              <label className="mb-1 block text-[11px] text-fg-muted">Format</label>
              <select
                className="field"
                aria-label="Format"
                value={format}
                onChange={(e) => setFormat(e.target.value as ExportFormat)}
              >
                {FORMATS.map((f) => (
                  <option key={f.value} value={f.value}>
                    {f.label} — {f.hint}
                  </option>
                ))}
              </select>
            </div>

            <div>
              <label className="mb-1 block text-[11px] text-fg-muted">
                {isDir ? "Output directory" : "Output file"}
              </label>
              <input
                className="field"
                placeholder={isDir ? "/home/me/exports/manifest" : `/home/me/manifest.${format}`}
                value={output}
                onChange={(e) => setOutput(e.target.value)}
                spellCheck={false}
              />
            </div>

            <label className="flex items-center gap-2 text-[11px] text-fg-muted">
              <input
                type="checkbox"
                checked={attributionOnly}
                onChange={(e) => setAttributionOnly(e.target.checked)}
              />
              Attribution only (license / credit fields — a credits list)
            </label>

            {err && <p className="text-[11px] text-danger">{err}</p>}

            <div className="flex justify-end gap-2">
              <button className="btn" onClick={onClose}>
                Cancel
              </button>
              <button
                className="btn btn-accent"
                onClick={submit}
                disabled={run.isPending}
              >
                {run.isPending ? "Submitting…" : "Export in background"}
              </button>
            </div>
      </div>
    </Modal>
  );
}
