import type React from "react";
import { useEffect, useRef, useState } from "react";
import {
  Check,
  ClipboardCopy,
  PanelRightClose,
  PanelRightOpen,
  Pencil,
  RefreshCw,
  RotateCcw,
  Sparkles,
  Star,
  X,
} from "lucide-react";
import {
  useAnalyze,
  useAsset,
  useCan,
  useCollectionMembers,
  useCollections,
  useDuplicateGroup,
  useEditTags,
  useRegenerateThumbnail,
  useReviewSuggestion,
  useSetFavorite,
  useSetLicense,
  useSetNote,
  useSimilar,
  useSources,
  useTagVocabulary,
} from "@/api/queries";
import { ApiError } from "@/api/client";
import { AUTH_COPY } from "@/lib/auth";
import type {
  Asset,
  AssetId,
  AssetSummary,
  CollectionId,
  LicenseEditResult,
  LicenseInput,
  MediaAttributes,
  Origin,
  ReviewAction,
  SimilarHit,
  TagRef,
} from "@/api/types";
import { bytes, duration, mediaLabel, originLabel, relTime } from "@/lib/format";
import { copyText } from "@/lib/clipboard";
import {
  LICENSE_PRESETS,
  RIGHTS_FIELDS,
  textPatch,
  TRI_OPTIONS,
  triFromBool,
  triToBool,
  type TriState,
} from "@/lib/license";
import { isLocal, peerReadOnlyTitle } from "@/lib/origin";
import { useViewState } from "@/lib/view-state";
import { shortcutLabel, SHORTCUT_EVENT, type ShortcutId } from "@/lib/shortcuts";
import { LicenseBadge } from "./LicenseBadge";
import { Thumbnail } from "./Thumbnail";
// The preview cluster is its own module (issue #167): one entry point over a media lifecycle
// (blob vs stream ticket, plus renewal) shared by the 3D island, audio, image, video and document
// previews. The WASM island stays behind its dynamic import, so this file pulls no wgpu.
import { Preview } from "./inspector/preview";
import { MediaIcon } from "./MediaIcon";
import { PeerBadge } from "./PeerBadge";
import { Discussion } from "./Discussion";
import { Drawer } from "./Drawer";

/** Inspector — a persistent right rail on `lg`, an overlay drawer below it (responsive + touch pass). Both
 *  render the same {@link InspectorPanel}, so a 360px phone and a wide desktop show identical detail.
 *
 *  Below `lg` the drawer is opt-in (issue #33): selecting an asset highlights it in place — it no longer
 *  hijacks the screen — and the drawer opens only when the caller flips `open` (via the explicit
 *  "Inspect" affordance in the Workspace selection bar). Dismissing the drawer keeps the selection.
 *
 *  On `lg` the rail's close button collapses the whole rail (issue #65) — reclaiming the space for the
 *  Browser — rather than just emptying it; a slim strip with a reopen button remains. */
export function Inspector({
  width,
  open,
  onClose,
  collapsed,
  onCollapse,
  onExpand,
}: {
  width?: number;
  open: boolean;
  onClose: () => void;
  collapsed: boolean;
  onCollapse: () => void;
  onExpand: () => void;
}) {
  const { state } = useViewState();
  const selectedAsset = useAsset(state.selected, state.owner);
  const setFavorite = useSetFavorite();
  const canWrite = useCan("write");

  useEffect(() => {
    const onShortcut = (event: Event) => {
      if ((event as CustomEvent<ShortcutId>).detail !== "toggle-favourite") return;
      const summary = selectedAsset.data?.summary;
      if (
        summary &&
        canWrite &&
        !peerReadOnlyTitle(summary.origin) &&
        !setFavorite.isPending
      )
        setFavorite.mutate({ asset: summary.id, favorite: !summary.favorite });
    };
    window.addEventListener(SHORTCUT_EVENT, onShortcut);
    return () => window.removeEventListener(SHORTCUT_EVENT, onShortcut);
  }, [canWrite, selectedAsset.data?.summary, setFavorite]);

  return (
    <>
      {collapsed ? (
        // Collapsed rail (issue #65): a slim edge strip whose only control reopens the inspector.
        <aside
          id="workspace-inspector-rail"
          className="hidden w-9 shrink-0 flex-col items-center border-l border-border bg-surface py-2 lg:flex"
        >
          <button
            onClick={onExpand}
            title="Show inspector"
            aria-label="Show inspector"
            className="flex items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
          >
            <PanelRightOpen size={15} />
          </button>
        </aside>
      ) : (
        <aside
          id="workspace-inspector-rail"
          className="hidden shrink-0 flex-col border-l border-border bg-surface lg:flex"
          style={{ width }}
        >
          {/* The rail's close collapses the panel (reclaims space); reopen via the strip above. */}
          <InspectorPanel selected={state.selected} onClose={onCollapse} mode="rail" />
        </aside>
      )}
      {/* The drawer's close only dismisses the overlay — the selection (and its grid highlight) stays. */}
      <Drawer open={open && !!state.selected} onClose={onClose} side="right" label="Inspector">
        <InspectorPanel selected={state.selected} onClose={onClose} mode="drawer" />
      </Drawer>
    </>
  );
}

function InspectorPanel({
  selected,
  onClose,
  mode,
}: {
  selected: string | null;
  onClose: () => void;
  /** `rail` → the close control collapses the `lg` rail; `drawer` → it dismisses the overlay. */
  mode: "rail" | "drawer";
}) {
  const { state, patch } = useViewState();
  const asset = useAsset(selected, state.owner);
  const collapse = mode === "rail";

  // If the selected asset no longer exists — its source was removed, or it was removed + blocked
  // (#21) — clear the dangling selection instead of leaving the inspector stuck on an error (#29).
  // This is independent of the close button, which on the rail collapses rather than deselects (#65).
  useEffect(() => {
    if (asset.isError && asset.error instanceof ApiError && asset.error.status === 404) {
      // Repair the current entry in place. Pushing here would let Back return to the same dangling
      // asset and trigger this cleanup again, creating a navigation loop.
      patch({ selected: null }, { replace: true });
    }
  }, [asset.isError, asset.error, patch]);

  // The header — and its collapse/close control — is always present, so the rail can be collapsed
  // even with nothing selected (issue #65). The body below swaps placeholder / skeleton / content.
  return (
    <div
      className="flex h-full flex-col overflow-y-auto"
      data-shortcut-region="inspector"
      tabIndex={-1}
    >
      <div className="flex items-center justify-between border-b border-border px-3 py-2">
        <span className="text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
          Inspector
        </span>
        <button
          className="flex items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
          title={collapse ? "Collapse" : "Close"}
          aria-label={collapse ? "Collapse inspector" : "Close inspector"}
          onClick={onClose}
        >
          {/* The rail collapses (panel-close glyph, issue #65); the drawer dismisses (the X the
              modal/dialog uses, issue #33 item 4). */}
          {collapse ? <PanelRightClose size={15} /> : <X size={15} />}
        </button>
      </div>

      {!selected ? (
        <div className="flex flex-1 items-center justify-center px-6 text-center text-xs text-fg-dim">
          Select an asset to inspect it.
        </div>
      ) : asset.isLoading ? (
        <InspectorSkeleton />
      ) : asset.isError || !asset.data ? (
        <div className="p-4 text-xs text-danger">Could not load this asset.</div>
      ) : (
        <Body asset={asset.data} />
      )}
    </div>
  );
}

