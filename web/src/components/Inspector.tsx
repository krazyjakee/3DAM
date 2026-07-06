import type React from "react";
import { useState } from "react";
import { Check, PanelRightClose, Sparkles, X } from "lucide-react";
import {
  useAnalyze,
  useAsset,
  useCollectionMembers,
  useCollections,
  useReviewSuggestion,
  useSimilar,
  useSources,
} from "@/api/queries";
import { api } from "@/api/client";
import type {
  Asset,
  AssetId,
  CollectionId,
  MediaAttributes,
  ReviewAction,
  SimilarHit,
  TagRef,
} from "@/api/types";
import { bytes, duration, mediaLabel, originLabel, relTime } from "@/lib/format";
import { useViewState } from "@/lib/view-state";
import { ModelViewerIsland } from "@/islands/ModelViewerIsland";
import { AudioPlayer } from "./AudioPlayer";
import { LicenseBadge } from "./LicenseBadge";
import { ImageViewer } from "./ImageViewer";
import { Thumbnail } from "./Thumbnail";
import { MediaIcon } from "./MediaIcon";
import { Drawer } from "./Drawer";

/** Inspector — a persistent right rail on `lg`, an overlay drawer below it (responsive + touch pass). Both
 *  render the same {@link InspectorPanel}, so a 360px phone and a wide desktop show identical detail. */
export function Inspector({ width }: { width?: number }) {
  const { state, patch } = useViewState();
  const close = () => patch({ selected: null });

  return (
    <>
      <aside
        className="hidden shrink-0 flex-col border-l border-border bg-surface lg:flex"
        style={{ width }}
      >
        <InspectorPanel selected={state.selected} onClose={close} />
      </aside>
      <Drawer open={!!state.selected} onClose={close} side="right" label="Inspector">
        <InspectorPanel selected={state.selected} onClose={close} />
      </Drawer>
    </>
  );
}

