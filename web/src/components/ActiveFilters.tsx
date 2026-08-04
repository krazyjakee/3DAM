import { X } from "lucide-react";
import { useCollections, useSources } from "@/api/queries";
import type { FacetField, Filter, FilterValue, MediaType } from "@/api/types";
import { bytes, licenseLabel, mediaLabel } from "@/lib/format";
import { usageRightPhrase } from "@/lib/license";
import { useViewState } from "@/lib/view-state";

const FIELD_LABELS: Record<FacetField, string> = {
  media_type: "Media",
  format: "Format",
  source: "Source",
  tag: "Tag",
  size_bytes: "Size",
  license: "License",
  usage_right: "Usage right",
  favorite: "Favourite",
  path: "Folder tree",
  folder: "Folder",
  width: "Width",
  height: "Height",
  color_depth: "Colour depth",
  has_alpha: "Alpha channel",
  color_space: "Colour space",
  image_class: "Image type",
  tileability: "Tileability",
  tile_class: "Tiling",
  bpm: "BPM",
  duration: "Duration",
  sample_rate: "Sample rate",
  bit_depth: "Bit depth",
  channels: "Channels",
  musical_key: "Key",
  loudness: "Loudness",
  brightness: "Brightness",
  harmonicity: "Harmonicity",
  audio_class: "Audio type",
  codec: "Codec",
  container: "Container",
  tri_count: "Triangles",
  vertex_count: "Vertices",
  mesh_count: "Meshes",
  material_count: "Materials",
  texture_count: "Textures",
  dependency_bytes: "Dependency size",
  has_rig: "Rigged",
  has_animation: "Animated",
  has_uv: "UV mapped",
  model_class: "Complexity",
  fps: "Frame rate",
  bitrate: "Bitrate",
  has_audio: "Has audio",
  video_class: "Video length",
  page_count: "Pages",
  word_count: "Words",
  author: "Author",
  document_class: "Document kind",
};

const VALUE_LABELS: Record<string, string> = {
  one_shot: "One-shot",
  sfx: "SFX",
  prop_lowpoly: "Low-poly",
  prop_highpoly: "High-poly",
  non_tiling: "Non-tiling",
};

const SEARCH_MODE_LABELS = {
  lexical: "Keywords",
  hybrid: "Keywords + similar",
  semantic: "Most similar",
} as const;

function humanNumber(field: FacetField, value: number, media: MediaType | null): string {
  if (field === "duration" && media === "audio") return `${(value / 1000).toLocaleString()} s`;
  if (field === "duration") return `${value.toLocaleString()} ms`;
  if (field === "width" || field === "height") return `${value.toLocaleString()} px`;
  if (field === "bpm") return `${value.toLocaleString()} BPM`;
  if (field === "sample_rate") return `${(value / 1000).toLocaleString()} kHz`;
  if (field === "bit_depth") return `${value.toLocaleString()}-bit`;
  if (field === "channels" && value === 1) return "Mono";
  if (field === "channels" && value === 2) return "Stereo";
  if (field === "loudness") return `${value.toLocaleString()} LUFS`;
  if (field === "fps") return `${value.toLocaleString()} fps`;
  if (field === "bitrate") return `${value.toLocaleString()} bps`;
  if (field === "size_bytes" || field === "dependency_bytes") return bytes(value);
  return value.toLocaleString();
}

function humanValue(value: FilterValue, field: FacetField, media: MediaType | null): string {
  if ("str" in value) {
    if (field === "tag" || field === "source" || field === "author") return value.str;
    if (field === "musical_key") return value.str.toUpperCase();
    return (
      VALUE_LABELS[value.str] ??
      value.str.replaceAll("_", " ").replace(/^./, (first) => first.toUpperCase())
    );
  }
  if ("num" in value) return humanNumber(field, value.num, media);
  if ("bool" in value) return value.bool ? "Yes" : "No";
  if ("range" in value)
    return `${humanNumber(field, value.range[0], media)}–${humanNumber(field, value.range[1], media)}`;
  return value.list.map((item) => humanValue(item, field, media)).join(", ");
}