/** Loading placeholder mirroring the inspector body (issue #30): a square preview block, a title,
 *  and a few metadata rows as pulsing skeletons — so the panel doesn't pop from "Loading…" to full. */
function InspectorSkeleton() {
  return (
    <div className="flex flex-col">
      <div className="aspect-square animate-pulse border-b border-border bg-surface-2" />
      <div className="flex flex-col gap-3 p-3">
        <div className="h-4 w-2/3 animate-pulse rounded bg-surface-2" />
        <div className="h-6 w-24 animate-pulse rounded bg-surface-2" />
        <div className="mt-1 flex flex-col gap-2">
          {Array.from({ length: 5 }).map((_, i) => (
            <div key={i} className="flex items-center justify-between gap-3">
              <div className="h-2.5 w-16 animate-pulse rounded bg-surface-2" />
              <div className="h-2.5 w-24 animate-pulse rounded bg-surface-2" />
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

function Body({ asset }: { asset: Asset }) {
  const { summary } = asset;
  const sources = useSources();
  const sourceName =
    sources.data?.find((s) => s.id === asset.source_id)?.name ?? asset.source_id;

  return (
    <div className="flex flex-col">
      {/* content-truthful preview — interactive WASM islands for 3D + audio (tech-spec 09 §B.3),
          a server-rendered thumbnail otherwise. Keyed by id so switching assets remounts cleanly. */}
      <Preview key={summary.id} asset={asset} />

      <div className="p-3">
        {/* title */}
        <div className="flex items-start gap-2">
          <MediaIcon media={summary.media} size={16} />
          <h1 className="min-w-0 flex-1 text-sm font-semibold break-words text-fg">
            {summary.name}
          </h1>
          {/* federated-origin chip (issue #39) — renders nothing for local assets */}
          <PeerBadge origin={summary.origin} className="mt-0.5" />
          <FavoriteButton asset={asset} />
        </div>

        {/* per-asset actions — reanalyze + rebuild the preview thumbnail (mirrors the context menu) */}
        <Actions asset={asset} />

        {/* license — HIGH in the inspector (DESIGN_GUIDELINES §3.1), and editable (issue #106) */}
        <LicenseSection key={summary.id} asset={asset} />

        {/* core metadata */}
        <Group title="Details">
          <Field label="Type" value={mediaLabel[summary.media]} />
          <Field label="Format" value={summary.format.toUpperCase()} />
          <Field label="Size" value={bytes(summary.size)} />
          {asset.attributes?.media === "model" &&
            (asset.attributes.dependency_bytes ?? 0) > 0 && (
              <Field
                label="↳ mesh + textures"
                value={`${bytes(
                  Math.max(0, summary.size - (asset.attributes.dependency_bytes ?? 0)),
                )} + ${bytes(asset.attributes.dependency_bytes ?? 0)}`}
              />
            )}
          <Field label="Origin" value={originLabel(summary.origin)} />
          <Field label="Source" value={sourceName} />
          <Field label="Path" value={asset.path} mono copyable />
          {asset.hash && <Field label="Hash" value={asset.hash.slice(0, 16) + "…"} mono />}
        </Group>

        <MediaFacts attrs={asset.attributes} />

        <Group title="Timestamps">
          <Field label="Scanned" value={relTime(asset.timestamps.scanned)} />
          <Field label="Modified" value={relTime(asset.timestamps.modified)} />
          <Field
            label="Analyzed"
            value={asset.timestamps.analyzed ? relTime(asset.timestamps.analyzed) : "not analyzed"}
          />
        </Group>

        {/* the user's own words — the one field no extractor can infer (issue #81) */}
        <Group title="Note">
          <NoteEditor asset={asset} />
        </Group>

        {/* tags — auto-suggestions are actionable (accept/reject); the analysis pass shipped in phase 3 */}
        <Group title={`Tags (${asset.tags.length})`}>
          <TagList assetId={summary.id} tags={asset.tags} origin={summary.origin} />
        </Group>

        {/* collections — this asset's manual memberships, with per-asset add/remove (issue #3) */}
        <CollectionsGroup asset={asset} />

        {/* duplicates — the byte-identical copies collapsed behind one card in the grid/table live
            here (rendered only when this asset is part of an exact-duplicate group) */}
        <DuplicatesSection asset={asset} />

        {/* find similar — cosine over embeddings, ranked in this asset's media space (phase 3) */}
        <SimilarSection asset={asset} />

        {/* discussion (issue #82) — deliberately *below* everything derived, and visually a message
            list rather than a field, so it never reads as a second note box. Renders nothing at all
            unless user accounts are on. */}
        <Discussion asset={asset} />
      </div>
    </div>
  );
}

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
function NoteEditor({ asset }: { asset: Asset }) {
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

/** Favourite toggle (issue #63) — a star in the inspector title row. Active state uses the one accent
 *  (warn/danger are reserved for exposure risk, DESIGN_GUIDELINES §4), and mirrors the nav Favorites
 *  facet: starring here makes the asset appear under that filter. */
function FavoriteButton({ asset }: { asset: Asset }) {
  const setFavorite = useSetFavorite();
  const canWrite = useCan("write");
  const peerTitle = peerReadOnlyTitle(asset.summary.origin);
  const on = asset.summary.favorite;
  const label = on ? "Remove from favourites" : "Add to favourites";
  return (
    <button
      type="button"
      className="flex shrink-0 items-center justify-center transition-colors disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
      style={{ color: on ? "var(--color-accent)" : "var(--color-fg-dim)" }}
      title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : `${label} (${shortcutLabel("toggle-favourite")})`)}
      aria-label={label}
      aria-pressed={on}
      aria-keyshortcuts="F"
      disabled={setFavorite.isPending || !canWrite || !!peerTitle}
      onClick={() => setFavorite.mutate({ asset: asset.summary.id, favorite: !on })}
    >
      <Star size={16} className={on ? "fill-current" : ""} />
    </button>
  );
}

/** Per-asset maintenance actions, mirrored from the grid context menu so they're reachable from the
 *  focused-asset view too. "Reanalyze" forces a re-run (never a silent no-op on an up-to-date asset);
 *  its label reflects whether analysis has ever run. "Regenerate thumbnail" drops the cached preview
 *  so it re-renders from the current source — only shown for media that has a server thumbnail. */
function Actions({ asset }: { asset: Asset }) {
  const analyze = useAnalyze();
  const regenerateThumbnail = useRegenerateThumbnail();
  const canWrite = useCan("write");
  const { summary } = asset;
  // Peer-owned assets are read-only references (tech-spec 07 §7.4) — maintenance runs on their peer.
  const peerTitle = peerReadOnlyTitle(summary.origin);
  const analyzed = asset.timestamps.analyzed != null;
  // Video joins image + 3D as a type the server can re-render a thumbnail for (its poster frame).
  const thumbable =
    summary.media === "image" || summary.media === "model" || summary.media === "video";

  return (
    <div className="mt-3 flex flex-wrap gap-2">
      <button
        type="button"
        className="btn disabled:cursor-not-allowed disabled:opacity-40"
        disabled={analyze.isPending || !canWrite || !!peerTitle}
        onClick={() => analyze.mutate({ assets: [summary.id], force: true })}
        title={
          peerTitle ??
          (!canWrite
            ? AUTH_COPY.needsWrite
            : "Re-run analysis (embeddings, tileability, auto-tags) for this asset")
        }
      >
        <Sparkles size={13} />
        {analyzed ? "Reanalyze" : "Analyze"}
      </button>
      {thumbable && (
        <button
          type="button"
          className="btn disabled:cursor-not-allowed disabled:opacity-40"
          disabled={regenerateThumbnail.isPending || !canWrite || !!peerTitle}
          onClick={() => regenerateThumbnail.mutate([summary.id])}
          title={
            peerTitle ??
            (!canWrite
              ? AUTH_COPY.needsWrite
              : "Rebuild the preview thumbnail from the current source file")
          }
        >
          <RefreshCw size={13} />
          Regenerate thumbnail
        </button>
      )}
    </div>
  );
}

/** This asset's collection memberships (issue #3). Manual memberships are editable per-asset here —
 *  remove with the chip's ×, add via the picker — which needs no multi-select (batch add/remove to a
 *  selection lands with the multi-select enabler). Smart folders are query-driven, so they show as a
 *  read-only chip with no remove control. */
function CollectionsGroup({ asset }: { asset: Asset }) {
  const collections = useCollections();
  const members = useCollectionMembers();
  const canWrite = useCan("write");
  // Membership rows live in this instance's catalog — a peer-owned reference can't join them.
  const peerTitle = peerReadOnlyTitle(asset.summary.origin);
  const all = collections.data ?? [];
  const inIds = new Set(asset.collections);
  const inCollections = all.filter((c) => inIds.has(c.id));
  const addable = all.filter((c) => c.kind === "manual" && !inIds.has(c.id));

  const edit = (id: CollectionId, op: "add" | "remove") =>
    members.mutate({ id, members: { [op]: [asset.summary.id] } });

  return (
    <Group title="Collections">
      {inCollections.length === 0 ? (
        <p className="text-[11px] text-fg-dim italic">Not in any collection.</p>
      ) : (
        <div className="flex flex-wrap gap-1">
          {inCollections.map((c) => (
            <span
              key={c.id}
              className="inline-flex items-center gap-1 rounded bg-surface-2 px-1.5 py-0.5 text-[10px] text-fg-muted"
              title={c.kind === "smart" ? "Smart folder (query-driven membership)" : c.name}
            >
              {c.name}
              {c.kind === "manual" && (
                <button
                  className="flex items-center justify-center hover:text-danger disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
                  title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : `Remove from ${c.name}`)}
                  aria-label={`Remove from ${c.name}`}
                  disabled={members.isPending || !canWrite || !!peerTitle}
                  onClick={() => edit(c.id, "remove")}
                >
                  <X size={11} />
                </button>
              )}
            </span>
          ))}
        </div>
      )}
      {addable.length > 0 && (
        <select
          className="field mt-2 disabled:cursor-not-allowed disabled:opacity-40"
          aria-label="Add to collection"
          title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : undefined)}
          value=""
          disabled={members.isPending || !canWrite || !!peerTitle}
          onChange={(e) => {
            if (e.target.value) edit(e.target.value, "add");
          }}
        >
          <option value="" disabled>
            Add to collection…
          </option>
          {addable.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
        </select>
      )}
    </Group>
  );
}

/** The byte-identical copies of this asset. In the grid/table those copies collapse into one badged
 *  card; this is where the full set is enumerated (the request in the golden rules: "duplicates listed
 *  in the inspector"). Exact only — perceptual near-matches are the separate "Similar" surface. Absent
 *  entirely when the asset has no identical twin, so the panel stays lean for the common case. */
function DuplicatesSection({ asset }: { asset: Asset }) {
  const { patch } = useViewState();
  const id = asset.summary.id;
  // A peer-local UUID may collide with a local UUID. Never resolve it against this server's local
  // duplicate index; peer duplicate review belongs to the owning peer.
  const dups = useDuplicateGroup(isLocal(asset.summary.origin) ? id : null);
  const group = dups.data;
  if (!group) return null;

  const others = group.total_members - 1;
  return (
    <Group title={`Duplicates (${others})`}>
      <p className="mb-2 text-[11px] text-fg-dim">
        {others} byte-identical {others === 1 ? "copy" : "copies"} (same content hash). 3DAM only
        groups — dispose of a copy from its context menu.
      </p>
      <div className="grid grid-cols-3 gap-1.5">
        {group.members.map((m) => (
          <DuplicateTile
            key={m.asset.id}
            member={m.asset}
            keep={(group.chosen_keep ?? group.suggested_keep) === m.asset.id}
            current={m.asset.id === id}
            onOpen={() => patch({
              selected: m.asset.id,
              owner: typeof m.asset.origin === "object" ? m.asset.source_id : null,
            })}
          />
        ))}
      </div>
      {group.members.length < group.total_members && (
        <p className="mt-2 text-[11px] text-fg-dim">
          Showing {group.members.length} of {group.total_members} copies. Open Duplicate review to
          page through the set.
        </p>
      )}
    </Group>
  );
}

function DuplicateTile({
  member,
  keep,
  current,
  onOpen,
}: {
  member: AssetSummary;
  keep: boolean;
  current: boolean;
  onOpen: () => void;
}) {
  return (
    <button
      className="flex flex-col overflow-hidden rounded border text-left transition-colors hover:border-border-strong coarse:min-h-11"
      style={{ borderColor: current ? "var(--color-accent)" : "var(--color-border)" }}
      title={`${member.name} · ${bytes(member.size)}${keep ? " · suggested keep" : ""}${current ? " · this asset" : ""}`}
      onClick={onOpen}
    >
      <span className="relative aspect-square w-full">
        <Thumbnail asset={member} size={32} />
        {keep && (
          <span className="absolute top-1 left-1 rounded bg-accent px-1 py-0.5 text-[9px] font-semibold text-accent-fg">
            Keep
          </span>
        )}
      </span>
      <span className="flex items-center justify-between gap-1 px-1 py-0.5">
        <span className="min-w-0 truncate text-[10px] text-fg-muted">{member.name}</span>
        <span className="shrink-0 text-[10px] text-fg-dim tabular-nums">{bytes(member.size)}</span>
      </span>
    </button>
  );
}

/** "Find similar" (tech-spec 05 §3): an opt-in ranked strip of neighbours by embedding cosine.
 *  Un-analyzed assets have no vector, so we offer to analyze first rather than query into the void.
 *  Each hit is selectable — clicking swaps the Inspector to that asset (and back/forward works, since
 *  selection lives in the URL). */
function SimilarSection({ asset }: { asset: Asset }) {
  const { patch } = useViewState();
  const [open, setOpen] = useState(false);
  const analyzed = asset.timestamps.analyzed != null;
  const analyze = useAnalyze();
  const peerTitle = peerReadOnlyTitle(asset.summary.origin);
  const similar = useSimilar(asset.summary.id, open && analyzed);
  const sectionRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onShortcut = (event: Event) => {
      if ((event as CustomEvent<ShortcutId>).detail !== "find-similar") return;
      // Inspector content exists twice responsively; act only in the visible rail/drawer instance.
      if (!sectionRef.current || sectionRef.current.getClientRects().length === 0) return;
      if (analyzed) setOpen(true);
      else if (!analyze.isPending && !peerTitle) analyze.mutate({ assets: [asset.summary.id] });
    };
    window.addEventListener(SHORTCUT_EVENT, onShortcut);
    return () => window.removeEventListener(SHORTCUT_EVENT, onShortcut);
  }, [analyze, analyzed, asset.summary.id, peerTitle]);

  if (!analyzed) {
    return (
      <div ref={sectionRef}>
        <Group title="Similar">
          <p className="text-[11px] text-fg-dim italic">
            Analyze this asset to find visually similar ones.
          </p>
          <button
            className="btn mt-2 disabled:cursor-not-allowed disabled:opacity-40"
            disabled={analyze.isPending || !!peerTitle}
            title={peerTitle ?? `Analyze and find similar (${shortcutLabel("find-similar")})`}
            aria-keyshortcuts="S"
            onClick={() => analyze.mutate({ assets: [asset.summary.id] })}
          >
            <Sparkles size={12} />
            {analyze.isPending ? "Analyzing…" : "Analyze now"}
          </button>
        </Group>
      </div>
    );
  }

  if (!open) {
    return (
      <div ref={sectionRef}>
        <Group title="Similar">
          <button
            className="btn"
            onClick={() => setOpen(true)}
            title={`Find similar (${shortcutLabel("find-similar")})`}
            aria-keyshortcuts="S"
          >
            <Sparkles size={12} />
            Find similar
          </button>
        </Group>
      </div>
    );
  }

  const hits = similar.data?.items ?? [];
  return (
    <div ref={sectionRef}>
      <Group title="Similar">
        {similar.isLoading ? (
          <p className="text-[11px] text-fg-dim">Searching…</p>
        ) : similar.isError ? (
          <p className="text-[11px] text-danger">Could not search for similar assets.</p>
        ) : hits.length === 0 ? (
          <p className="text-[11px] text-fg-dim italic">No similar assets found.</p>
        ) : (
          <div className="grid grid-cols-3 gap-1.5">
            {hits.map((hit) => (
              <SimilarTile
                key={hit.asset.id}
                hit={hit}
                onOpen={() => patch({
                  selected: hit.asset.id,
                  owner: typeof hit.asset.origin === "object" ? hit.asset.source_id : null,
                })}
              />
            ))}
          </div>
        )}
      </Group>
    </div>
  );
}

