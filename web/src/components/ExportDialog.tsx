// Export / manifest dialog (issues #9/#114). Submits a background export job and closes; the
// persistent status/history surfaces own progress, cancellation, and the terminal report. Pick a
// output path, scoped to either the current multi-selection or the current search. Destinations are
// server-side paths — for a local-first server that is the user's own disk, so this is "write a
// manifest to <path>", not a browser download. Shows the returned report (files written + count).

import { useState } from "react";
import { FileDown } from "lucide-react";
import { useExport, useManagedExport } from "@/api/queries";
import { ApiError } from "@/api/client";
import { Modal } from "@/lib/dialogs";
import type { ExportFormat, QueryRequest } from "@/api/types";
import { hasLocalFilesystemAccess, pickDirectory, saveFile } from "@/lib/tauri";

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
  const runManaged = useManagedExport();
  const [format, setFormat] = useState<ExportFormat>("json");
  const [output, setOutput] = useState("");
  const [attributionOnly, setAttributionOnly] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const localPaths = hasLocalFilesystemAccess();
  const [delivery, setDelivery] = useState<"download" | "server_path">(
    localPaths ? "server_path" : "download",
  );

  const isDir = FORMATS.find((f) => f.value === format)?.dir;
  const scopeLabel =
    "assets" in scope
      ? `${scope.assets.length} selected asset${scope.assets.length === 1 ? "" : "s"}`
      : "collection" in scope
        ? "the selected collection"
        : "the current search";

  const browseOutput = async () => {
    setErr(null);
    try {
      const picked = isDir
        ? await pickDirectory("Choose a sidecar export destination")
        : await saveFile("Choose a manifest destination", `manifest.${format}`);
      if (picked) setOutput(picked);
    } catch (e) {
      setErr(String(e));
    }
  };

  const submit = () => {
    setErr(null);
    if (delivery === "server_path" && !output.trim())
      return setErr("An output path is required.");
    const common = { ...scope, format, attribution_only: attributionOnly };
    const options = {
      onSuccess: onClose,
      onError: (e: unknown) => setErr(e instanceof ApiError ? e.message : String(e)),
    };
    if (delivery === "download") runManaged.mutate(common, options);
    else run.mutate({ ...common, output: output.trim() }, options);
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
              Exporting <span className="text-fg-muted">{scopeLabel}</span>.
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
              {!localPaths && (
                <div className="mb-2 flex overflow-hidden rounded border border-border">
                  <button
                    type="button"
                    className="flex-1 px-3 py-1 text-xs coarse:min-h-11"
                    aria-pressed={delivery === "download"}
                    onClick={() => setDelivery("download")}
                  >
                    Download in browser
                  </button>
                  <button
                    type="button"
                    className="flex-1 border-l border-border px-3 py-1 text-xs coarse:min-h-11"
                    aria-pressed={delivery === "server_path"}
                    onClick={() => setDelivery("server_path")}
                  >
                    Write on server
                  </button>
                </div>
              )}
              {delivery === "download" ? (
                <p className="rounded border border-border bg-surface-2 p-2 text-[10px] text-fg-dim">
                  3DAM stores the result in its managed export area. Download it from Job history
                  when complete{isDir ? " as one ZIP package" : ""}; no server path is required.
                </p>
              ) : (
                <>
                  <label className="mb-1 block text-[11px] text-fg-muted">
                    {isDir ? "Output directory" : "Output file"}{" "}
                    {localPaths ? "on this computer" : "on the 3DAM server"}
                  </label>
                  <div className="flex gap-2">
                    <input
                      className="field min-w-0"
                      placeholder={
                        localPaths
                          ? isDir
                            ? "/Users/you/Exports/sidecars"
                            : `/Users/you/Exports/manifest.${format}`
                          : isDir
                            ? "/srv/3dam/exports/sidecars"
                            : `/srv/3dam/exports/manifest.${format}`
                      }
                      value={output}
                      onChange={(e) => setOutput(e.target.value)}
                      spellCheck={false}
                    />
                    {localPaths && (
                      <button type="button" className="btn shrink-0" onClick={browseOutput}>
                        Browse…
                      </button>
                    )}
                  </div>
                  <p className="mt-1 text-[10px] text-fg-dim">
                    {localPaths
                      ? "The embedded server writes this path on this computer."
                      : "The server machine writes this path; it is not a path on the browser's computer."}
                  </p>
                </>
              )}
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
                disabled={run.isPending || runManaged.isPending}
              >
                {run.isPending || runManaged.isPending ? "Submitting…" : "Export in background"}
              </button>
            </div>
      </div>
    </Modal>
  );
}
