import type React from "react";
import { useEffect, useRef, useState } from "react";
import {
  Box,
  ClipboardCopy,
  Grid3x3,
  PanelRightClose,
  PanelRightOpen,
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
  useDuplicates,
  useRegenerateThumbnail,
  useReviewSuggestion,
  useSetFavorite,
  useSetNote,
  useSimilar,
  useSources,
  useVersion,
} from "@/api/queries";
import { api, ApiError } from "@/api/client";
import { AUTH_COPY } from "@/lib/auth";
import type {
  Asset,
  AssetId,
  AssetSummary,
  CollectionId,
  MediaAttributes,
  Origin,
  ReviewAction,
  SimilarHit,
  TagRef,
} from "@/api/types";
import { bytes, duration, mediaLabel, originLabel, relTime } from "@/lib/format";
import { copyText } from "@/lib/clipboard";
import { DUPLICATE_QUERY_LIMIT } from "@/lib/limits";
import { peerReadOnlyTitle } from "@/lib/origin";
import { hasInteractive3D } from "@/lib/model-formats";
import { useViewState } from "@/lib/view-state";
import { ModelViewerIsland } from "@/islands/ModelViewerIsland";
import { AudioPlayer } from "./AudioPlayer";
import { LicenseBadge } from "./LicenseBadge";
import { ImageViewer } from "./ImageViewer";
import { TilePreview } from "./TilePreview";
import { Thumbnail } from "./Thumbnail";
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

  return (
    <>
      {collapsed ? (
        // Collapsed rail (issue #65): a slim edge strip whose only control reopens the inspector.
        <aside className="hidden w-9 shrink-0 flex-col items-center border-l border-border bg-surface py-2 lg:flex">
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
  const asset = useAsset(selected);
  const { patch } = useViewState();
  const collapse = mode === "rail";

  // If the selected asset no longer exists — its source was removed, or it was removed + blocked
  // (#21) — clear the dangling selection instead of leaving the inspector stuck on an error (#29).
  // This is independent of the close button, which on the rail collapses rather than deselects (#65).
  useEffect(() => {
    if (asset.isError && asset.error instanceof ApiError && asset.error.status === 404) {
      patch({ selected: null });
    }
  }, [asset.isError, asset.error, patch]);

  // The header — and its collapse/close control — is always present, so the rail can be collapsed
  // even with nothing selected (issue #65). The body below swaps placeholder / skeleton / content.
  return (
    <div className="flex h-full flex-col overflow-y-auto">
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

/** The preview slot: an interactive WASM island for 3D models and audio (fed raw bytes from the
 *  file-03 content endpoint), or the server-rendered thumbnail for images and as the fallback. */
function Preview({ asset }: { asset: Asset }) {
  const { summary } = asset;
  const [tiling, setTiling] = useState(false);
  if (summary.media === "model") {
    // Interactive island for every Assimp-decodable mesh format (issue #18) — the server decodes the
    // preview blob so the DOM never resolves external buffers.
    if (hasInteractive3D(summary.format)) {
      return (
        <div className="aspect-square border-b border-border">
          <ModelViewerIsland src={api.assetPreviewMeshUrl(summary.id)} />
        </div>
      );
    }
    // `.blend` has no interactive 3D (Assimp can't decode a modern .blend), so it falls through to
    // the server thumbnail below — which surfaces Blender's own embedded preview image when present.
    // Anything else (the USD family) has no decode path yet: show a clear "preview not available"
    // state rather than a doomed fetch or a silent blank (issue #18 acceptance).
    if (summary.format !== "blend") {
      return <NoModelPreview format={summary.format} />;
    }
  }
  const src = api.assetContentUrl(summary.id);
  if (summary.media === "audio") {
    // Playable inline: waveform + transport, with the playhead driven by real progress (issues
    // #16, #14). Keyed by id so switching assets resets playback + the decoded waveform.
    // Peaks come from the server analysis pass (issue #73), so the waveform draws with no client-side
    // audio decode; null (not yet analysed) falls back to DOM decode inside the island.
    const peaks =
      asset.attributes?.media === "audio" ? (asset.attributes.peaks ?? null) : null;
    return (
      <div className="border-b border-border">
        <AudioPlayer key={summary.id} src={src} assetId={summary.id} peaks={peaks} />
      </div>
    );
  }
  if (summary.media === "image") {
    // Full-resolution content image in a zoom/pan viewer — 1:1 is pixel-accurate for inspecting
    // texture detail / tileability (issue #17). Keyed by id so switching assets resets the view.
    // A "Check tiling" affordance opens the interactive tile preview (cube / flat repeat, issue #58).
    return (
      <div className="relative aspect-square border-b border-border">
        <ImageViewer key={summary.id} src={src} alt={summary.name} />
        <button
          onClick={() => setTiling(true)}
          className="absolute top-1.5 right-1.5 flex items-center gap-1 rounded border border-border bg-surface/85 px-1.5 py-1 text-[10px] text-fg-muted backdrop-blur hover:text-fg coarse:min-h-11"
          title="Check tiling — wrap this texture on a cube / flat repeat"
        >
          <Grid3x3 size={12} /> Check tiling
        </button>
        {tiling && (
          <TilePreview src={src} name={summary.name} onClose={() => setTiling(false)} />
        )}
      </div>
    );
  }
  if (summary.media === "video") {
    // No WASM island and no transcode: the browser plays the original bytes natively. `preload`
    // stays on metadata so opening the inspector doesn't pull a 200 MB cutscene down before the
    // user has asked to watch it — the server's range support (see `ranged_content_response`) is
    // what makes both that and seeking work. Playback works regardless of what the *server* can
    // decode; only the metadata and poster frame depend on its ffmpeg (ADR 0015).
    return <VideoPreview key={summary.id} src={src} />;
  }
  if (summary.media === "document") {
    const excerpt =
      asset.attributes?.media === "document" ? (asset.attributes.excerpt ?? null) : null;
    return <DocumentPreview excerpt={excerpt} format={summary.format} src={src} />;
  }
  return (
    <div className="aspect-square border-b border-border">
      <Thumbnail asset={summary} size={64} />
    </div>
  );
}

/** Native `<video>` playback, plus an honest note when the server has no decode backend.
 *
 *  The two are independent and it matters not to conflate them: the browser plays the original
 *  bytes whatever the server can decode, so the video is watchable either way — but with no
 *  discovered `ffmpeg`/`ffprobe` (ADR 0015) the server can't report duration/codec/resolution or
 *  render a poster frame, and every video tile in the grid stays a typed glyph. Left unexplained
 *  that reads as a broken thumbnailer rather than a missing optional dependency, so we say it once,
 *  here, where the user is already looking at a video. */
function VideoPreview({ src }: { src: string }) {
  const version = useVersion();
  // Only claim a decoder is missing once we've actually heard from the server — mid-fetch,
  // `capabilities` is undefined, and asserting "not installed" then would be a guess.
  const probeMissing =
    version.data != null && !version.data.capabilities.includes("video_probe");
  return (
    <div className="border-b border-border">
      <video src={src} controls preload="metadata" playsInline className="max-h-[60vh] w-full bg-black">
        <track kind="captions" />
      </video>
      {probeMissing && (
        <p className="px-3 py-2 text-[11px] text-fg-dim">
          No video decoder on the server — playback works, but duration, codec and poster-frame
          thumbnails need <span className="font-mono">ffmpeg</span> installed where 3DAM runs. 3DAM
          looks for it once per run, so restart the server after installing it.
        </p>
      )}
    </div>
  );
}

/** The document preview: the extracted opening text, set as readable prose rather than rasterised
 *  server-side (PRODUCT_SPEC §9 phase 2b). A document with no text layer — a scanned-image PDF, or
 *  one not yet analysed — says so plainly instead of showing an empty card. "Open original" is the
 *  escape hatch to the real file for anything we can't typeset. */
function DocumentPreview({
  excerpt,
  format,
  src,
}: {
  excerpt: string | null;
  format: string;
  src: string;
}) {
  return (
    <div className="flex max-h-[50vh] flex-col gap-2 overflow-y-auto border-b border-border bg-surface-2 p-4">
      {excerpt ? (
        <p className="text-[12px] leading-relaxed whitespace-pre-wrap text-fg-muted">{excerpt}</p>
      ) : (
        <p className="text-[11px] text-fg-dim">
          No text extracted — this <span className="font-mono uppercase">.{format}</span> may have no
          text layer, or it hasn’t been analysed yet.
        </p>
      )}
      <a
        href={src}
        target="_blank"
        rel="noreferrer"
        className="self-start text-[11px] text-accent hover:underline coarse:min-h-11"
      >
        Open original
      </a>
    </div>
  );
}

/** "Preview not available" state for a 3D format with no decode path yet (the USD family) — issue #18.
 *  Metadata still loads below; this just makes the absent preview explicit rather than a blank tile. */
function NoModelPreview({ format }: { format: string }) {
  return (
    <div className="flex aspect-square flex-col items-center justify-center gap-2 border-b border-border bg-surface-2 px-6 text-center">
      <Box size={28} className="text-fg-dim" />
      <p className="text-xs font-medium text-fg-muted">Preview not available</p>
      <p className="text-[11px] text-fg-dim">
        3DAM can’t render <span className="font-mono uppercase">.{format}</span> yet — its metadata is
        still catalogued below.
      </p>
    </div>
  );
}

function Body({ asset }: { asset: Asset }) {
  const { summary, license } = asset;
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

        {/* license — HIGH in the inspector (DESIGN_GUIDELINES §3.1) */}
        <div className="mt-2">
          <LicenseBadge badge={{ id: license.id, status: license.status }} prominent />
          <Rights license={license} />
        </div>

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
      title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : label)}
      aria-label={label}
      aria-pressed={on}
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

/** Pull enough exact-duplicate groups to cover the library; keyed identically to the Browser's fetch
 *  so the two share one cached request under `qk.duplicates`. */

/** The byte-identical copies of this asset. In the grid/table those copies collapse into one badged
 *  card; this is where the full set is enumerated (the request in the golden rules: "duplicates listed
 *  in the inspector"). Exact only — perceptual near-matches are the separate "Similar" surface. Absent
 *  entirely when the asset has no identical twin, so the panel stays lean for the common case. */
function DuplicatesSection({ asset }: { asset: Asset }) {
  const { patch } = useViewState();
  const id = asset.summary.id;
  const dups = useDuplicates({ kind: "exact", limit: DUPLICATE_QUERY_LIMIT });
  const group = dups.data?.find(
    (g) => g.members.length > 1 && g.members.some((m) => m.id === id),
  );
  if (!group) return null;

  const others = group.members.length - 1;
  return (
    <Group title={`Duplicates (${others})`}>
      <p className="mb-2 text-[11px] text-fg-dim">
        {others} byte-identical {others === 1 ? "copy" : "copies"} (same content hash). 3DAM only
        groups — dispose of a copy from its context menu.
      </p>
      <div className="grid grid-cols-3 gap-1.5">
        {group.members.map((m) => (
          <DuplicateTile
            key={m.id}
            member={m}
            keep={m.id === group.suggested_keep}
            current={m.id === id}
            onOpen={() => patch({ selected: m.id })}
          />
        ))}
      </div>
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

  if (!analyzed) {
    return (
      <Group title="Similar">
        <p className="text-[11px] text-fg-dim italic">
          Analyze this asset to find visually similar ones.
        </p>
        <button
          className="btn mt-2 disabled:cursor-not-allowed disabled:opacity-40"
          disabled={analyze.isPending || !!peerTitle}
          title={peerTitle}
          onClick={() => analyze.mutate({ assets: [asset.summary.id] })}
        >
          <Sparkles size={12} />
          {analyze.isPending ? "Analyzing…" : "Analyze now"}
        </button>
      </Group>
    );
  }

  if (!open) {
    return (
      <Group title="Similar">
        <button className="btn" onClick={() => setOpen(true)}>
          <Sparkles size={12} />
          Find similar
        </button>
      </Group>
    );
  }

  const hits = similar.data?.items ?? [];
  return (
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
              onOpen={() => patch({ selected: hit.asset.id })}
            />
          ))}
        </div>
      )}
    </Group>
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

/** Auto-tag review (reject-only lifecycle): every auto-tag is active — it powers search and
 *  filtering as soon as the analysis pass proposes it, no accept step. The only review action is
 *  *reject*, which hides a wrong tag from search and stops the same extractor re-suggesting it;
 *  a rejected tag can be *restored*. User-authored tags are static — there is nothing to review. */
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
  // Tag review mutates this instance's catalog — a peer-owned asset's tags are reviewed on the peer.
  const peerTitle = peerReadOnlyTitle(origin);
  if (tags.length === 0) {
    return (
      <p className="text-[11px] text-fg-dim italic">
        No tags yet — the analysis pass proposes auto-tags that power search. Reject any that are wrong.
      </p>
    );
  }
  return (
    <div className="flex flex-wrap gap-1">
      {tags.map((t) => (
        <TagChip
          key={t.name}
          tag={t}
          busy={review.isPending && review.variables?.tag === t.name}
          peerTitle={peerTitle}
          onReview={(action) => review.mutate({ asset: assetId, tag: t.name, action })}
        />
      ))}
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
  const confidence =
    tag.confidence != null ? ` · ${Math.round(tag.confidence * 100)}%` : "";

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

  // Reject-only lifecycle: an auto tag is active (searchable) unless rejected. Active tags offer a
  // reject; rejected tags show struck-through with a restore. There is no accept-to-confirm step.
  const rejected = tag.state === "rejected";
  return (
    <span
      className="inline-flex items-center gap-1 rounded border px-1.5 py-0.5 text-[10px]"
      style={{
        borderColor: rejected ? "var(--color-border)" : "var(--color-accent)",
        color: rejected ? "var(--color-fg-dim)" : "var(--color-accent)",
        background: "color-mix(in srgb, currentColor 10%, transparent)",
      }}
      title={rejected ? `rejected auto tag${confidence}` : `auto tag${confidence} · powers search`}
    >
      <span className={rejected ? "line-through" : ""}>{tag.name}</span>
      {rejected ? (
        <button
          className="flex items-center justify-center hover:text-accent disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
          title={
            peerTitle ??
            (!canWrite ? AUTH_COPY.needsWrite : "Restore tag — include it in search again")
          }
          aria-label={`Restore tag ${tag.name}`}
          disabled={busy || !canWrite || !!peerTitle}
          onClick={() => onReview("accept")}
        >
          <RotateCcw size={12} />
        </button>
      ) : (
        <button
          className="flex items-center justify-center hover:text-danger disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
          title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : "Reject tag — hide it from search")}
          aria-label={`Reject tag ${tag.name}`}
          disabled={busy || !canWrite || !!peerTitle}
          onClick={() => onReview("reject")}
        >
          <X size={12} />
        </button>
      )}
    </span>
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
  if (known.length === 0 && !license.holder) return null;
  return (
    <div className="mt-2 space-y-1">
      {license.holder && <Field label="Holder" value={license.holder} />}
      {license.credit && <Field label="Credit" value={license.credit} />}
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
