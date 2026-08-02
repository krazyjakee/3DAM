import { useEffect, useMemo, useState } from "react";
import { AlertTriangle, Sparkles } from "lucide-react";
import { ApiError } from "@/api/client";
import {
  useCollections,
  useCreateCollection,
  useSources,
  useUpdateCollection,
} from "@/api/queries";
import type { QueryRequest } from "@/api/types";
import { Modal } from "@/lib/dialogs";
import { savedQuery, summarizeQuery } from "@/lib/query-summary";

export function SmartFolderDialog({
  query,
  onClose,
}: {
  query: QueryRequest;
  onClose: () => void;
}) {
  const collections = useCollections();
  const sources = useSources();
  const create = useCreateCollection();
  const update = useUpdateCollection();
  const smartFolders = useMemo(
    () => (collections.data ?? []).filter((collection) => collection.kind === "smart"),
    [collections.data],
  );
  const [mode, setMode] = useState<"create" | "update">("create");
  const [selected, setSelected] = useState("");
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const cleanQuery = useMemo(() => savedQuery(query), [query]);
  const summary = useMemo(
    () => summarizeQuery(cleanQuery, sources.data),
    [cleanQuery, sources.data],
  );
  const pending = create.isPending || update.isPending;

  useEffect(() => {
    if (mode !== "update") return;
    const collection = smartFolders.find((folder) => folder.id === selected);
    if (collection) setName(collection.name);
  }, [mode, selected, smartFolders]);

  const submit = () => {
    setError(null);
    const cleanName = name.trim();
    if (!cleanName) return setError("A name is required.");
    const onError = (reason: unknown) =>
      setError(reason instanceof ApiError ? reason.message : String(reason));
    if (mode === "create") {
      create.mutate(
        { name: cleanName, kind: "smart", query: cleanQuery },
        { onSuccess: onClose, onError },
      );
      return;
    }
    if (!selected) return setError("Choose a smart folder to update.");
    update.mutate(
      { id: selected, patch: { name: cleanName, query: cleanQuery } },
      { onSuccess: onClose, onError },
    );
  };

  return (
    <Modal
      title="Save search as smart folder"
      icon={<Sparkles size={15} />}
      labelledBy="smart-folder-dialog-title"
      onClose={onClose}
      wide
      scroll
    >
      <div className="flex flex-col gap-3">
        <p className="text-[11px] text-fg-muted">
          Smart folders update automatically as assets begin or stop matching. Manual collections
          keep only the assets you add yourself.
        </p>

        <fieldset className="grid grid-cols-2 gap-2" aria-label="Save action">
          <label className="flex items-center gap-2 rounded border border-border p-2 text-xs">
            <input
              type="radio"
              name="smart-folder-action"
              checked={mode === "create"}
              onChange={() => {
                setMode("create");
                setName("");
              }}
            />
            Create new
          </label>
          <label className="flex items-center gap-2 rounded border border-border p-2 text-xs">
            <input
              type="radio"
              name="smart-folder-action"
              checked={mode === "update"}
              disabled={smartFolders.length === 0}
              onChange={() => {
                setMode("update");
                const first = selected || smartFolders[0]?.id || "";
                setSelected(first);
                setName(smartFolders.find((folder) => folder.id === first)?.name ?? "");
              }}
            />
            Update existing
          </label>
        </fieldset>

        {mode === "update" && (
          <div>
            <label htmlFor="smart-folder-existing" className="mb-1 block text-[11px] text-fg-muted">
              Smart folder to replace
            </label>
            <select
              id="smart-folder-existing"
              className="field"
              value={selected}
              onChange={(event) => setSelected(event.target.value)}
            >
              <option value="" disabled>
                Choose a smart folder…
              </option>
              {smartFolders.map((folder) => (
                <option key={folder.id} value={folder.id}>
                  {folder.name}
                </option>
              ))}
            </select>
          </div>
        )}

        <div>
          <label htmlFor="smart-folder-name" className="mb-1 block text-[11px] text-fg-muted">
            Name
          </label>
          <input
            id="smart-folder-name"
            className="field"
            value={name}
            placeholder="e.g. Safe to ship"
            autoFocus
            onChange={(event) => setName(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") submit();
            }}
          />
        </div>

        <section className="rounded border border-border bg-surface-2 p-3" aria-label="Search preview">
          <div className="mb-1 text-[11px] font-semibold text-fg">
            {mode === "update" ? "Replace with this live search" : "This live search will be saved"}
          </div>
          <ul className="space-y-1 text-[11px] text-fg-muted">
            {summary.lines.map((line) => (
              <li key={line}>• {line}</li>
            ))}
          </ul>
          {summary.warnings.map((warning) => (
            <p key={warning} role="alert" className="mt-2 flex items-start gap-1.5 text-[11px] text-warn">
              <AlertTriangle size={13} className="mt-0.5 shrink-0" /> {warning}
            </p>
          ))}
        </section>

        {error && <p role="alert" className="text-[11px] text-danger">{error}</p>}

        <div className="flex justify-end gap-2">
          <button className="btn" onClick={onClose}>Cancel</button>
          <button className="btn btn-accent" disabled={pending} onClick={submit}>
            {pending ? "Saving…" : mode === "update" ? "Replace query" : "Create smart folder"}
          </button>
        </div>
      </div>
    </Modal>
  );
}
