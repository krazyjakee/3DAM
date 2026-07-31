import { useCallback, useRef, useState } from "react";
import { Link } from "react-router-dom";
import {
  AlertTriangle,
  Ban,
  Check,
  FolderUp,
  HardDrive,
  Link2,
  Loader2,
  Trash2,
  Upload as UploadIcon,
  X,
} from "lucide-react";
import { useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "@/api/client";
import { qk, useSources } from "@/api/queries";
import type { SourceId, SourceInfo, UploadCollision, UploadOutcome } from "@/api/types";
import { bytes } from "@/lib/format";
import { FolderTree } from "./FolderTree";

/** How many files travel at once.
 *
 *  One request per file is what makes per-file progress and fail-soft work (see the server's
 *  `upload` module), but letting a 200-file drop open 200 sockets would stall the whole browser
 *  connection pool and starve the thumbnails the user is looking at. Three keeps the pipe busy
 *  while leaving room for the rest of the app. */
const CONCURRENCY = 3;

type ItemState = "queued" | "uploading" | "done" | "skipped" | "error";

interface Item {
  id: string;
  file: File;
  state: ItemState;
  /** 0–1 while uploading. */
  progress: number;
  outcome?: UploadOutcome;
  error?: string;
}

let seq = 0;

/** Why this source cannot be an upload destination, or `null` if it can.
 *
 *  A disabled row with a reason beats a missing one: a user who registered a share and cannot find
 *  it in the picker has no way to learn that the mount is read-only. `writable` is probed by the
 *  server, so this only has to explain the answer, not compute it. */
function unwritableReason(s: SourceInfo): string | null {
  if (s.kind === "federated") return "a peer's library is read-only";
  if (s.writable) return null;
  if (s.kind === "sftp" || s.kind === "smb") return "remote sources cannot be written to yet";
  return "not writable — check permissions on the folder";
}

export function Upload() {
  const sources = useSources();
  const qc = useQueryClient();
  const [source, setSource] = useState<SourceId | null>(null);
  const [folder, setFolder] = useState("");
  const [collision, setCollision] = useState<UploadCollision>("fail");
  const [items, setItems] = useState<Item[]>([]);
  const [running, setRunning] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);

  // Drag state is a *counter*, not a boolean. `dragenter`/`dragleave` fire for every descendant the
  // pointer crosses, so a boolean flickers off the moment the cursor moves over a child of the drop
  // zone. Counting enters minus leaves is stable no matter how deeply nested the zone gets.
  const [dragDepth, setDragDepth] = useState(0);

  /** The work queue, deliberately *not* React state.
   *
   *  `items` is what the list renders; this is what the workers consume. Keeping them separate is
   *  what lets files dropped mid-run join the same pass — a run that snapshotted `items` at click
   *  time would strand them as rows that say "queued" forever after the batch reports itself
   *  finished — without any of the read-your-own-state-mid-async-loop guesswork. */
  const pending = useRef<Item[]>([]);

  const addFiles = useCallback((files: FileList | File[]) => {
    const next = Array.from(files).map((file) => ({
      id: `${(seq += 1)}`,
      file,
      state: "queued" as ItemState,
      progress: 0,
    }));
    if (!next.length) return;
    pending.current.push(...next);
    setItems((prev) => [...prev, ...next]);
  }, []);

  const onDrop = (e: React.DragEvent) => {
    e.preventDefault();
    setDragDepth(0);

    // Directories have to be identified through `webkitGetAsEntry`, not by an empty `files` list:
    // Chrome and Firefox *do* put a dropped folder in `dataTransfer.files`, as a zero-byte entry
    // with no type. Testing `files.length === 0` therefore misses it on the two browsers that
    // matter, and the folder sails through as an ordinary upload — writing a real 0-byte file named
    // after the directory into the user's source. Recursing into one is deliberately out of scope
    // (issue #80 §7), so name the limitation instead.
    const entries = Array.from(e.dataTransfer.items ?? []);
    const dirNames = entries
      .map((it) => it.webkitGetAsEntry?.())
      .filter((entry): entry is FileSystemEntry => !!entry && entry.isDirectory)
      .map((entry) => entry.name);

    const files = Array.from(e.dataTransfer.files).filter((f) => !dirNames.includes(f.name));

    if (dirNames.length) {
      setItems((prev) => [
        ...prev,
        ...dirNames.map((name) => ({
          id: `${(seq += 1)}`,
          file: new File([], name),
          state: "error" as ItemState,
          progress: 0,
          error: "Folders can't be dropped yet — open it and drop the files inside.",
        })),
      ]);
    }
    if (files.length) addFiles(files);
  };

  const clearFinished = () =>
    setItems((prev) => prev.filter((i) => i.state === "queued" || i.state === "uploading"));

  const patchItem = (id: string, patch: Partial<Item>) =>
    setItems((prev) => prev.map((i) => (i.id === id ? { ...i, ...patch } : i)));

  /** Ids the user removed, so a worker can skip a file that is still waiting.
   *
   *  Removing a row has to actually *cancel* it, not merely hide it: without this the worker would
   *  still reach the entry, write the bytes into the user's source, and audit the write — with no
   *  row left to report it, so nothing in the UI would ever say it landed. */
  const removed = useRef(new Set<string>());

  const removeItem = (id: string) => {
    removed.current.add(id);
    // By id, not object identity: `patchItem` replaces the object on every progress tick, so a
    // handler closing over the old one would match nothing and silently do nothing.
    setItems((prev) => prev.filter((i) => i.id !== id));
  };

  /** Upload everything queued, `CONCURRENCY` at a time.
   *
   *  Fail-soft per item by construction: each file is its own request, so one rejection settles one
   *  promise and the rest carry on. Nothing here aborts the batch. */
  const start = async () => {
    if (!source || running) return;
    setRunning(true);

    const worker = async () => {
      for (;;) {
        // `shift()` is the whole synchronisation story: JS is single-threaded and there is no
        // `await` inside it, so two workers can never take the same file, and anything appended
        // while the run is in flight is picked up by whichever worker frees up next.
        const item = pending.current.shift();
        if (!item) return;
        if (removed.current.has(item.id)) continue;
        patchItem(item.id, { state: "uploading", progress: 0 });
        try {
          let lastPct = -1;
          const outcome = await api.upload(
            { source, folder, name: item.file.name, collision },
            item.file,
            (fraction) => {
              // Only on a whole-percent change. `patchItem` rebuilds the array and re-renders every
              // row, and XHR fires progress every few tens of milliseconds per file — on a 200-file
              // drop the progress bars would themselves be what makes the page stutter.
              const pct = Math.floor(fraction * 100);
              if (pct !== lastPct) {
                lastPct = pct;
                patchItem(item.id, { progress: fraction });
              }
            },
          );
          patchItem(item.id, {
            state: outcome.skipped ? "skipped" : "done",
            progress: 1,
            outcome,
          });
        } catch (e) {
          patchItem(item.id, {
            state: "error",
            error: e instanceof ApiError ? e.message : String(e),
          });
        }
      }
    };
    await Promise.all(Array.from({ length: CONCURRENCY }, worker));

    setRunning(false);
    // The server's `asset_added` events already invalidate these over the WebSocket, but an upload
    // that was stored-but-not-catalogued emits none — and the folder counts still moved.
    qc.invalidateQueries({ queryKey: qk.assets });
    qc.invalidateQueries({ queryKey: qk.stats });
    qc.invalidateQueries({ queryKey: ["folders"] });
  };

  const all = sources.data ?? [];
  const writable = all.filter((s) => !unwritableReason(s));
  const blocked = all.filter((s) => unwritableReason(s));
  const queued = items.filter((i) => i.state === "queued");
  const dropping = dragDepth > 0;

  return (
    <div className="mx-auto flex min-h-dvh max-w-4xl flex-col gap-5 p-6 text-sm">
      <header className="flex items-center justify-between">
        <h1 className="flex items-center gap-2 text-lg font-semibold text-fg">
          <UploadIcon size={18} className="text-accent" /> Upload assets
        </h1>
        <Link to="/" className="text-xs text-accent hover:underline">
          ← Back to library
        </Link>
      </header>

      <p className="text-xs text-fg-dim">
        Files are written into the source you choose and catalogued in place. Uploading never
        replaces an existing file — a name that is already taken fails, is suffixed, or is skipped,
        whichever you pick below.
      </p>

      {/* ── 1. destination source ─────────────────────────────────────────── */}
      <section className="flex flex-col gap-2">
        <h2 className="text-[11px] font-semibold tracking-wider text-fg-dim uppercase">
          1 · Source
        </h2>
        {sources.isLoading && <p className="text-xs text-fg-dim">Loading sources…</p>}
        {!sources.isLoading && writable.length === 0 && (
          <p className="rounded border border-warn/40 bg-warn/10 p-3 text-xs text-fg">
            No source can accept uploads. Register a local folder you have write access to, or check
            permissions on the ones below.
          </p>
        )}
        <div className="flex flex-col gap-1">
          {writable.map((s) => (
            <button
              key={s.id}
              onClick={() => {
                setSource(s.id);
                setFolder("");
              }}
              className="flex items-center gap-2 rounded border px-3 py-2 text-left text-xs transition-colors coarse:min-h-11"
              style={{
                borderColor:
                  source === s.id ? "var(--color-accent)" : "var(--color-border)",
                background:
                  source === s.id ? "var(--color-accent-muted)" : "var(--color-surface)",
                color: source === s.id ? "var(--color-accent)" : "var(--color-fg-muted)",
              }}
            >
              <HardDrive size={14} className="shrink-0" />
              <span className="font-medium">{s.name}</span>
              <span className="truncate text-fg-dim" title={s.uri}>
                {s.uri}
              </span>
              {source === s.id && <Check size={14} className="ml-auto shrink-0" />}
            </button>
          ))}
          {blocked.map((s) => (
            <div
              key={s.id}
              className="flex items-center gap-2 rounded border border-border px-3 py-2 text-xs opacity-60"
              title={s.uri}
            >
              {s.kind === "federated" ? (
                <Link2 size={14} className="shrink-0 text-fg-dim" />
              ) : (
                <Ban size={14} className="shrink-0 text-fg-dim" />
              )}
              <span className="font-medium text-fg-dim">{s.name}</span>
              <span className="ml-auto shrink-0 text-[10px] text-fg-dim italic">
                {unwritableReason(s)}
              </span>
            </div>
          ))}
        </div>
      </section>

      {/* ── 2. destination folder ─────────────────────────────────────────── */}
      {source && (
        <section className="flex flex-col gap-2">
          <h2 className="text-[11px] font-semibold tracking-wider text-fg-dim uppercase">
            2 · Destination folder
          </h2>
          <div className="max-h-64 overflow-y-auto rounded border border-border bg-surface py-1">
            <button
              onClick={() => setFolder("")}
              className="flex w-full items-center gap-1.5 px-3 py-1 text-left text-xs coarse:min-h-11"
              style={{
                background: folder === "" ? "var(--color-accent-muted)" : "transparent",
                color: folder === "" ? "var(--color-accent)" : "var(--color-fg-muted)",
              }}
            >
              <FolderUp size={13} className="shrink-0" /> Source root
            </button>
            <FolderTree
              source={source}
              prefix=""
              depth={0}
              select={{ current: folder, onSelect: setFolder }}
            />
          </div>
          <p className="text-[11px] text-fg-dim">
            Writing to <span className="text-fg">{folder || "the source root"}</span>. Missing
            folders are created.
          </p>
        </section>
      )}

      {/* ── 3. files ──────────────────────────────────────────────────────── */}
      {source && (
        <section className="flex flex-col gap-2">
          <h2 className="text-[11px] font-semibold tracking-wider text-fg-dim uppercase">
            3 · Files
          </h2>

          <div
            onDragEnter={(e) => {
              e.preventDefault();
              setDragDepth((d) => d + 1);
            }}
            onDragLeave={() => setDragDepth((d) => Math.max(0, d - 1))}
            onDragOver={(e) => e.preventDefault()}
            onDrop={onDrop}
            className="flex flex-col items-center justify-center gap-2 rounded border-2 border-dashed p-8 transition-colors"
            style={{
              borderColor: dropping ? "var(--color-accent)" : "var(--color-border)",
              background: dropping ? "var(--color-accent-muted)" : "transparent",
            }}
          >
            <UploadIcon size={22} className={dropping ? "text-accent" : "text-fg-dim"} />
            <p className="text-xs text-fg-muted">Drop files here</p>
            <button
              className="btn"
              onClick={() => fileInput.current?.click()}
              disabled={running}
            >
              Choose files…
            </button>
            <input
              ref={fileInput}
              type="file"
              multiple
              className="hidden"
              onChange={(e) => {
                if (e.target.files) addFiles(e.target.files);
                // Reset, or picking the same file twice in a row fires no change event.
                e.target.value = "";
              }}
            />
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <label className="text-[11px] text-fg-muted">If the name is taken</label>
            <select
              className="field w-auto"
              value={collision}
              onChange={(e) => setCollision(e.target.value as UploadCollision)}
              disabled={running}
            >
              <option value="fail">Fail — don't upload it</option>
              <option value="suffix">Suffix — keep both (name-1.ext)</option>
              <option value="skip">Skip — leave the existing file</option>
            </select>
            <span className="text-[10px] text-fg-dim">
              Existing files are never overwritten.
            </span>
          </div>

          {items.length > 0 && (
            <ul className="flex flex-col gap-1 rounded border border-border bg-surface p-1">
              {items.map((i) => (
                <Row key={i.id} item={i} onRemove={() => removeItem(i.id)} />
              ))}
            </ul>
          )}

          <div className="flex items-center gap-2">
            <button
              className="btn btn-accent"
              onClick={start}
              disabled={running || queued.length === 0}
            >
              {running ? (
                <>
                  <Loader2 size={13} className="animate-spin" /> Uploading…
                </>
              ) : queued.length ? (
                `Upload ${queued.length} file${queued.length === 1 ? "" : "s"}`
              ) : (
                "Upload files"
              )}
            </button>
            {items.length > 0 && !running && (
              <button className="btn" onClick={clearFinished}>
                <Trash2 size={13} /> Clear finished
              </button>
            )}
          </div>
        </section>
      )}
    </div>
  );
}

