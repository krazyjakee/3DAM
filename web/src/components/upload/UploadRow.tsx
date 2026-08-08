import {
  AlertTriangle,
  Ban,
  Check,
  Loader2,
  Upload as UploadIcon,
  X,
} from "lucide-react";
import { bytes } from "@/lib/format";
import type { UploadItem, UploadItemState } from "./useUploadQueue";

export function UploadRow({ item, onRemove }: { item: UploadItem; onRemove: () => void }) {
  const pct = Math.round(item.progress * 100);
  const name = item.outcome?.path ?? item.file.name;
  const nameId = `upload-name-${item.id}`;
  const stateId = `upload-state-${item.id}`;
  const terminalAnnouncement =
    item.state === "done"
      ? `${name} uploaded`
      : item.state === "skipped"
        ? `${name} skipped because a file of that name already exists`
        : item.state === "error"
          ? `${name} upload failed: ${item.error}`
          : "";

  return (
    <li className="flex items-center gap-2 rounded px-2 py-1 text-xs">
      <StateIcon state={item.state} />
      {/* The name keeps a floor so a long message can't starve it down to one letter — knowing
          *which* file failed matters at least as much as why. Both sides truncate; both carry the
          full text in a title. */}
      <span id={nameId} className="min-w-[7rem] flex-1 truncate text-fg" title={name}>
        {name}
      </span>

      {item.state === "uploading" && (
        <>
          <div
            className="h-1 w-24 shrink-0 overflow-hidden rounded bg-surface-2"
            role="progressbar"
            aria-labelledby={`${nameId} ${stateId}`}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={pct}
            aria-valuetext={`${pct}% uploaded`}
          >
            <div
              aria-hidden="true"
              className="h-full rounded"
              style={{
                width: `${pct}%`,
                background: "var(--color-accent)",
                transition: "width .2s",
              }}
            />
          </div>
          <span id={stateId} className="shrink-0 tabular-nums text-fg-dim">
            {pct}% uploaded
          </span>
        </>
      )}

      {item.state === "queued" && (
        <span className="shrink-0 tabular-nums text-fg-dim">{bytes(item.file.size)}</span>
      )}

      {item.state === "skipped" && (
        <span className="min-w-0 truncate text-fg-dim">
          skipped — a file of that name is already there
        </span>
      )}

      {/* Stored but not catalogued is a *success* the user still has to know about: the file is on
          disk, and quietly showing a green tick would turn "why isn't it in my library?" into a bug
          report. */}
      {item.state === "done" && item.outcome?.uncatalogued_reason && (
        <span
          className="flex min-w-0 items-center gap-1 text-warn"
          title={item.outcome.uncatalogued_reason}
        >
          <AlertTriangle size={12} className="shrink-0" />
          <span className="truncate">{item.outcome.uncatalogued_reason}</span>
        </span>
      )}

      {item.state === "error" && (
        <span className="min-w-0 truncate text-danger" title={item.error}>
          {item.error}
        </span>
      )}

      <span className="sr-only" role="status" aria-live="polite" aria-atomic="true">
        {terminalAnnouncement}
      </span>

      {(item.state === "queued" || item.state === "error") && (
        <button
          onClick={onRemove}
          className="shrink-0 text-fg-dim hover:text-danger coarse:min-h-11 coarse:min-w-11"
          title="Remove"
          aria-label={`Remove ${item.file.name}`}
        >
          <X size={12} />
        </button>
      )}
    </li>
  );
}

function StateIcon({ state }: { state: UploadItemState }) {
  if (state === "uploading")
    return <Loader2 size={13} className="shrink-0 animate-spin text-accent" />;
  if (state === "done") return <Check size={13} className="shrink-0 text-lic-permissive" />;
  if (state === "skipped") return <Ban size={13} className="shrink-0 text-fg-dim" />;
  if (state === "error") return <X size={13} className="shrink-0 text-danger" />;
  return <UploadIcon size={13} className="shrink-0 text-fg-dim" />;
}
