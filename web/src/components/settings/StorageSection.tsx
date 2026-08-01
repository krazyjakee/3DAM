import { useState } from "react";
import { admin, type CacheTarget, type StorageUsage } from "@/api/admin";
import { useScan } from "@/api/queries";
import { useDialogs } from "@/lib/dialogs";
import { binaryBytes } from "@/lib/format";
import { errorMessage, toast } from "@/lib/toast";
import { AdminField } from "./AdminField";

/** One maintenance action: a labelled row with a hint and a single button. */
function ActionRow({
  title,
  hint,
  button,
  onClick,
  busy,
  danger,
}: {
  title: string;
  hint: string;
  button: string;
  onClick: () => void;
  busy: boolean;
  danger?: boolean;
}) {
  return (
    <div className="flex items-center justify-between gap-4 rounded border border-border p-3">
      <div className="min-w-0">
        <div className="font-medium">{title}</div>
        <p className="text-xs text-fg-dim">{hint}</p>
      </div>
      <button
        type="button"
        disabled={busy}
        onClick={onClick}
        className={`btn shrink-0 disabled:opacity-40 ${danger ? "text-danger" : ""}`}
      >
        {busy ? "Working…" : button}
      </button>
    </div>
  );
}

/** Storage overview + maintenance actions (tech-spec 10 §5). */
export function StorageSection({
  usage,
  error,
  onChange,
}: {
  usage: StorageUsage | null;
  error: string | null;
  onChange: () => void;
}) {
  const { confirm } = useDialogs();
  const scan = useScan();
  const [busy, setBusy] = useState<string | null>(null);

  const run = async (key: string, fn: () => Promise<string>) => {
    setBusy(key);
    try {
      toast.success(await fn());
      onChange();
    } catch (error) {
      toast.error(errorMessage(error));
    } finally {
      setBusy(null);
    }
  };

  const clearCache = (target: CacheTarget, label: string) =>
    run(`cache:${target}`, async () => {
      const result = await admin.clearCache(target);
      return `Cleared ${label}: ${result.files_deleted} files, ${binaryBytes(result.bytes_freed)} freed`;
    });

  const rescanAll = () => {
    scan.mutate({ mode: "full" });
    toast.success("Full rescan of all sources started");
  };

  const clearAnalysis = async () => {
    if (
      !(await confirm({
        title: "Clear analysis?",
        message:
          "Drops auto-tag/dedup suggestions and derived analysis, and marks every asset for re-analysis. Your confirmed tags are kept.",
        danger: true,
        confirmLabel: "Clear analysis",
      }))
    )
      return;
    void run("analysis", async () => {
      const result = await admin.clearAnalysis();
      return `Cleared ${result.suggestions_removed} suggestions and ${result.embeddings_removed} embeddings`;
    });
  };

  const vacuum = () =>
    run("vacuum", async () => {
      const result = await admin.vacuum();
      return `Database compacted — reclaimed ${binaryBytes(result.reclaimed_bytes)}`;
    });

  const resetCatalog = async () => {
    if (
      !(await confirm({
        title: "Reset the catalog?",
        message:
          "Removes every cataloged asset, source, collection, and tag from this library. Files on disk are NOT touched, and your tokens & settings are kept. This cannot be undone.",
        danger: true,
        confirmLabel: "Reset catalog",
      }))
    )
      return;
    void run("wipe", async () => {
      const result = await admin.wipe(true);
      return `Catalog reset — ${result.assets_removed} assets, ${result.sources_removed} sources removed`;
    });
  };

  const factoryReset = async () => {
    if (
      !(await confirm({
        title: "Factory reset everything?",
        message:
          "Erases the catalog AND all caches, API tokens, feature flags, and the audit log. The app returns to its first-run state and your current admin token stops working. This cannot be undone.",
        danger: true,
        confirmLabel: "Factory reset",
      }))
    )
      return;
    void run("factory", async () => {
      const result = await admin.factoryReset(true);
      return `Factory reset complete — ${result.catalog.assets_removed} assets and ${result.tokens_removed} tokens removed`;
    });
  };

  return (
    <section className="flex flex-col gap-3">
      <h2 className="font-medium text-fg-muted">Storage &amp; maintenance</h2>
      {error && (
        <div
          className="rounded border border-danger/40 bg-danger/10 px-3 py-2 text-danger"
          role="alert"
        >
          <span className="font-medium">Storage usage unavailable.</span> {error}
        </div>
      )}
      <section className="rounded border border-border p-3">
        {usage ? (
          <div className="grid grid-cols-2 gap-x-6 gap-y-1 sm:grid-cols-3">
            <AdminField label="Data directory" value={usage.data_dir} />
            <AdminField label="Catalog (library.db)" value={binaryBytes(usage.library_db_bytes)} />
            <AdminField label="Server config (server.db)" value={binaryBytes(usage.server_db_bytes)} />
            <AdminField
              label="Thumbnail cache"
              value={`${binaryBytes(usage.thumbnails.bytes)} · ${usage.thumbnails.files} files`}
            />
            <AdminField
              label="3D preview cache"
              value={`${binaryBytes(usage.previews.bytes)} · ${usage.previews.files} files`}
            />
            <AdminField label="Assets" value={String(usage.asset_count)} />
            <AdminField label="Sources" value={String(usage.source_count)} />
          </div>
        ) : !error ? (
          <p className="text-fg-dim">Loading storage usage…</p>
        ) : null}
      </section>

      <ActionRow title="Rescan all sources" hint="Full re-read of every registered source (the sidebar only runs quick, changed-file scans)." button="Rescan all (full)" busy={scan.isPending} onClick={rescanAll} />
      <ActionRow title="Clear thumbnail cache" hint="Delete cached image thumbnails. They regenerate on next view." button="Clear thumbnails" busy={busy === "cache:thumbnails"} onClick={() => void clearCache("thumbnails", "thumbnails")} />
      <ActionRow title="Clear 3D preview cache" hint="Delete cached 3D preview meshes. They regenerate on next view." button="Clear 3D previews" busy={busy === "cache:previews"} onClick={() => void clearCache("previews", "3D previews")} />
      <ActionRow title="Clear analysis" hint="Drop auto-tag/dedup suggestions and embeddings; keeps confirmed tags." button="Clear analysis" busy={busy === "analysis"} onClick={() => void clearAnalysis()} danger />
      <ActionRow title="Compact database" hint="Reclaim disk space freed by deletions (VACUUM)." button="Compact" busy={busy === "vacuum"} onClick={vacuum} />

      <div className="mt-2 flex flex-col gap-3 rounded border border-danger/40 bg-danger/5 p-3">
        <div className="text-xs font-medium tracking-wide text-danger uppercase">Danger zone</div>
        <ActionRow title="Reset catalog" hint="Wipe all cataloged assets/sources/collections. Files on disk are untouched; tokens & settings are kept." button="Reset catalog" busy={busy === "wipe"} onClick={() => void resetCatalog()} danger />
        <ActionRow title="Factory reset" hint="Erase everything: catalog, caches, tokens, feature flags, and audit log. Returns to first-run." button="Factory reset" busy={busy === "factory"} onClick={() => void factoryReset()} danger />
      </div>
    </section>
  );
}
