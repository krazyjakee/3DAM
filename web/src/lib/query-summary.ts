import type { Filter, FilterOp, FilterValue, QueryRequest, SourceInfo } from "@/api/types";
// Relative *and* extensioned, not `@/lib/license`: this module is exercised by the node
// `--experimental-strip-types` unit runner, which erases type-only `@` imports but can neither
// resolve a *value* one nor guess an extension the way the bundler does.
import { usageRightPhrase } from "./license.ts";

const FIELD_LABELS = {
  media_type: "Media type",
  format: "Format",
  source: "Source",
  tag: "Tag",
  size_bytes: "File size",
  license: "License",
  usage_right: "Usage right",
  favorite: "Favourite",
  path: "Folder subtree",
  folder: "Folder",
  width: "Width",
  height: "Height",
  color_depth: "Colour depth",
  has_alpha: "Alpha channel",
  color_space: "Colour space",
  image_class: "Image type",
  tileability: "Tileability",
  tile_class: "Tile type",
  bpm: "Tempo",
  duration: "Duration",
  sample_rate: "Sample rate",
  bit_depth: "Bit depth",
  channels: "Channels",
  musical_key: "Musical key",
  loudness: "Loudness",
  brightness: "Brightness",
  harmonicity: "Harmonicity",
  audio_class: "Audio type",
  codec: "Codec",
  container: "Container",
  tri_count: "Triangle count",
  vertex_count: "Vertex count",
  mesh_count: "Mesh count",
  material_count: "Material count",
  texture_count: "Texture count",
  dependency_bytes: "Dependency size",
  has_rig: "Rigged",
  has_animation: "Animated",
  has_uv: "UV mapped",
  model_class: "Model type",
  fps: "Frame rate",
  bitrate: "Bitrate",
  has_audio: "Has audio",
  video_class: "Video type",
  page_count: "Page count",
  word_count: "Word count",
  author: "Author",
  document_class: "Document type",
} as const;

const LICENSE_STATUS_LABELS: Record<string, string> = {
  permissive: "Permissive (commercial use allowed, no attribution required)",
  attribution: "Attribution required",
  restricted: "Restricted",
  unknown: "Unknown / unverified",
};

const OP_LABELS: Record<FilterOp, string> = {
  eq: "is",
  ne: "is not",
  lt: "is less than",
  lte: "is at most",
  gt: "is greater than",
  gte: "is at least",
  in: "is one of",
  range: "is between",
  contains: "contains",
  exists: "exists",
};

export interface QuerySummary {
  lines: string[];
  warnings: string[];
}

/** Strip transport-only paging/count switches while retaining every matching and routing field. */
export function savedQuery(query: QueryRequest): QueryRequest {
  const saved = structuredClone(query);
  delete saved.page;
  delete saved.include_facets;
  delete saved.include_total;
  saved.filters ??= [];
  return saved;
}

function valueLabel(value: FilterValue | unknown): string | null {
  if (!value || typeof value !== "object") return null;
  if ("str" in value && typeof value.str === "string") return value.str || "(empty)";
  if ("num" in value && typeof value.num === "number")
    return Number.isFinite(value.num) ? value.num.toLocaleString() : null;
  if ("bool" in value && typeof value.bool === "boolean") return value.bool ? "yes" : "no";
  if ("range" in value && Array.isArray(value.range))
    return value.range.length === 2 && value.range.every((part) => typeof part === "number" && Number.isFinite(part))
      ? `${Number(value.range[0]).toLocaleString()} and ${Number(value.range[1]).toLocaleString()}`
      : null;
  if ("list" in value && Array.isArray(value.list)) {
    const labels = value.list.map(valueLabel);
    return labels.every((label): label is string => label !== null) ? labels.join(", ") : null;
  }
  return null;
}

function filterSummary(
  filter: Filter,
  sourceNames: ReadonlyMap<string, string>,
  validateSources: boolean,
): { line?: string; warning?: string } {
  const field = (filter as { field?: string }).field;
  const op = (filter as { op?: string }).op;
  const label = field && FIELD_LABELS[field as keyof typeof FIELD_LABELS];
  const opLabel = op && OP_LABELS[op as FilterOp];
  const value = filter && typeof filter === "object" ? valueLabel(filter.value) : null;
  if (!field || !label || !opLabel || value === null) {
    return { warning: `A saved filter (${field ?? "unknown"}) is not supported by this version.` };
  }
  // A usage right is a claim about a permission, not a category, and its *op* carries the direction
  // (dam-store `helpers.rs`): `eq` → granted, `ne` → known to be denied. A smart folder saved years
  // ago is read back through this preview, so both directions have to survive the round trip
  // legibly — "Usage right is not commercial" would be actively misleading (issue #106).
  if (field === "usage_right" && "str" in filter.value && op) {
    const phrase = usageRightPhrase(filter.value.str, op, "summary");
    if (phrase) return { line: phrase };
  }
  if (field === "license" && "str" in filter.value) {
    const status = LICENSE_STATUS_LABELS[filter.value.str];
    if (status) return { line: `${label} ${opLabel} ${status}` };
  }
  if (field === "source" && "str" in filter.value) {
    const sourceName = sourceNames.get(filter.value.str);
    if (!sourceName && validateSources) {
      return {
        line: `${label} ${opLabel} ${filter.value.str}`,
        warning: "A source used by this search is no longer available.",
      };
    }
    return { line: `${label} ${opLabel} ${sourceName ?? filter.value.str}` };
  }
  return { line: `${label} ${opLabel} ${value}` };
}

/** Human-language preview plus compatibility warnings for a current or previously saved query. */
export function summarizeQuery(query: QueryRequest | null | undefined, sources?: SourceInfo[]): QuerySummary {
  if (!query) {
    return {
      lines: [],
      warnings: ["This smart folder’s saved query cannot be read by this version."],
    };
  }
  const lines: string[] = [];
  const warnings: string[] = [];
  const sourceNames = new Map((sources ?? []).map((source) => [source.id, source.name]));
  if (query.text?.trim()) {
    const mode = query.mode === "semantic" ? "Semantic search" : query.mode === "hybrid" ? "Keywords + similar" : "Keywords";
    lines.push(`${mode}: “${query.text.trim()}”`);
  }
  for (const filter of query.filters ?? []) {
    const summary = filterSummary(filter, sourceNames, sources !== undefined);
    if (summary.line) lines.push(summary.line);
    if (summary.warning && !warnings.includes(summary.warning)) warnings.push(summary.warning);
  }
  if (lines.length === 0) lines.push("All assets from local and federated sources");
  const sort = query.sort;
  if (sort && (sort.field !== "name" || sort.dir !== "asc")) {
    const field = sort.field === "relevance" ? "best match" : sort.field;
    lines.push(`Sorted by ${field} (${sort.dir === "asc" ? "ascending" : "descending"})`);
  }
  return { lines, warnings };
}
