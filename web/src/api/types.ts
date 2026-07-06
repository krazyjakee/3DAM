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
  codec?: string | null;
  container?: string | null;
}
export interface ImageAttributes {
  width: number | null;
  height: number | null;
  color_depth?: number | null;
  has_alpha: boolean | null;
  color_space: string | null;
}
export interface ModelAttributes {
  vertex_count: number | null;
  triangle_count: number | null;
  mesh_count: number | null;
  material_count?: number | null;
  texture_count?: number | null;
  has_rig?: boolean | null;
  has_animation?: boolean | null;
  has_uvs?: boolean | null;
}

export interface TagRef {
  name: string;
  state: string; // suggested | confirmed | rejected
  source: string; // auto | user
  confidence: number | null;
}

/** Accept promotes a suggested tag to confirmed; reject records a negative (mirrors
 *  `dam-api` `ReviewAction`). Backend route: POST /api/v1/suggestions/review. */
export type ReviewAction = "accept" | "reject";
export interface SuggestionReview {
  asset: AssetId;
  tag: string;
  action: ReviewAction;
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

// ── collections / smart folders (phase 4; PRODUCT_SPEC §3, §6.4) ─────────────

/** A **manual** collection holds a hand-curated member list; a **smart** folder holds a saved
 *  query and resolves live. Mirrors `dam-api` `CollectionKind`. */
export type CollectionKind = "manual" | "smart";

export interface Collection {
  id: CollectionId;
  name: string;
  kind: CollectionKind;
  /** The saved query backing a smart folder; absent for a manual collection. */
  query?: QueryRequest | null;
  /** Exact member count (manual) or current match count (smart) when computed, else null. */
  count: number | null;
  created_at: number;
  updated_at: number;
}
/** Create a collection. A smart folder must carry a `query`; a manual collection ignores it. */
export interface NewCollection {
  name: string;
  kind?: CollectionKind;
  query?: QueryRequest | null;
}
/** Patch: rename and/or (smart folders) replace the saved query. Absent fields are unchanged. */
export interface UpdateCollection {
  name?: string | null;
  query?: QueryRequest | null;
}
/** Add/remove members of a **manual** collection (smart membership is query-driven). */
export interface CollectionMembers {
  add?: AssetId[];
  remove?: AssetId[];
}

// ── duplicates (phase 3: exact content-hash + near pHash/embedding) ──────────

/** Which duplicate tier to surface (mirrors `dam-api` `DupKind`): byte-identical `exact`, or
 *  perceptually-close `near`. */
export type DupKind = "exact" | "near";

/** Request the duplicate groups for review, optionally scoped to one media type. */
export interface DupRequest {
  kind?: DupKind;
  media?: MediaType;
  limit?: number;
}

/** A cluster of duplicates for the review view. 3DAM only *groups* — nothing is auto-deleted. */
export interface DupGroup {
  kind: DupKind;
  media: MediaType;
  members: AssetSummary[];
  /** The pairwise signal that linked the group — the explanation (content hash / pHash distance). */
  signal: string;
  /** A suggested "keep" (highest resolution / largest); the user disposes. */
  suggested_keep: AssetId;
}

// ── find similar (phase 3: cosine over embeddings) ──────────────────────────

/** "More like this" by asset id (mirrors `dam-api` `SimilarRequest`). Scoped to the query asset's
 *  media space; composes the same faceted `filters` as text search. Route: POST /api/v1/similar. */
export interface SimilarRequest {
  asset: AssetId;
  k?: number; // neighbours to return (post self-drop); server default 24
  filters?: Filter[];
}
/** One neighbour: the asset plus its cosine score and the embedding space it was ranked in. */
export interface SimilarHit {
  asset: AssetSummary;
  score: number; // cosine similarity in [0,1] (1 = identical direction)
  space: string; // the EmbeddingSpace id the ranking happened in
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
  // Connection auth for network sources (SFTP/SMB); ignored for local. Values here override any
  // userinfo parsed from the URI (mirrors dam-api SourceOptions).
  username?: string;
  password?: string;
  private_key?: string;
  passphrase?: string;
  domain?: string;
  port?: number;
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
export interface AnalyzeRequest {
  assets?: AssetId[]; // empty → every asset due for (re)analysis
  force?: boolean; // re-run even if already up to date
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