function SimilarTile({ hit, onOpen }: { hit: SimilarHit; onOpen: () => void }) {
  const pct = Math.round(hit.score * 100);
  return (
    <button
      className="group flex flex-col overflow-hidden rounded border border-border bg-bg text-left transition-colors hover:border-border-strong coarse:min-h-11"
      title={`${hit.asset.name} · ${pct}% similar · ${hit.space}`}
      onClick={onOpen}
    >
      <span className="aspect-square w-full">
        <Thumbnail asset={hit.asset} size={32} />
      </span>
      <span className="flex items-center justify-between gap-1 px-1 py-0.5">
        <span className="min-w-0 truncate text-[10px] text-fg-muted">{hit.asset.name}</span>
        <span className="shrink-0 text-[10px] font-medium text-accent">{pct}%</span>
      </span>
    </button>
  );
}

/** Automatic tag/class review. Pending automation is visible here but confirmed-only discovery
 *  keeps it out of search and filters until a user accepts it. Decisions are reversible. */
function TagList({
  assetId,
  tags,
  origin,
}: {
  assetId: AssetId;
  tags: TagRef[];
  origin: Origin;
}) {
  const review = useReviewSuggestion();
  const edit = useEditTags();
  const canWrite = useCan("write");
  const [newTag, setNewTag] = useState("");
  const vocabulary = useTagVocabulary(newTag.trim());
  // Tag review mutates this instance's catalog — a peer-owned asset's tags are reviewed on the peer.
  const peerTitle = peerReadOnlyTitle(origin);
  const manual = tags.filter((tag) => tag.source !== "auto");
  const automatic = tags.filter((tag) => tag.source === "auto");
  const disabled = !canWrite || !!peerTitle || edit.isPending;
  const add = () => {
    const tag = newTag.trim();
    if (!tag) return;
    edit.mutate(
      { assets: [assetId], add: [tag], dry_run: false },
      { onSuccess: () => setNewTag("") },
    );
  };
  return (
    <div className="flex flex-col gap-2">
      <div>
        <p className="mb-1 text-[10px] font-medium tracking-wide text-fg-dim uppercase">Manual</p>
        <div className="flex flex-wrap gap-1">
          {manual.length === 0 && <span className="text-[11px] text-fg-dim italic">No manual tags</span>}
          {manual.map((tag) => (
            <span key={tag.name} className="inline-flex items-center gap-1 rounded bg-surface-2 px-1.5 py-0.5 text-[10px] text-fg-muted">
              {tag.name}
              <button
                aria-label={`Remove manual tag ${tag.name}`}
                title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : "Remove manual tag")}
                disabled={disabled}
                onClick={() => edit.mutate({ assets: [assetId], remove: [tag.name], dry_run: false })}
                className="hover:text-danger disabled:cursor-not-allowed disabled:opacity-40"
              >
                <X size={10} />
              </button>
            </span>
          ))}
        </div>
        <div className="mt-1.5 flex gap-1">
          <input
            className="field min-w-0"
            aria-label="New manual tag"
            placeholder="Add a manual tag…"
            list="known-manual-tags"
            value={newTag}
            disabled={disabled}
            onChange={(event) => setNewTag(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                add();
              }
            }}
            title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : undefined)}
          />
          <button className="btn" disabled={disabled || !newTag.trim()} onClick={add}>Add</button>
          <datalist id="known-manual-tags">
            {(vocabulary.data ?? []).map((tag) => <option key={tag.name} value={tag.name} />)}
          </datalist>
        </div>
      </div>
      <div>
        <p className="text-[10px] font-medium tracking-wide text-fg-dim uppercase">
          Automatic suggestions &amp; classifications
        </p>
        <p className="mb-1.5 text-[10px] text-fg-dim">
          Only accepted suggestions affect search and filters.
        </p>
        {automatic.length === 0 ? (
          <p className="text-[11px] text-fg-dim italic">No automatic suggestions</p>
        ) : (
          <div className="flex flex-col gap-1.5">
            {automatic.map((tag) => (
              <TagChip
                key={tag.name}
                tag={tag}
                busy={review.isPending && review.variables?.tag === tag.name}
                peerTitle={peerTitle}
                onReview={(action) => review.mutate({ asset: assetId, tag: tag.name, action })}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function TagChip({
  tag,
  busy,
  peerTitle,
  onReview,
}: {
  tag: TagRef;
  busy: boolean;
  peerTitle?: string;
  onReview: (action: ReviewAction) => void;
}) {
  const auto = tag.source === "auto";
  const canWrite = useCan("write");
  const confidence = tag.confidence != null
    ? `${Math.round(tag.confidence * 100)}% confidence`
    : "Confidence unavailable";

  // User tags (and any non-auto) are not reviewable — render a plain chip.
  if (!auto) {
    return (
      <span
        className="rounded bg-surface-2 px-1.5 py-0.5 text-[10px] text-fg-muted"
        title="user tag"
      >
        {tag.name}
      </span>
    );
  }

  const pending = tag.state === "pending";
  const confirmed = tag.state === "confirmed";
  const disabled = busy || !canWrite || !!peerTitle;
  return (
    <article
      tabIndex={0}
      className={`rounded border p-2 text-[10px] focus-visible:outline-2 focus-visible:outline-accent ${
        pending
          ? "border-warn/50 bg-warn/5"
          : confirmed
            ? "border-accent/50 bg-accent/5"
            : "border-border bg-surface-2"
      }`}
      aria-label={`${tag.name}, ${tag.state}, ${confidence}`}
      onKeyDown={(event) => {
        if (
          event.target !== event.currentTarget ||
          event.altKey ||
          event.ctrlKey ||
          event.metaKey ||
          event.shiftKey ||
          disabled
        )
          return;
        const key = event.key.toLowerCase();
        if (pending && (key === "y" || key === "n")) {
          event.preventDefault();
          onReview(key === "y" ? "accept" : "reject");
        } else if (!pending && key === "u") {
          event.preventDefault();
          onReview("undo");
        }
      }}
    >
      <div className="flex items-center gap-2">
        <span className={`font-medium ${tag.state === "rejected" ? "text-fg-dim line-through" : "text-fg"}`}>
          {tag.name}
        </span>
        <span className={`rounded px-1 py-0.5 font-medium ${
          pending ? "text-warn" : confirmed ? "text-accent" : "text-danger"
        }`}>
          {pending ? "Pending" : confirmed ? "Accepted" : "Rejected"}
        </span>
        <span className="ml-auto tabular-nums text-fg-muted">{confidence}</span>
      </div>
      <p className="mt-1 text-fg-dim">Why: {tag.why || "The automated analyser proposed this value."}</p>
      {pending ? (
        <div className="mt-1.5 flex gap-1.5">
          <button
            className="btn flex-1 justify-center coarse:min-h-11"
            title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : "Accept suggestion (Y)")}
            aria-label={`Accept suggestion ${tag.name}`}
            aria-keyshortcuts="Y"
            disabled={disabled}
            onClick={() => onReview("accept")}
          >
            <Check size={12} /> Accept <kbd className="text-[9px] text-fg-dim">Y</kbd>
          </button>
          <button
            className="btn flex-1 justify-center text-danger coarse:min-h-11"
            title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : "Reject suggestion (N)")}
            aria-label={`Reject suggestion ${tag.name}`}
            aria-keyshortcuts="N"
            disabled={disabled}
            onClick={() => onReview("reject")}
          >
            <X size={12} /> Reject <kbd className="text-[9px] text-fg-dim">N</kbd>
          </button>
        </div>
      ) : (
        <button
          className="btn mt-1.5 w-full justify-center coarse:min-h-11"
          title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : "Undo decision (U)")}
          aria-label={`Undo ${tag.state} suggestion ${tag.name}`}
          aria-keyshortcuts="U"
          disabled={disabled}
          onClick={() => onReview("undo")}
        >
          <RotateCcw size={12} /> Undo <kbd className="text-[9px] text-fg-dim">U</kbd>
        </button>
      )}
    </article>
  );
}