function describeFilter(filter: Filter, media: MediaType | null): string {
  // `usage_right` carries its direction in the *op* — `eq` asserts the right is granted, `ne` that
  // it is known to be denied. Both must be spelled out: rendering the value alone would give
  // "commercial eq" and "commercial ne" an identical chip, which on a safety field is worse than
  // showing nothing. The generic path below can't do it, because "Usage right is not commercial"
  // reads as a category exclusion rather than a claim about a permission.
  if (filter.field === "usage_right" && "str" in filter.value) {
    return (
      usageRightPhrase(filter.value.str, filter.op, "chip") ??
      `Usage right: ${filter.value.str} (${filter.op})`
    );
  }
  const field = FIELD_LABELS[filter.field];
  const value = humanValue(filter.value, filter.field, media);
  switch (filter.op) {
    case "eq":
      return `${field}: ${value}`;
    case "ne":
      return `${field} is not ${value}`;
    case "lt":
      return `${field} < ${value}`;
    case "lte":
      return `${field} ≤ ${value}`;
    case "gt":
      return `${field} > ${value}`;
    case "gte":
      return `${field} ≥ ${value}`;
    case "range":
      return `${field}: ${value}`;
    case "in":
      return `${field}: ${value}`;
    case "contains":
      return `${field} contains ${value}`;
    case "exists":
      return `${field}: ${value}`;
  }
}

function FilterChip({ label, onRemove }: { label: string; onRemove: () => void }) {
  return (
    <li className="inline-flex min-h-7 max-w-full items-center rounded-full border border-accent/60 bg-accent-muted text-[11px] text-accent coarse:min-h-11">
      <span className="min-w-0 break-words py-1 pl-2">{label}</span>
      <button
        type="button"
        className="flex min-h-7 min-w-7 shrink-0 items-center justify-center rounded-full hover:bg-accent/15 hover:text-fg coarse:min-h-11 coarse:min-w-11"
        aria-label={`Remove filter: ${label}`}
        title={`Remove ${label}`}
        onClick={onRemove}
      >
        <X size={12} aria-hidden />
      </button>
    </li>
  );
}

/** Always-visible summary of every URL-backed constraint on the asset browser. */
export function ActiveFilters({ onClearAll }: { onClearAll: () => void }) {
  const { state, patch } = useViewState();
  const sources = useSources();
  const collections = useCollections();
  const chips: { key: string; label: string; remove: () => void }[] = [];

  if (state.q) {
    chips.push({
      key: "search",
      label: `Search (${SEARCH_MODE_LABELS[state.mode]}): “${state.q}”`,
      remove: () => patch({ q: "", mode: "lexical" }),
    });
  }
  if (state.media) {
    chips.push({
      key: "media",
      label: `Media: ${mediaLabel[state.media as MediaType]}`,
      remove: () => patch({ media: null }),
    });
  }
  if (state.source) {
    const name = sources.data?.find((source) => source.id === state.source)?.name ?? state.source;
    chips.push({
      key: "source",
      label: `Source: ${name}`,
      remove: () => patch({ source: null, path: null, subfolders: true }),
    });
  }
  if (state.path) {
    chips.push({
      key: "folder",
      label: `Folder: ${state.path} (${
        state.subfolders ? "including subfolders" : "this folder only"
      })`,
      remove: () => patch({ path: null, subfolders: true }),
    });
  }
  if (state.license) {
    const label =
      state.license in licenseLabel
        ? licenseLabel[state.license as keyof typeof licenseLabel]
        : state.license;
    chips.push({
      key: "license",
      label: `License: ${label}`,
      remove: () => patch({ license: null }),
    });
  }
  if (state.tag) {
    chips.push({
      key: "tag",
      label: `Tag: ${state.tag}`,
      remove: () => patch({ tag: null }),
    });
  }
  if (state.fav) {
    chips.push({
      key: "favourite",
      label: "Favourites only",
      remove: () => patch({ fav: false }),
    });
  }
  if (state.collection) {
    const name =
      collections.data?.find((collection) => collection.id === state.collection)?.name ??
      state.collection;
    chips.push({
      key: "collection",
      label: `Collection: ${name}`,
      remove: () => patch({ collection: null }),
    });
  }
  state.adv.forEach((filter, index) => {
    chips.push({
      key: `advanced-${index}`,
      label: describeFilter(filter, state.media),
      remove: () => patch({ adv: state.adv.filter((_, candidate) => candidate !== index) }),
    });
  });

  if (chips.length === 0) return null;

  return (
    <section
      className="flex flex-wrap items-center gap-1.5 border-b border-border bg-surface px-3 py-1.5"
      aria-label="Active filters"
    >
      <span className="mr-0.5 text-[10px] font-semibold tracking-wider whitespace-nowrap text-fg-dim uppercase">
        Active filters
      </span>
      <ul className="flex min-w-0 flex-1 flex-wrap gap-1.5">
        {chips.map((chip) => (
          <FilterChip key={chip.key} label={chip.label} onRemove={chip.remove} />
        ))}
      </ul>
      <button
        type="button"
        className="min-h-7 shrink-0 rounded px-2 text-[11px] font-medium text-fg-muted hover:bg-surface-2 hover:text-accent coarse:min-h-11"
        onClick={onClearAll}
        title="Clear filters and selection, then sort by name"
      >
        Clear all filters
      </button>
    </section>
  );
}
