// The Inspector's note field (issue #166, parent #96) — extracted from `Inspector.tsx`. One panel,
// one mutation (`useSetNote`), one `asset` prop; `web/tests/components/note-autosave.test.tsx` is
// its regression guard.

import { useEffect, useRef, useState } from "react";
import { useCan, useSetNote } from "@/api/queries";
import type { Asset, AssetId } from "@/api/types";
import { AUTH_COPY } from "@/lib/auth";
import { relTime } from "@/lib/format";
import { peerReadOnlyTitle } from "@/lib/origin";

/** How long a pause in typing counts as "done" for autosave. Long enough not to fire mid-word,
 *  short enough that the save has landed before a user's hand reaches the mouse. Blur, switching
 *  asset, and unmount all flush immediately, so this delay is never the only thing standing between
 *  a keystroke and the database. */
const NOTE_AUTOSAVE_MS = 700;

/** The user's free-text note (issue #81) — the one field the automation will never infer.
 *
 *  Autosaves on a typing pause, on blur, and on the way out (asset switch or unmount). Losing a
 *  note to a navigation would be worse than having no note feature, so every exit path flushes.
 *
 *  Two failure modes are designed against explicitly, and both are why the pending edit is a ref
 *  tagged with the asset id rather than plain state:
 *
 *  1. **Cross-asset writes.** The queued body carries the id it was typed against, so the flush
 *     that fires *because* the selection moved still addresses the asset the user was looking at.
 *     Asset A's prose can never land on asset B.
 *  2. **Echo stomping.** Saving refreshes the cached asset, and a peer's edit arrives over the
 *     WebSocket the same way. Adopting server text unconditionally would overwrite keystrokes typed
 *     while the request was in flight, so an incoming value is only adopted when nothing is
 *     pending — or when the asset changed, where adopting is the whole point. */
export function NoteEditor({ asset }: { asset: Asset }) {
  const setNote = useSetNote();
  const canWrite = useCan("write");
  // Peer-owned assets are read-only references (tech-spec 07 §7.4) — annotate them on their peer.
  const peerTitle = peerReadOnlyTitle(asset.summary.origin);
  const readOnly = !canWrite || !!peerTitle;
  const id = asset.summary.id;
  const stored = asset.note?.body ?? "";

  const [draft, setDraft] = useState(stored);
  const [dirty, setDirty] = useState(false);
  /** The edit waiting to be written, tagged with the asset it belongs to. */
  const pending = useRef<{ id: AssetId; body: string } | null>(null);
  const seeded = useRef(id);

  const mutate = setNote.mutate;
  // A ref-held callback so the flush effects can stay keyed on the asset id: reading the latest
  // draft through `pending` instead of through the closure is what keeps a keystroke from
  // re-arming the "flush on exit" cleanup on every character.
  const commit = useRef(() => {});
  commit.current = () => {
    const queued = pending.current;
    if (!queued) return;
    pending.current = null;
    setDirty(false);
    mutate(queued);
  };

  useEffect(() => {
    const switched = seeded.current !== id;
    seeded.current = id;
    if (switched || pending.current === null) {
      setDraft(stored);
      setDirty(false);
    }
  }, [id, stored]);

  // Autosave after a pause in typing…
  useEffect(() => {
    if (!dirty) return;
    const t = setTimeout(() => commit.current(), NOTE_AUTOSAVE_MS);
    return () => clearTimeout(t);
  }, [draft, dirty]);

  // …and on the way out. The cleanup fires on unmount *and* whenever the inspected asset changes,
  // which is exactly the navigation that would otherwise drop an unsaved note on the floor.
  useEffect(() => () => commit.current(), [id]);

  const status = setNote.isPending
    ? "Saving…"
    : dirty
      ? "Unsaved"
      : asset.note
        ? `Saved ${relTime(asset.note.updated_at)}${
            asset.note.updated_by ? ` by ${asset.note.updated_by}` : ""
          }`
        : "";

  return (
    <>
      <textarea
        className="field min-h-16 resize-y leading-relaxed disabled:cursor-not-allowed disabled:opacity-60"
        rows={3}
        value={draft}
        disabled={readOnly}
        placeholder={
          readOnly ? "No note" : "Add a note — the why a filename can’t carry…"
        }
        title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : undefined)}
        aria-label="Asset note"
        onChange={(e) => {
          setDraft(e.target.value);
          setDirty(true);
          pending.current = { id, body: e.target.value };
        }}
        onBlur={() => commit.current()}
      />
      <div className="flex justify-end text-[10px] text-fg-dim" aria-live="polite">
        {status}
      </div>
    </>
  );
}