/** The licence block: the derived badge, the rights readout, and the editor behind it (issue #106).
 *
 *  Peer-owned assets are read-only references (tech-spec 07 §7.4) — their rights are managed on the
 *  owning peer and attributed to it here, never edited through this server. */
function LicenseSection({ asset }: { asset: Asset }) {
  const { license } = asset;
  const canWrite = useCan("write");
  const peerTitle = peerReadOnlyTitle(asset.summary.origin);
  const readOnly = !canWrite || !!peerTitle;
  const [editing, setEditing] = useState(false);
  /** The status the *server* derived on the last save — never a locally guessed one. */
  const [derived, setDerived] = useState<LicenseEditResult | null>(null);

  return (
    <div className="mt-2">
      <div className="flex items-start justify-between gap-2">
        <LicenseBadge badge={{ id: license.id, status: license.status }} prominent />
        {!editing && (
          <button
            type="button"
            className="btn shrink-0 px-1.5 py-1 disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11"
            disabled={readOnly}
            title={
              peerTitle ??
              (!canWrite ? AUTH_COPY.needsWrite : "Edit licence and usage rights")
            }
            aria-label="Edit licence"
            onClick={() => {
              setDerived(null);
              setEditing(true);
            }}
          >
            <Pencil size={12} /> Licence
          </button>
        )}
      </div>
      {typeof asset.summary.origin !== "string" && (
        <p className="mt-1 text-[10px] text-fg-dim">
          Licence recorded by peer “{asset.summary.origin.peer}” — read-only here. Federated rights
          are attributed to their owner and managed there.
        </p>
      )}
      {editing ? (
        <LicenseEditor
          asset={asset}
          onDone={(result) => {
            setDerived(result);
            setEditing(false);
          }}
          onCancel={() => setEditing(false)}
        />
      ) : (
        <>
          <Rights license={license} />
          {derived && <DerivedStatus result={derived} />}
        </>
      )}
    </div>
  );
}

