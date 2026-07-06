import type React from "react";
import { PanelRightClose } from "lucide-react";
import { useAsset, useSources } from "@/api/queries";
import { api } from "@/api/client";
import type { Asset, MediaAttributes } from "@/api/types";
import { bytes, duration, mediaLabel, originLabel, relTime } from "@/lib/format";
import { useViewState } from "@/lib/view-state";
import { ModelViewerIsland } from "@/islands/ModelViewerIsland";
import { WaveformIsland } from "@/islands/WaveformIsland";
import { LicenseBadge } from "./LicenseBadge";
import { Thumbnail } from "./Thumbnail";
import { MediaIcon } from "./MediaIcon";
import { Drawer } from "./Drawer";

/** Inspector — a persistent right rail on `lg`, an overlay drawer below it (responsive + touch pass). Both
 *  render the same {@link InspectorPanel}, so a 360px phone and a wide desktop show identical detail. */
export function Inspector() {
  const { state, patch } = useViewState();
  const close = () => patch({ selected: null });

  return (
    <>
      <aside className="hidden w-[300px] shrink-0 flex-col border-l border-border bg-surface lg:flex">
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
    return (
      <div className="h-24 border-b border-border">
        <WaveformIsland src={src} />
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

        {/* tags */}
        <Group title={`Tags (${asset.tags.length})`}>
          {asset.tags.length === 0 ? (
            <p className="text-[11px] text-fg-dim italic">
              No tags yet — auto-tagging arrives with the analysis pipeline.
            </p>
          ) : (
            <div className="flex flex-wrap gap-1">
              {asset.tags.map((t) => (
                <span
                  key={t.name}
                  className="rounded bg-surface-2 px-1.5 py-0.5 text-[10px] text-fg-muted"
                  title={`${t.state} · ${t.source}`}
                >
                  {t.name}
                </span>
              ))}
            </div>
          )}
        </Group>
      </div>
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
    if (attrs.channels != null) rows.push(["Channels", String(attrs.channels)]);
  } else if (attrs.media === "image") {
    if (attrs.width != null && attrs.height != null)
      rows.push(["Dimensions", `${attrs.width} × ${attrs.height}`]);
    if (attrs.has_alpha != null) rows.push(["Alpha", attrs.has_alpha ? "yes" : "no"]);
    if (attrs.color_space) rows.push(["Color space", attrs.color_space]);
  } else if (attrs.media === "model") {
    if (attrs.vertex_count != null) rows.push(["Vertices", attrs.vertex_count.toLocaleString()]);
    if (attrs.triangle_count != null)
      rows.push(["Triangles", attrs.triangle_count.toLocaleString()]);
    if (attrs.mesh_count != null) rows.push(["Meshes", String(attrs.mesh_count)]);
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
