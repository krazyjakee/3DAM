// Typed mirror of the `dam-api` DTOs (tech-spec 03 §3–§7). These match the serde wire
// shapes exactly — ids are `#[serde(transparent)]` UUID strings, enums are snake/lowercase
// tagged unions. Keep in lock-step with crates/3dam-api/src/{dto,event,page,error}.rs.

export type AssetId = string;
export type SourceId = string;
export type JobId = string;
export type CollectionId = string;
export type ContentHash = string;

export type MediaType = "audio" | "image" | "model";
export type LicenseStatus = "permissive" | "attribution" | "restricted" | "unknown";

export type Origin = "local" | { peer: string };

export interface LicenseBadge {
  id: string | null;
  status: LicenseStatus;
}

export interface License {
  id: string | null;
  status: LicenseStatus;
  commercial: boolean | null;
  modify: boolean | null;
  redistribute: boolean | null;
  attribution: boolean | null;
  holder: string | null;
  credit: string | null;
  url: string | null;
  provenance: string;
}

export interface AssetSummary {
  id: AssetId;
  name: string;
  media: MediaType;
  format: string;
  size: number;
  license: LicenseBadge;
  top_tags: string[];
  origin: Origin;
  key_attrs: Record<string, string>;
}

export interface AssetTimes {
  created: number | null;
  modified: number | null;
  scanned: number;
  analyzed: number | null;
}

export type MediaAttributes =
  | ({ media: "audio" } & AudioAttributes)
  | ({ media: "image" } & ImageAttributes)
  | ({ media: "model" } & ModelAttributes)
  | { media: "none" };

export interface AudioAttributes {
  duration_ms: number | null;
  sample_rate: number | null;
  bit_depth: number | null;
  channels: number | null;
}
export interface ImageAttributes {
  width: number | null;
  height: number | null;
  has_alpha: boolean | null;
  color_space: string | null;
}
export interface ModelAttributes {
  vertex_count: number | null;
  triangle_count: number | null;
  mesh_count: number | null;
}

export interface TagRef {
  name: string;
  state: string; // suggested | confirmed | rejected
  source: string; // auto | user
  confidence: number | null;
}

export interface Asset {
  summary: AssetSummary;
  hash: ContentHash | null;
  source_id: SourceId;
  path: string;
  timestamps: AssetTimes;
  attributes: MediaAttributes;
  license: License;
  tags: TagRef[];
  collections: CollectionId[];
}

// ── query ──────────────────────────────────────────────────────────────────

export type FacetField =
  | "media_type"
  | "format"
  | "source"
  | "tag"
  | "size_bytes"
  | "license"
  | "usage_right"
  | "width"
  | "height"
  | "bpm"
  | "tri_count";

export type FilterOp =
  | "eq"
  | "ne"
  | "lt"
  | "lte"
  | "gt"
  | "gte"
  | "in"
  | "range"
  | "contains"
  | "exists";

export type FilterValue =
  | { str: string }
  | { num: number }
  | { bool: boolean }
  | { range: [number, number] }
  | { list: FilterValue[] };

export interface Filter {
  field: FacetField;
  op: FilterOp;
  value: FilterValue;
}

export type SortField = "name" | "size" | "scanned" | "relevance";
export type SortDir = "asc" | "desc";
export interface Sort {
  field: SortField;
  dir: SortDir;
}

export type QueryScope = "local" | "federated" | { sources: SourceId[] };

export interface PageParams {
  after?: string | null;
  limit: number;
}

export interface QueryRequest {
  text?: string | null;
  filters?: Filter[];
  sort?: Sort;
  scope?: QueryScope;
  page?: PageParams;
  include_facets?: boolean;
}

export interface ItemWarning {
  subject: string;
  code: string;
  message: string;
}
export interface PartialStatus {
  complete: boolean;
  warnings?: ItemWarning[];
}
export interface Page<T> {
  items: T[];
  cursor: string | null;
  total: number | null;
  partial: PartialStatus;
}

// ── sources ────────────────────────────────────────────────────────────────

export type SourceKind = "local_fs" | "sftp" | "smb" | "federated";
export type SourceState =
  | "online"
  | "offline"
  | "scanning"
  | { error: string };

export interface SourceStats {
  asset_count: number;
  last_scanned_at: number | null;
  last_error: string | null;
}
export interface SourceInfo {
  id: SourceId;
  kind: SourceKind;
  name: string;
  uri: string;
  state: SourceState;
  stats: SourceStats;
  watch: boolean;
}
export interface SourceOptions {
  watch?: boolean;
  include?: string[];
  exclude?: string[];
}
export interface AddSource {
  kind: SourceKind;
  uri: string;
  name?: string | null;
  options?: SourceOptions;
}
export interface RemoveSource {
  keep_metadata?: boolean;
}

// ── jobs ───────────────────────────────────────────────────────────────────

export type ScanMode = "full" | "delta";
export interface ScanRequest {
  sources?: SourceId[];
  mode?: ScanMode;
}
export type JobKind = "scan" | "analyze" | "convert" | "export";
export type JobState =
  | "queued"
  | "running"
  | "paused"
  | "done"
  | "failed"
  | "cancelled";
export interface Progress {
  done: number;
  total: number | null;
  current: string | null;
}
export interface JobStatus {
  id: JobId;
  kind: JobKind;
  state: JobState;
  progress: Progress;
  error: string | null;
}
export interface JobListRequest {
  kinds?: JobKind[];
  state?: JobState | null;
  page?: PageParams;
}

// ── stats ──────────────────────────────────────────────────────────────────

export interface LibraryStats {
  total: number;
  by_media: Record<string, number>;
  by_source: Record<string, number>;
  unanalyzed: number;
  sources: number;
}

// ── live events (WebSocket) ─────────────────────────────────────────────────

export type ChangeKind = "reanalyzed" | "retagged" | "license_set" | "metadata";
export type LibraryEvent =
  | ({ type: "asset_added" } & AssetSummary)
  | { type: "asset_changed"; id: AssetId; kind: ChangeKind }
  | { type: "asset_removed"; 0: AssetId } // AssetRemoved(AssetId) — newtype variant
  | { type: "source_state"; id: SourceId; state: SourceState }
  | ({ type: "job_progress" } & JobStatus);

// ── error envelope ──────────────────────────────────────────────────────────

export interface ErrorBody {
  code: string;
  message: string;
  detail?: unknown;
}
