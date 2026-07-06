import { useState } from "react";
import { FolderPlus, X } from "lucide-react";
import { useAddSource, useScan } from "@/api/queries";
import { ApiError } from "@/api/client";
import type { SourceKind } from "@/api/types";

// The dialog offers local_fs today and marks sftp/smb/federated as not-yet-wired here (the phase-4
// backend supports sftp/smb; surfacing their connection fields is issue #2) rather than hiding the
// roadmap.
const KINDS: { value: SourceKind; label: string; enabled: boolean }[] = [
  { value: "local_fs", label: "Local folder", enabled: true },
  { value: "sftp", label: "SFTP (soon)", enabled: false },
  { value: "smb", label: "SMB / Samba (soon)", enabled: false },
  { value: "federated", label: "Federated peer (soon)", enabled: false },
];

export function AddSourceDialog({ onClose }: { onClose: () => void }) {
  const add = useAddSource();
  const scan = useScan();
  const [kind, setKind] = useState<SourceKind>("local_fs");
  const [uri, setUri] = useState("");
  const [name, setName] = useState("");
  const [watch, setWatch] = useState(true);
  const [err, setErr] = useState<string | null>(null);

  const submit = async () => {
    setErr(null);
    if (!uri.trim()) return setErr("A path is required.");
    try {
      const { id } = await add.mutateAsync({
        kind,
        uri: uri.trim(),
        name: name.trim() || null,
        options: { watch },
      });
      // add_source does not auto-scan — kick a full scan of the new source (tech-spec 03).
      await scan.mutateAsync({ sources: [id], mode: "full" });
      onClose();
    } catch (e) {
      setErr(e instanceof ApiError ? e.message : String(e));
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      onClick={onClose}
    >
      <div
        className="w-full max-w-[380px] rounded-lg border border-border bg-surface p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center justify-between">
          <h2 className="flex items-center gap-2 text-sm font-semibold">
            <FolderPlus size={15} /> Add source
          </h2>
          <button
            className="flex items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
            onClick={onClose}
            aria-label="Close"
          >
            <X size={16} />
          </button>
        </div>

        <label className="mb-1 block text-[11px] text-fg-muted">Kind</label>
        <select
          className="field mb-3 coarse:min-h-11"
          value={kind}
          onChange={(e) => setKind(e.target.value as SourceKind)}
        >
          {KINDS.map((k) => (
            <option key={k.value} value={k.value} disabled={!k.enabled}>
              {k.label}
            </option>
          ))}
        </select>

        <label className="mb-1 block text-[11px] text-fg-muted">Path</label>
        <input
          className="field mb-3 coarse:min-h-11"
          placeholder="/mnt/assets/sfx"
          value={uri}
          onChange={(e) => setUri(e.target.value)}
          autoFocus
          onKeyDown={(e) => e.key === "Enter" && submit()}
        />

        <label className="mb-1 block text-[11px] text-fg-muted">Name (optional)</label>
        <input
          className="field mb-3 coarse:min-h-11"
          placeholder="SFX library"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />

        <label className="mb-3 flex items-center gap-2 text-xs text-fg-muted select-none coarse:min-h-11">
          <input
            type="checkbox"
            className="coarse:h-5 coarse:w-5"
            checked={watch}
            onChange={(e) => setWatch(e.target.checked)}
          />
          Watch for changes and re-scan deltas
        </label>

        {err && <p className="mb-3 text-xs text-danger">{err}</p>}

        <div className="flex justify-end gap-2">
          <button className="btn coarse:min-h-11" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn btn-accent coarse:min-h-11"
            onClick={submit}
            disabled={add.isPending}
          >
            {add.isPending ? "Adding…" : "Add & scan"}
          </button>
        </div>
      </div>
    </div>
  );
}