function Row({ item, onRemove }: { item: Item; onRemove: () => void }) {
  const pct = Math.round(item.progress * 100);
  return (
    <li className="flex items-center gap-2 rounded px-2 py-1 text-xs">
      <StateIcon state={item.state} />
      {/* The name keeps a floor so a long message can't starve it down to one letter — knowing
          *which* file failed matters at least as much as why. Both sides truncate; both carry the
          full text in a title. */}
      <span
        className="min-w-[7rem] flex-1 truncate text-fg"
        title={item.outcome?.path ?? item.file.name}
      >
        {item.outcome?.path ?? item.file.name}
      </span>

      {item.state === "uploading" && (
        <>
          <div className="h-1 w-24 shrink-0 overflow-hidden rounded bg-surface-2">
            <div
              className="h-full rounded"
              style={{
                width: `${pct}%`,
                background: "var(--color-accent)",
                transition: "width .2s",
              }}
            />
          </div>
          <span className="shrink-0 tabular-nums text-fg-dim">{pct}%</span>
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

function StateIcon({ state }: { state: ItemState }) {
  if (state === "uploading")
    return <Loader2 size={13} className="shrink-0 animate-spin text-accent" />;
  if (state === "done") return <Check size={13} className="shrink-0 text-lic-permissive" />;
  if (state === "skipped") return <Ban size={13} className="shrink-0 text-fg-dim" />;
  if (state === "error") return <X size={13} className="shrink-0 text-danger" />;
  return <UploadIcon size={13} className="shrink-0 text-fg-dim" />;
}
