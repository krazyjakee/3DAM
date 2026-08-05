// The Inspector's per-media metadata panel (issue #166, parent #96) — extracted from
// `Inspector.tsx`. Pure presentation over `asset.attributes`: no query, no mutation, no ownership
// rules, so it renders standalone from a fixture.
//
// The four media arms plus the two derived-signal blocks (image seamlessness, audio features) stay
// in one module because they are one decision — "what facts does *this* media type have?" — and the
// readouts share the 0–1 `FeatureBar`.

import { duration } from "@/lib/format";
import type { MediaAttributes } from "@/api/types";
import { Field, Group } from "./primitives";

export function MediaFacts({ attrs }: { attrs: MediaAttributes }) {
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