function InspectorPanel({
  selected,
  onClose,
}: {
  selected: string | null;
  onClose: () => void;
}) {
  const asset = useAsset(selected);

  if (!selected) {
    return (
      <div className="flex h-full items-center justify-center px-6 text-center text-xs text-fg-dim">
        Select an asset to inspect it.
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-y-auto">
      <div className="flex items-center justify-between border-b border-border px-3 py-2">
        <span className="text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
          Inspector
        </span>
        <button
          className="flex items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
          title="Close"
          aria-label="Close inspector"
          onClick={onClose}
        >
          <PanelRightClose size={15} />
        </button>
      </div>

      {asset.isLoading ? (
        <div className="p-4 text-xs text-fg-dim">Loading…</div>
      ) : asset.isError || !asset.data ? (
        <div className="p-4 text-xs text-danger">Could not load this asset.</div>
      ) : (
        <Body asset={asset.data} />
      )}
    </div>
  );
}

/** The preview slot: an interactive WASM island for 3D models and audio (fed raw bytes from the
 *  file-03 content endpoint), or the server-rendered thumbnail for images and as the fallback. */
function Preview({ asset }: { asset: Asset }) {
  const { summary } = asset;
  const src = api.assetContentUrl(summary.id);
  if (summary.media === "model") {
    return (
      <div className="aspect-square border-b border-border">
        <ModelViewerIsland src={src} />
      </div>
    );
  }
  if (summary.media === "audio") {
    // Playable inline: waveform + transport, with the playhead driven by real progress (issues
    // #16, #14). Keyed by id so switching assets resets playback + the decoded waveform.
    return (
      <div className="border-b border-border">
        <AudioPlayer key={summary.id} src={src} />
      </div>
    );
  }
  if (summary.media === "image") {
    // Full-resolution content image in a zoom/pan viewer — 1:1 is pixel-accurate for inspecting
    // texture detail / tileability (issue #17). Keyed by id so switching assets resets the view.
    return (
      <div className="aspect-square border-b border-border">
        <ImageViewer key={summary.id} src={src} alt={summary.name} />
      </div>
    );
  }
  return (
    <div className="aspect-square border-b border-border">
      <Thumbnail asset={summary} size={64} />
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
        </div>

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
          <Field label="Origin" value={originLabel(summary.origin)} />
          <Field label="Source" value={sourceName} />
          <Field label="Path" value={asset.path} mono />
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

        {/* tags — auto-suggestions are actionable (accept/reject); the analysis pass shipped in phase 3 */}
        <Group title={`Tags (${asset.tags.length})`}>
          <TagList assetId={summary.id} tags={asset.tags} />
        </Group>

        {/* collections — this asset's manual memberships, with per-asset add/remove (issue #3) */}
        <CollectionsGroup asset={asset} />

        {/* find similar — cosine over embeddings, ranked in this asset's media space (phase 3) */}
        <SimilarSection asset={asset} />
      </div>
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
                  className="flex items-center justify-center hover:text-danger disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
                  title={`Remove from ${c.name}`}
                  aria-label={`Remove from ${c.name}`}
                  disabled={members.isPending}
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
          className="field mt-2"
          value=""
          disabled={members.isPending}
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

/** "Find similar" (tech-spec 05 §3): an opt-in ranked strip of neighbours by embedding cosine.
 *  Un-analyzed assets have no vector, so we offer to analyze first rather than query into the void.
 *  Each hit is selectable — clicking swaps the Inspector to that asset (and back/forward works, since
 *  selection lives in the URL). */
function SimilarSection({ asset }: { asset: Asset }) {
  const { patch } = useViewState();
  const [open, setOpen] = useState(false);
  const analyzed = asset.timestamps.analyzed != null;
  const analyze = useAnalyze();
  const similar = useSimilar(asset.summary.id, open && analyzed);

  if (!analyzed) {
    return (
      <Group title="Similar">
        <p className="text-[11px] text-fg-dim italic">
          Analyze this asset to find visually similar ones.
        </p>
        <button
          className="btn mt-2 coarse:min-h-11"
          disabled={analyze.isPending}
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
        <button className="btn coarse:min-h-11" onClick={() => setOpen(true)}>
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

/** Auto-tag review (tech-spec 05 §1.4): a *suggested* tag shows accept/reject; a *confirmed* or
 *  *rejected* tag shows its state and lets the user flip the decision (the one endpoint supports
 *  both directions). User-authored tags are static — there is nothing to review. */
function TagList({ assetId, tags }: { assetId: AssetId; tags: TagRef[] }) {
  const review = useReviewSuggestion();
  if (tags.length === 0) {
    return (
      <p className="text-[11px] text-fg-dim italic">
        No tags yet — the analysis pass proposes auto-tags to accept or reject.
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
          onReview={(action) => review.mutate({ asset: assetId, tag: t.name, action })}
        />
      ))}
    </div>
  );
}

function TagChip({
  tag,
  busy,
  onReview,
}: {
  tag: TagRef;
  busy: boolean;
  onReview: (action: ReviewAction) => void;
}) {
  const auto = tag.source === "auto";
  const confidence =
    tag.confidence != null ? ` · ${Math.round(tag.confidence * 100)}%` : "";
  const title = `${tag.state} · ${tag.source}${confidence}`;

  // User tags (and any non-auto) are not reviewable — render a plain chip.
  if (!auto) {
    return (
      <span
        className="rounded bg-surface-2 px-1.5 py-0.5 text-[10px] text-fg-muted"
        title={title}
      >
        {tag.name}
      </span>
    );
  }

  const rejected = tag.state === "rejected";
  const confirmed = tag.state === "confirmed";
  return (
    <span
      className="inline-flex items-center gap-1 rounded border px-1.5 py-0.5 text-[10px]"
      style={{
        borderColor: confirmed
          ? "var(--color-lic-permissive)"
          : rejected
            ? "var(--color-border)"
            : "var(--color-accent)",
        color: confirmed
          ? "var(--color-lic-permissive)"
          : rejected
            ? "var(--color-fg-dim)"
            : "var(--color-accent)",
        background: "color-mix(in srgb, currentColor 10%, transparent)",
      }}
      title={title}
    >
      <span className={rejected ? "line-through" : ""}>{tag.name}</span>
      {/* Accept is offered unless already confirmed; reject unless already rejected. */}
      {!confirmed && (
        <button
          className="flex items-center justify-center hover:text-lic-permissive disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
          title={rejected ? "Accept tag" : "Accept suggestion"}
          aria-label={`Accept tag ${tag.name}`}
          disabled={busy}
          onClick={() => onReview("accept")}
        >
          <Check size={12} />
        </button>
      )}
      {!rejected && (
        <button
          className="flex items-center justify-center hover:text-danger disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
          title={confirmed ? "Reject tag" : "Reject suggestion"}
          aria-label={`Reject tag ${tag.name}`}
          disabled={busy}
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
  } else if (attrs.media === "model") {
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
  if (rows.length === 0) return null;
  return (
    <Group title="Media">
      {rows.map(([label, value]) => (
        <Field key={label} label={label} value={value} />
      ))}
    </Group>
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

function Field({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-baseline justify-between gap-2 text-[11px]">
      <span className="shrink-0 text-fg-dim">{label}</span>
      <span
        className={`min-w-0 truncate text-right text-fg-muted ${mono ? "font-mono text-[10px]" : ""}`}
        title={value}
      >
        {value}
      </span>
    </div>
  );
}
