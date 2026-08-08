import { useRef, useState } from "react";
import { Link } from "react-router";
import {
  Ban,
  Check,
  FolderUp,
  HardDrive,
  Link2,
  Loader2,
  Trash2,
  Upload as UploadIcon,
} from "lucide-react";
import { useCan, useSources, useVersion } from "@/api/queries";
import type { SourceId, SourceInfo, UploadCollision } from "@/api/types";
import { AUTH_COPY } from "@/lib/auth";
import { FolderTree } from "./FolderTree";
import { UploadRow } from "./upload/UploadRow";
import { useUploadQueue } from "./upload/useUploadQueue";

/** Why this source cannot be an upload destination, or `null` if it can.
 *
 *  A disabled row with a reason beats a missing one: a user who registered a share and cannot find
 *  it in the picker has no way to learn that the mount is read-only. `writable` is probed by the
 *  server, so this only has to explain the answer, not compute it. */
function unwritableReason(s: SourceInfo): string | null {
  if (s.kind === "federated") return "a peer's library is read-only";
  if (s.writable) return null;
  if (s.writable_reason) return s.writable_reason;
  // SFTP and SMB *do* have a write side (issue #80 slice 7), so an unwritable one is a property of
  // this build rather than of the protocol: the backend was compiled out, or it is an SMB share on a
  // non-default port, which `SmbSource::connect` refuses outright. Neither is fixable by the user
  // changing permissions, which is why it doesn't share the local wording below.
  if (s.kind === "sftp" || s.kind === "smb")
    return "this server can't write here — backend not built in, or a non-default SMB port";
  return "not writable — check permissions on the folder";
}

export function Upload() {
  const sources = useSources();
  const version = useVersion();
  const canWrite = useCan("write");
  const [source, setSource] = useState<SourceId | null>(null);
  const [folder, setFolder] = useState("");
  const [collision, setCollision] = useState<UploadCollision>("fail");
  const { items, running, addFiles, addRejected, remove, clear, run } = useUploadQueue();
  const fileInput = useRef<HTMLInputElement>(null);

  // Drag state is a *counter*, not a boolean. `dragenter`/`dragleave` fire for every descendant the
  // pointer crosses, so a boolean flickers off the moment the cursor moves over a child of the drop
  // zone. Counting enters minus leaves is stable no matter how deeply nested the zone gets.
  const [dragDepth, setDragDepth] = useState(0);

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
      addRejected(
        dirNames,
        "Folders can't be dropped yet — open it and drop the files inside.",
      );
    }
    if (files.length) addFiles(files);
  };

  const all = sources.data ?? [];
  const writable = all.filter((s) => !unwritableReason(s));
  const blocked = all.filter((s) => unwritableReason(s));
  const queued = items.filter((i) => i.state === "queued");
  const dropping = dragDepth > 0;

  // The `upload` flag is off (issue #80): the route itself is absent server-side, so every request
  // this view could make would 404. The nav entry is already hidden — this covers the deep link,
  // and says *why* rather than letting the user discover it one failed drop at a time. Waits for
  // the version query so a slow first load doesn't flash "disabled" at an enabled server.
  if (version.isSuccess && version.data.upload !== true) {
    return (
      <Unavailable>
        Uploads are disabled on this server. Writing files into a source is off by default — an
        admin can turn it on in Administration, or with{" "}
        <code className="font-mono text-fg">3dam admin flag upload on</code>.
      </Unavailable>
    );
  }

  // The caller holds no write scope: the same deep-link hole one gate further in. The route already
  // 403s per file, but a working source picker and drop zone that only fail after the bytes have
  // been chosen is the "disables rather than 403s" rule (issue #80 §3) applied to everything except
  // the view itself. `useCan` is optimistic while `/whoami` is in flight, so this settles on a real
  // read-only token rather than flashing at every cold load.
  if (!canWrite) {
    return (
      <Unavailable>
        {AUTH_COPY.needsWrite} Uploading writes files into a source, so it needs write access even
        where browsing does not.
      </Unavailable>
    );
  }

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
                <UploadRow key={i.id} item={i} onRemove={() => remove(i.id)} />
              ))}
            </ul>
          )}

          <div className="flex items-center gap-2">
            <button
              className="btn btn-accent"
              onClick={() => {
                if (source) void run({ source, folder, collision });
              }}
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
              <button className="btn" onClick={clear}>
                <Trash2 size={13} /> Clear finished
              </button>
            )}
          </div>
        </section>
      )}
    </div>
  );
}

/** The "you can't do this here" shell — same header, one explanation.
 *
 *  Both refusals (the deployment doesn't do uploads; this caller may not write) look identical to
 *  the user and differ only in the sentence, so they share a frame rather than drifting into two
 *  near-identical panels. */
function Unavailable({ children }: { children: React.ReactNode }) {
  return (
    <div className="mx-auto flex min-h-dvh max-w-4xl flex-col gap-4 p-6 text-sm">
      <header className="flex items-center justify-between">
        <h1 className="flex items-center gap-2 text-lg font-semibold text-fg">
          <UploadIcon size={18} className="text-fg-dim" /> Upload assets
        </h1>
        <Link to="/" className="text-xs text-accent hover:underline">
          ← Back to library
        </Link>
      </header>
      <p className="rounded border border-border bg-panel p-3 text-xs text-fg-dim">{children}</p>
    </div>
  );
}
