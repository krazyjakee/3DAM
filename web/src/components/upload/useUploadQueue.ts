// The upload batch, extracted from `Upload.tsx` (issue #169, parent #96).
//
// This is the whole of the client-side batch: the rows the list renders, the ref-backed queue the
// workers consume, the cancellation set, and the concurrency-limited run loop. It is deliberately a
// hook rather than a component split, because none of it is UI — there is no server-side upload
// *job* to report (the transport is one request per file, tech-spec 08 §5.1), so "the batch" exists
// only on this side of the wire and is exactly the piece worth testing on its own. Behind the
// dropzone it could only be exercised by dragging files at a DOM.
//
// What stays in `Upload.tsx` is everything that *is* the container's DOM: the drop zone, the
// drag-depth counter (`dragenter`/`dragleave` are the shell's own events and nothing here could
// count them), the folder picker, and the source list.

import { useCallback, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "@/api/client";
import { qk } from "@/api/queries";
import type { SourceId, UploadCollision, UploadOutcome } from "@/api/types";
import { toast } from "@/lib/toast";

/** How many files travel at once.
 *
 *  One request per file is what makes per-file progress and fail-soft work (see the server's
 *  `upload` module), but letting a 200-file drop open 200 sockets would stall the whole browser
 *  connection pool and starve the thumbnails the user is looking at. Three keeps the pipe busy
 *  while leaving room for the rest of the app. */
export const CONCURRENCY = 3;

export type UploadItemState = "queued" | "uploading" | "done" | "skipped" | "error";

export interface UploadItem {
  id: string;
  file: File;
  state: UploadItemState;
  /** 0–1 while uploading. */
  progress: number;
  outcome?: UploadOutcome;
  error?: string;
}

/** Where a run writes. Passed at `run()` time rather than held by the hook: the destination is the
 *  container's form state, and a queue that mirrored it would have two copies to keep in step. */
export interface UploadDestination {
  source: SourceId;
  folder: string;
  collision: UploadCollision;
}

let seq = 0;

const nextId = () => `${(seq += 1)}`;

/** One line for the batch that just finished, raised as a toast.
 *
 *  There is deliberately no server-side upload *job* to report: the transport is one request per
 *  file, which is what makes per-file progress and fail-soft free rather than invented (tech-spec 08
 *  §5.1, and `dam-server`'s `upload` module). The batch therefore only exists on this side of the
 *  wire, so its summary is assembled here. The rows keep the per-file detail; this answers "did my
 *  drop land?" without the user reading twenty of them — and it survives navigating away from the
 *  list, since the toast viewport sits above every route. */
export function summarise(results: UploadItem[]): void {
  if (!results.length) return;
  const written = results.filter((r) => r.state === "done");
  const uncatalogued = written.filter((r) => r.outcome?.uncatalogued_reason).length;
  const skipped = results.filter((r) => r.state === "skipped").length;
  const failed = results.filter((r) => r.state === "error").length;

  const parts = [`${written.length} uploaded`];
  if (skipped) parts.push(`${skipped} skipped (name already taken)`);
  if (failed) parts.push(`${failed} failed`);
  // Stored-but-not-catalogued is part of the "uploaded" count, not an alternative to it, so it is
  // appended rather than listed alongside — otherwise the numbers would appear not to add up.
  const suffix = uncatalogued ? ` — ${uncatalogued} stored but not catalogued` : "";
  const message = `${parts.join(", ")}${suffix}`;

  // A batch with any failure is an error toast: those linger, and a success toast that auto-dismisses
  // in 3.5s is exactly the wrong lifetime for "one of your files didn't make it".
  if (failed) toast.error(message);
  else toast.success(message);
}

export interface UploadQueue {
  /** Every row the list shows, in the order it was added — including rows from earlier batches, and
   *  rows that were never queued at all (see `addRejected`). */
  items: UploadItem[];
  /** A run is in flight. Files may still be added; they join the same pass. */
  running: boolean;
  addFiles: (files: FileList | File[]) => void;
  /** Rows for things the user offered that will never be sent — a dropped folder, today. They are
   *  rendered as failures but are deliberately kept out of the work queue. */
  addRejected: (names: string[], error: string) => void;
  remove: (id: string) => void;
  /** Drop the finished rows, keeping anything still queued or in flight. */
  clear: () => void;
  run: (dest: UploadDestination) => Promise<void>;
}

export function useUploadQueue(): UploadQueue {
  const qc = useQueryClient();
  const [items, setItems] = useState<UploadItem[]>([]);
  const [running, setRunning] = useState(false);

  /** The work queue, deliberately *not* React state.
   *
   *  `items` is what the list renders; this is what the workers consume. Keeping them separate is
   *  what lets files dropped mid-run join the same pass — a run that snapshotted `items` at click
   *  time would strand them as rows that say "queued" forever after the batch reports itself
   *  finished — without any of the read-your-own-state-mid-async-loop guesswork. */
  const pending = useRef<UploadItem[]>([]);

  /** Ids the user removed, so a worker can skip a file that is still waiting.
   *
   *  Removing a row has to actually *cancel* it, not merely hide it: without this the worker would
   *  still reach the entry, write the bytes into the user's source, and audit the write — with no
   *  row left to report it, so nothing in the UI would ever say it landed. */
  const removed = useRef(new Set<string>());

  const addFiles = useCallback((files: FileList | File[]) => {
    const next = Array.from(files).map((file) => ({
      id: nextId(),
      file,
      state: "queued" as UploadItemState,
      progress: 0,
    }));
    if (!next.length) return;
    pending.current.push(...next);
    setItems((prev) => [...prev, ...next]);
  }, []);

  const addRejected = useCallback((names: string[], error: string) => {
    if (!names.length) return;
    // Ids are allocated here rather than inside the `setItems` updater: an updater must stay pure,
    // and React invokes it twice under StrictMode.
    const next = names.map((name) => ({
      id: nextId(),
      file: new File([], name),
      state: "error" as UploadItemState,
      progress: 0,
      error,
    }));
    setItems((prev) => [...prev, ...next]);
  }, []);

  const remove = useCallback((id: string) => {
    removed.current.add(id);
    // By id, not object identity: a progress tick replaces the object, so a handler closing over
    // the old one would match nothing and silently do nothing.
    setItems((prev) => prev.filter((i) => i.id !== id));
  }, []);

  const clear = useCallback(
    () => setItems((prev) => prev.filter((i) => i.state === "queued" || i.state === "uploading")),
    [],
  );

  const patchItem = useCallback(
    (id: string, patch: Partial<UploadItem>) =>
      setItems((prev) => prev.map((i) => (i.id === id ? { ...i, ...patch } : i))),
    [],
  );

  /** Upload everything queued, `CONCURRENCY` at a time.
   *
   *  Fail-soft per item by construction: each file is its own request, so one rejection settles one
   *  promise and the rest carry on. Nothing here aborts the batch. */
  const run = async ({ source, folder, collision }: UploadDestination) => {
    if (running) return;
    setRunning(true);

    // What the batch summary counts, accumulated by the workers themselves rather than read back
    // out of `items` when the run ends. `items` is the wrong source for two reasons: a `setItems`
    // closure captured at click time is stale by the first await, and the list deliberately keeps
    // rows from *earlier* batches until the user clears them — summarising it would re-report
    // yesterday's failures every time. Pushing is safe without a lock because JS is single-threaded
    // and every push happens synchronously after its own await resumes.
    const results: UploadItem[] = [];

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
          const patch: Partial<UploadItem> = {
            state: outcome.skipped ? "skipped" : "done",
            progress: 1,
            outcome,
          };
          patchItem(item.id, patch);
          results.push({ ...item, ...patch });
        } catch (e) {
          const patch: Partial<UploadItem> = {
            state: "error",
            error: e instanceof ApiError ? e.message : String(e),
          };
          patchItem(item.id, patch);
          results.push({ ...item, ...patch });
        }
      }
    };
    await Promise.all(Array.from({ length: CONCURRENCY }, worker));

    summarise(results);
    setRunning(false);
    // The server's `asset_added` events already invalidate these over the WebSocket, but an upload
    // that was stored-but-not-catalogued emits none — and the folder counts still moved.
    qc.invalidateQueries({ queryKey: qk.assets });
    qc.invalidateQueries({ queryKey: qk.stats });
    qc.invalidateQueries({ queryKey: ["folders"] });
  };

  return { items, running, addFiles, addRejected, remove, clear, run };
}
