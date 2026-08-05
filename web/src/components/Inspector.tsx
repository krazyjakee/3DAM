import { useEffect } from "react";
import {
  PanelRightClose,
  PanelRightOpen,
  RefreshCw,
  Sparkles,
  Star,
  X,
} from "lucide-react";
import {
  useAnalyze,
  useAsset,
  useCan,
  useRegenerateThumbnail,
  useSetFavorite,
  useSources,
} from "@/api/queries";
import { ApiError } from "@/api/client";
import { AUTH_COPY } from "@/lib/auth";
import type { Asset } from "@/api/types";
import { bytes, mediaLabel, originLabel, relTime } from "@/lib/format";
import { peerReadOnlyTitle } from "@/lib/origin";
import { useViewState } from "@/lib/view-state";
import { shortcutLabel, SHORTCUT_EVENT, type ShortcutId } from "@/lib/shortcuts";
// The preview cluster is its own module (issue #167): one entry point over a media lifecycle
// (blob vs stream ticket, plus renewal) shared by the 3D island, audio, image, video and document
// previews. The WASM island stays behind its dynamic import, so this file pulls no wgpu.
import { Preview } from "./inspector/preview";
// …and the metadata panels are their own modules too (issue #166). Each owns exactly one
// query/mutation pair and takes a single `asset` prop, so `Body` below stays a manifest of panels
// and each panel can be rendered from a fixture without mounting the workspace.
import { CollectionsGroup } from "./inspector/Collections";
import { DuplicatesSection } from "./inspector/Duplicates";
import { LicenseSection } from "./inspector/License";
import { MediaFacts } from "./inspector/MediaFacts";
import { NoteEditor } from "./inspector/NoteEditor";
import { Field, Group } from "./inspector/primitives";
import { SimilarSection } from "./inspector/Similar";
import { TagList } from "./inspector/Tags";
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

/** The inspected asset, as a manifest of panels — one per concern, each imported from
 *  `./inspector/` (issue #166). Everything below the title block is a panel that fetches its own
 *  data from the `asset` it is handed, so the order here is the whole layout decision. */
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