/** What the server derived from the id + rights it just stored (tech-spec 02 §5). Shown verbatim so
 *  the user learns the rule — notably that "permissive" is unreachable without a named licence and
 *  four known rights. */
function DerivedStatus({ result }: { result: LicenseEditResult }) {
  return (
    <div className="mt-2 rounded border border-border bg-surface-2 p-2 text-[11px] text-fg-muted">
      <div className="flex flex-wrap items-center gap-1.5">
        <span>Saved — status now</span>
        {result.status.length === 0 ? (
          <span className="text-fg-dim">unchanged</span>
        ) : (
          result.status.map((entry) => (
            <span key={entry.status} className="inline-flex items-center gap-1">
              <LicenseBadge badge={{ id: null, status: entry.status }} />
              {result.status.length > 1 && <span className="tabular-nums">×{entry.count}</span>}
            </span>
          ))
        )}
      </div>
      {result.warnings.map((warning, index) => (
        <p key={`${warning.code}:${warning.subject}:${index}`} className="mt-1 text-warn">
          {warning.message}
        </p>
      ))}
    </div>
  );
}

function Rights({ license }: { license: Asset["license"] }) {
  const flags: [string, boolean | null][] = [
    ["Commercial", license.commercial],
    ["Modify", license.modify],
    ["Redistribute", license.redistribute],
    ["Attribution", license.attribution],
  ];
  const known = flags.filter(([, v]) => v !== null);
  if (known.length === 0 && !license.holder && !license.credit && !license.url) return null;
  return (
    <div className="mt-2 space-y-1">
      {license.holder && <Field label="Holder" value={license.holder} />}
      {license.credit && <Field label="Credit" value={license.credit} />}
      {license.url && <Field label="Source" value={license.url} copyable />}
      {known.length > 0 && (
        <div className="flex flex-wrap gap-1 pt-1">
          {known.map(([label, v]) => (
            <span
              key={label}
              className="rounded px-1.5 py-0.5 text-[10px]"
              style={{
                color: v ? "var(--color-lic-permissive)" : "var(--color-lic-restricted)",
                background: "color-mix(in srgb, currentColor 12%, transparent)",
              }}
            >
              {v ? "✓" : "✕"} {label}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

interface LicenseDraft {
  id: string;
  commercial: TriState;
  modify: TriState;
  redistribute: TriState;
  attribution: TriState;
  holder: string;
  credit: string;
  url: string;
}

/** Per-asset licence editor. A single-asset edit owns the whole block, so it sends every field
 *  explicitly — the three-state patch earns its keep in the *bulk* editor, where "leave this alone"
 *  is a real answer. Each right is three-state here too: conflating "unknown" with "no" is the exact
 *  bug the nullable rights columns exist to prevent. */
function LicenseEditor({
  asset,
  onDone,
  onCancel,
}: {
  asset: Asset;
  onDone: (result: LicenseEditResult) => void;
  onCancel: () => void;
}) {
  const setLicense = useSetLicense();
  const { license } = asset;
  const [draft, setDraft] = useState<LicenseDraft>({
    id: license.id ?? "",
    commercial: triFromBool(license.commercial),
    modify: triFromBool(license.modify),
    redistribute: triFromBool(license.redistribute),
    attribution: triFromBool(license.attribution),
    holder: license.holder ?? "",
    credit: license.credit ?? "",
    url: license.url ?? "",
  });
  const [presetNote, setPresetNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const field = <K extends keyof LicenseDraft>(key: K, value: LicenseDraft[K]) =>
    setDraft((prev) => ({ ...prev, [key]: value }));

  const applyPreset = (id: string) => {
    const preset = LICENSE_PRESETS.find((candidate) => candidate.id === id);
    if (!preset) return;
    setPresetNote(
      preset.note ??
        "Pre-filled from a well-known licence — review it against the actual grant, then save.",
    );
    setDraft((prev) => ({
      ...prev,
      id: preset.id,
      ...(preset.rights
        ? {
            commercial: triFromBool(preset.rights.commercial),
            modify: triFromBool(preset.rights.modify),
            redistribute: triFromBool(preset.rights.redistribute),
            attribution: triFromBool(preset.rights.attribution),
          }
        : {}),
    }));
  };

  const save = () => {
    setError(null);
    const patch: LicenseInput = {
      id: textPatch(draft.id),
      commercial: triToBool(draft.commercial),
      modify: triToBool(draft.modify),
      redistribute: triToBool(draft.redistribute),
      attribution: triToBool(draft.attribution),
      holder: textPatch(draft.holder),
      credit: textPatch(draft.credit),
      url: textPatch(draft.url),
    };
    setLicense.mutate(
      { assets: [asset.summary.id], license: patch },
      {
        onSuccess: onDone,
        onError: (reason) =>
          setError(reason instanceof ApiError ? reason.message : String(reason)),
      },
    );
  };

  return (
    <div className="mt-2 space-y-2 rounded border border-border bg-surface-2 p-2">
      <p className="text-[10px] text-fg-dim">
        The badge is derived from the identifier plus the four rights — it is never something you set
        directly. “Unknown” stays unknown until someone establishes it.
      </p>

      <label className="block text-[11px] text-fg-muted">
        Identifier
        <input
          className="field mt-1"
          value={draft.id}
          list="license-preset-ids"
          placeholder="e.g. CC-BY-4.0, Proprietary"
          onChange={(event) => field("id", event.target.value)}
        />
      </label>
      <datalist id="license-preset-ids">
        {LICENSE_PRESETS.map((preset) => (
          <option key={preset.id} value={preset.id}>
            {preset.label}
          </option>
        ))}
      </datalist>

      <label className="block text-[11px] text-fg-muted">
        Pre-fill from a known licence
        <select
          className="field mt-1"
          value=""
          aria-label="Pre-fill from a known licence"
          onChange={(event) => {
            if (event.target.value) applyPreset(event.target.value);
            event.currentTarget.value = "";
          }}
        >
          <option value="">Choose a licence…</option>
          {LICENSE_PRESETS.map((preset) => (
            <option key={preset.id} value={preset.id}>
              {preset.label}
            </option>
          ))}
        </select>
      </label>
      {presetNote && <p className="text-[10px] text-warn">{presetNote}</p>}

      {RIGHTS_FIELDS.map(([key, label, hint]) => (
        <label key={key} className="block text-[11px] text-fg-muted" title={hint}>
          {label}
          <select
            className="field mt-1"
            value={draft[key]}
            aria-label={label}
            onChange={(event) => field(key, event.target.value as TriState)}
          >
            {TRI_OPTIONS.map(([value, optionLabel]) => (
              <option key={value} value={value}>
                {optionLabel}
              </option>
            ))}
          </select>
        </label>
      ))}

      <label className="block text-[11px] text-fg-muted">
        Rights holder
        <input
          className="field mt-1"
          value={draft.holder}
          placeholder="Who owns it"
          onChange={(event) => field("holder", event.target.value)}
        />
      </label>
      <label className="block text-[11px] text-fg-muted">
        Credit line
        <input
          className="field mt-1"
          value={draft.credit}
          placeholder="How to credit them"
          onChange={(event) => field("credit", event.target.value)}
        />
      </label>
      <label className="block text-[11px] text-fg-muted">
        Source URL
        <input
          className="field mt-1"
          type="url"
          value={draft.url}
          placeholder="https://…"
          onChange={(event) => field("url", event.target.value)}
        />
      </label>

      {error && (
        <p role="alert" className="text-[11px] text-danger">
          {error}
        </p>
      )}
      <div className="flex justify-end gap-2">
        <button className="btn" onClick={onCancel} disabled={setLicense.isPending}>
          Cancel
        </button>
        <button className="btn btn-accent" onClick={save} disabled={setLicense.isPending}>
          {setLicense.isPending ? "Saving…" : "Save licence"}
        </button>
      </div>
    </div>
  );
}

function MediaFacts({ attrs }: { attrs: MediaAttributes }) {
  if (attrs.media === "none") return null;
  const rows: [string, string][] = [];
  if (attrs.media === "audio") {
    if (attrs.duration_ms != null) rows.push(["Duration", duration(attrs.duration_ms)]);
    if (attrs.sample_rate != null) rows.push(["Sample rate", `${attrs.sample_rate} Hz`]);
    if (attrs.bit_depth != null) rows.push(["Bit depth", `${attrs.bit_depth}-bit`]);
    if (attrs.channels != null) rows.push(["Channels", channelLabel(attrs.channels)]);
    if (attrs.codec) rows.push(["Codec", attrs.codec]);
    if (attrs.container) rows.push(["Container", attrs.container]);
  } else if (attrs.media === "image") {
    if (attrs.width != null && attrs.height != null)
      rows.push(["Dimensions", `${attrs.width} × ${attrs.height}`]);
    if (attrs.color_depth != null) rows.push(["Bit depth", `${attrs.color_depth}-bit`]);
    if (attrs.has_alpha != null) rows.push(["Alpha", attrs.has_alpha ? "yes" : "no"]);
    if (attrs.color_space) rows.push(["Color space", attrs.color_space]);
  }

  // Seamlessness readout for images (issue #57) — the analysis pass's edge-continuity score +
  // `seamless | tiled | non_tiling` class. Rendered as its own block (badge + score bar), separate
  // from the plain key/value rows. Absent until `analyze` has run.
  const seam =
    attrs.media === "image" && (attrs.tileability != null || attrs.tile_class) ? (
      <Seamlessness
        tileability={attrs.tileability ?? null}
        tileClass={attrs.tile_class ?? null}
        repeatPeriod={attrs.repeat_period ?? null}
      />
    ) : null;

  if (attrs.media === "model") {
    if (attrs.vertex_count != null) rows.push(["Vertices", attrs.vertex_count.toLocaleString()]);
    if (attrs.triangle_count != null)
      rows.push(["Triangles", attrs.triangle_count.toLocaleString()]);
    if (attrs.mesh_count != null) rows.push(["Meshes", String(attrs.mesh_count)]);
    if (attrs.material_count != null) rows.push(["Materials", String(attrs.material_count)]);
    if (attrs.texture_count != null) rows.push(["Textures", String(attrs.texture_count)]);
    if (attrs.has_rig != null) rows.push(["Rigged", attrs.has_rig ? "yes" : "no"]);
    if (attrs.has_animation != null) rows.push(["Animation", attrs.has_animation ? "yes" : "no"]);
    if (attrs.has_uvs != null) rows.push(["UVs", attrs.has_uvs ? "yes" : "no"]);
  }
  if (attrs.media === "video") {
    if (attrs.duration_ms != null) rows.push(["Duration", duration(attrs.duration_ms)]);
    if (attrs.width != null && attrs.height != null)
      rows.push(["Dimensions", `${attrs.width} × ${attrs.height}`]);
    if (attrs.fps != null) rows.push(["Frame rate", `${attrs.fps.toFixed(2)} fps`]);
    if (attrs.codec) rows.push(["Codec", attrs.codec]);
    if (attrs.container) rows.push(["Container", attrs.container]);
    if (attrs.bitrate != null)
      rows.push(["Bitrate", `${Math.round(attrs.bitrate / 1000).toLocaleString()} kbps`]);
    if (attrs.has_audio != null) rows.push(["Audio track", attrs.has_audio ? "yes" : "no"]);
  }
  if (attrs.media === "document") {
    if (attrs.title) rows.push(["Title", attrs.title]);
    if (attrs.author) rows.push(["Author", attrs.author]);
    if (attrs.page_count != null) rows.push(["Pages", attrs.page_count.toLocaleString()]);
    if (attrs.word_count != null) rows.push(["Words", attrs.word_count.toLocaleString()]);
    if (attrs.encoding) rows.push(["Encoding", attrs.encoding]);
  }
  // "Extracted features" for audio (issue #61): the analysis pass's continuous acoustic signals —
  // loudness (text) + brightness / harmonicity (0–1 bars). Its own group, separate from the container
  // metadata rows above. Absent until `analyze` has run.
  const audioFeatures =
    attrs.media === "audio" &&
    (attrs.loudness_lufs != null || attrs.brightness != null || attrs.harmonicity != null) ? (
      <AudioFeatures
        loudness={attrs.loudness_lufs ?? null}
        brightness={attrs.brightness ?? null}
        harmonicity={attrs.harmonicity ?? null}
      />
    ) : null;

  if (rows.length === 0 && !seam && !audioFeatures) return null;
  return (
    <>
      {(rows.length > 0 || seam) && (
        <Group title="Media">
          {rows.map(([label, value]) => (
            <Field key={label} label={label} value={value} />
          ))}
          {seam}
        </Group>
      )}
      {audioFeatures}
    </>
  );
}

/** "Extracted features" readout for audio (issue #61): integrated loudness as a value, plus brightness
 *  (spectral centroid) and harmonicity (harmonic-vs-noise) as 0–1 bars — the audio analogue of the
 *  image Seamlessness block. Only rendered once the analyze pass has measured them. */
function AudioFeatures({
  loudness,
  brightness,
  harmonicity,
}: {
  loudness: number | null;
  brightness: number | null;
  harmonicity: number | null;
}) {
  return (
    <Group title="Extracted features">
      {loudness != null && (
        <Field label="Loudness" value={`${loudness.toFixed(1)} LUFS`} />
      )}
      {brightness != null && <FeatureBar label="Brightness" value={brightness} />}
      {harmonicity != null && <FeatureBar label="Harmonicity" value={harmonicity} />}
    </Group>
  );
}

/** A labelled 0–1 feature bar (raw value on hover) — the shared shape behind the audio brightness /
 *  harmonicity readouts. */
function FeatureBar({ label, value }: { label: string; value: number }) {
  const pct = Math.round(Math.max(0, Math.min(1, value)) * 100);
  return (
    <div className="pt-1.5">
      <div className="mb-1 flex items-center justify-between">
        <span className="text-[11px] text-fg-dim">{label}</span>
        <span className="text-[10px] tabular-nums text-fg-muted">{pct}%</span>
      </div>
      <div className="h-1.5 overflow-hidden rounded bg-surface-2" title={value.toFixed(3)}>
        <div style={{ width: `${pct}%`, height: "100%", background: "var(--color-accent)" }} />
      </div>
    </div>
  );
}

/** Seamlessness readout for a texture (issue #57): the `seamless | tiled | non_tiling` class as a
 *  coloured badge and the 0–1 edge-continuity score as a bar (raw value on hover). Only rendered once
 *  the analysis pass has scored the image. */
function Seamlessness({
  tileability,
  tileClass,
  repeatPeriod,
}: {
  tileability: number | null;
  tileClass: string | null;
  repeatPeriod: number | null;
}) {
  const pct = tileability != null ? Math.round(tileability * 100) : null;
  const label =
    tileClass === "seamless"
      ? "Seamless"
      : tileClass === "tiled"
        ? "Tiled"
        : tileClass === "non_tiling"
          ? "Non-tiling"
          : null;
  const tone =
    tileClass === "seamless"
      ? "var(--color-lic-permissive)"
      : tileClass === "tiled"
        ? "var(--color-accent)"
        : "var(--color-fg-dim)";

  return (
    <div className="pt-1.5">
      <div className="mb-1 flex items-center justify-between">
        <span className="text-[11px] text-fg-dim">Seamlessness</span>
        {label && (
          <span
            className="rounded px-1.5 py-0.5 text-[10px] font-medium"
            style={{ color: tone, background: "color-mix(in srgb, currentColor 14%, transparent)" }}
          >
            {label}
          </span>
        )}
      </div>
      {pct != null && (
        <div
          className="flex items-center gap-2"
          title={`Edge-continuity tileability: ${tileability!.toFixed(3)}`}
        >
          <div className="h-1.5 flex-1 overflow-hidden rounded bg-surface-2">
            <div style={{ width: `${pct}%`, height: "100%", background: tone }} />
          </div>
          <span className="shrink-0 text-[10px] tabular-nums text-fg-muted">{pct}%</span>
        </div>
      )}
      {repeatPeriod != null && (
        <p className="mt-1 text-[10px] text-fg-dim">Repeat period ~{repeatPeriod}px</p>
      )}
    </div>
  );
}

/** Human channel label: 1 → Mono, 2 → Stereo, otherwise "N ch". */
function channelLabel(n: number): string {
  if (n === 1) return "Mono";
  if (n === 2) return "Stereo";
  return `${n} ch`;
}

function Group({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="mt-4">
      <div className="mb-1.5 text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
        {title}
      </div>
      <div className="space-y-1">{children}</div>
    </div>
  );
}

function Field({
  label,
  value,
  mono,
  copyable,
}: {
  label: string;
  value: string;
  mono?: boolean;
  copyable?: boolean;
}) {
  return (
    <div className="flex items-baseline justify-between gap-2 text-[11px]">
      <span className="shrink-0 text-fg-dim">{label}</span>
      <span className="flex min-w-0 items-center gap-1">
        <span
          className={`min-w-0 truncate text-right text-fg-muted ${mono ? "font-mono text-[10px]" : ""}`}
          title={value}
        >
          {value}
        </span>
        {copyable && (
          <button
            type="button"
            className="flex shrink-0 items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
            title={`Copy ${label.toLowerCase()}`}
            aria-label={`Copy ${label.toLowerCase()}`}
            onClick={() => void copyText(value, label)}
          >
            <ClipboardCopy size={12} />
          </button>
        )}
      </span>
    </div>
  );
}
