// Typed mirror of the `dam-api` DTOs (tech-spec 03 §3–§7). These match the serde wire
// shapes exactly — ids are `#[serde(transparent)]` UUID strings, enums are snake/lowercase
// tagged unions. Keep in lock-step with crates/3dam-api/src/{dto,event,page,error}.rs.

export type AssetId = string;
export type SourceId = string;
export type JobId = string;
export type CollectionId = string;
export type ContentHash = string;

/** Deep media types plus the shallower `video`/`document` (PRODUCT_SPEC §9 phase 2b). */
export type MediaType = "audio" | "image" | "model" | "video" | "document";
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
  /** User-flagged favourite (issue #63); persisted in the asset `flags` bitset server-side. */
  favorite: boolean;
  /** Which source this row came from. Present so the server can match an `asset_added` event
   *  against a share-based visibility ceiling before sending it (issue #42). `null` for a payload
   *  from a server predating the field. */
  source_id: SourceId | null;
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
  | ({ media: "video" } & VideoAttributes)
  | ({ media: "document" } & DocumentAttributes)
  | { media: "none" };

export interface AudioAttributes {
  duration_ms: number | null;
  sample_rate: number | null;
  bit_depth: number | null;
  channels: number | null;
  codec?: string | null;
  container?: string | null;
  /** Auto-category guess (`one_shot` | `loop` | `music` | `sfx`). */
  class?: string | null;
  // ── derived by the analysis pass (null until `analyze` runs), issue #61 ──
  /** Integrated loudness in dBFS (an RMS approximation of LUFS). Negative. */
  loudness_lufs?: number | null;
  /** Spectral-centroid brightness, normalised 0–1. */
  brightness?: number | null;
  /** Harmonic-vs-noise ratio, normalised 0–1. */
  harmonicity?: number | null;
  /** Normalised (0–1) waveform peak buckets computed server-side (issue #73); draw the inspector
   *  waveform from these instead of decoding the audio. Null until analysed. */
  peaks?: number[] | null;
}

/** Prefetch hint (issue #72): warm these assets' thumbnails/previews ahead of render. */
export interface PrefetchRequest {
  assets: AssetId[];
  edge?: number | null;
  /** Set only by a server relaying a federation hint; prevents relay cycles. */
  relay?: boolean;
}
export interface ImageAttributes {
  width: number | null;
  height: number | null;
  color_depth?: number | null;
  has_alpha: boolean | null;
  color_space: string | null;
  // ── GPU texture containers (DDS/KTX2, issue #49); null for ordinary rasters ──
  /** The container's own pixel format — `BC7_UNORM`, `R8G8B8A8_SRGB`, … For a texture this is what
   *  distinguishes one file from another: dimensions alone don't say whether a 4 MB `.dds` is a BC5
   *  normal map or a BC7 albedo. */
  texture_format?: string | null;
  /** Mip levels stored in the file (1 = just the base image). */
  mip_levels?: number | null;
  // ── derived by the analysis pass (null until `analyze` runs) ──
  /** Perceptual (dHash) hash, hex-encoded. */
  phash?: string | null;
  /** Edge-continuity tileability score in [0,1]. */
  tileability?: number | null;
  /** Detected internal repeat period in source pixels, if the image already tiles. */
  repeat_period?: number | null;
  /** `seamless` | `tiled` | `non_tiling`. */
  tile_class?: string | null;
  /** Dominant colours as `#rrggbb`, most-prominent first. */
  dominant_colors?: string[];
  /** Auto-category guess (`texture` | `photo` | `sprite` | …). */
  class?: string | null;
}
export interface ModelAttributes {
  vertex_count: number | null;
  triangle_count: number | null;
  mesh_count: number | null;
  material_count?: number | null;
  texture_count?: number | null;
  /** On-disk bytes of external companion files (textures, glTF .bin). Folded into the asset size. */
  dependency_bytes?: number | null;
  has_rig?: boolean | null;
  has_animation?: boolean | null;
  has_uvs?: boolean | null;
  /** Auto-category guess (`prop` | `character` | `environment` | …). */
  class?: string | null;
}

/**
 * Cheap-tier video facts from a discovered `ffprobe` (ADR 0015). Every field is nullable because
 * the whole struct is empty-but-valid when no prober is installed on the server — render that as
 * "unknown", not as a broken asset.
 */
export interface VideoAttributes {
  duration_ms: number | null;
  width: number | null;
  height: number | null;
  fps?: number | null;
  codec?: string | null;
  container?: string | null;
  bitrate?: number | null;
  has_audio?: boolean | null;
  class?: string | null;
}

/** Cheap-tier document facts. `excerpt` is a short leading extract, used for the tile card. */
export interface DocumentAttributes {
  page_count: number | null;
  word_count: number | null;
  /** Title from the container's metadata (PDF info dict, DOCX core props, leading `#`) — not the filename. */
  title?: string | null;
  author?: string | null;
  encoding?: string | null;
  excerpt?: string | null;
  class?: string | null;
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

export interface TagEditRequest {
  assets?: AssetId[];
  collection?: CollectionId;
  query?: QueryRequest;
  add?: string[];
  remove?: string[];
  dry_run?: boolean;
}

export interface TagEditResult {
  matched: number;
  changed: number;
  additions: number;
  removals: number;
  warnings: ItemWarning[];
}

export interface TagInfo {
  name: string;
  count: number;
  manual: boolean;
}

/** Flag/unflag an asset as a favourite (issue #63). Backend route: POST /api/v1/assets/favorite. */
export interface FavoriteRequest {
  asset: AssetId;
  favorite: boolean;
}

/** A user's free-text annotation on an asset (issue #81) — the "why" no extractor can infer.
 *  Backend routes: GET/PUT /api/v1/assets/{id}/note. `body` is verbatim what the user typed. */
export interface Note {
  body: string;
  updated_at: number;
  updated_by: string | null;
}

/** Who wrote a discussion message (issue #82). `display` is resolved server-side across the
 *  database boundary and is `null` once the account is gone — the message survives its author. */
export interface CommentAuthor {
  id: string;
  display: string | null;
}

/** One message in an asset's discussion thread. `deleted_at` marks a tombstone: the row survives so
 *  replies keep their parent, with `body` blanked. Backend: GET/POST /api/v1/assets/{id}/comments,
 *  PUT/DELETE /api/v1/comments/{id} — all 404 while the `user_accounts` flag is off. */
export interface Comment {
  id: string;
  asset: AssetId;
  author: CommentAuthor;
  body: string;
  created_at: number;
  edited_at: number | null;
  deleted_at: number | null;
  reply_to: string | null;
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
  /** `null` when the asset has no note — an empty body and no note are the same state. */
  note: Note | null;
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
  | "favorite"
  /** Folder subtree — this prefix and everything below it (issue #66). */
  | "path"
  /** One folder exactly — its own files, excluding deeper subfolders (issue #66). */
  | "folder"
  // image (image_attr)
  | "width"
  | "height"
  | "color_depth"
  | "has_alpha"
  | "color_space"
  | "image_class"
  | "tileability"
  | "tile_class"
  // audio (audio_attr)
  | "bpm"
  | "duration"
  | "sample_rate"
  | "bit_depth"
  | "channels"
  | "musical_key"
  | "loudness"
  | "brightness"
  | "harmonicity"
  | "audio_class"
  | "codec"
  | "container"
  // model (model_attr)
  | "tri_count"
  | "vertex_count"
  | "mesh_count"
  | "material_count"
  | "texture_count"
  | "dependency_bytes"
  | "has_rig"
  | "has_animation"
  | "has_uv"
  | "model_class"
  // video (video_attr) — width/height/duration above also match video
  | "fps"
  | "bitrate"
  | "has_audio"
  | "video_class"
  // document (document_attr)
  | "page_count"
  | "word_count"
  | "author"
  | "document_class";

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

export interface PageParams {
  after?: string | null;
  limit: number;
}

/** Text-search strategy (semantic-search M5). Mirrors `dam-api` `SearchMode`. `lexical` is the
 *  FTS + synonym path; `hybrid` also pulls in embedding neighbours of the matches; `semantic` ranks
 *  by that neighbourhood. Omitting it (or `lexical`) preserves the classic behaviour. */
export type SearchMode = "lexical" | "hybrid" | "semantic";

export interface QueryRequest {
  text?: string | null;
  filters?: Filter[];
  sort?: Sort;
  page?: PageParams;
  /** Ask the server to include facet counts with the query response. */
  include_facets?: boolean;
  mode?: SearchMode;
  /** Federation (phase 6): skip the peer fan-out and answer from this instance's catalog only. */
  local_only?: boolean;
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

// ── convert (phase 2: image + audio, source-safe, atomic) ───────────────────

/** On a planned-output collision (mirrors `dam-api` `CollisionRule`): fail the item (default),
 *  disambiguate with a `-N` suffix, skip (leave existing), or overwrite a non-source file. */
export type CollisionRule = "fail" | "suffix" | "skip" | "overwrite";

/** The media-typed encode target. One target per request; items of the other media type fail as
 *  `unsupported` (fail-soft). Matches `dam-api` `ConvertTarget` (`#[serde(tag = "media")]`). */
export type ConvertTarget =
  | { media: "image"; format: string; max_edge?: number | null; quality?: number | null }
  | { media: "audio"; format: string }
  /** 3D container transcode (issue #49). `glb` only for now — a self-contained glTF binary with
   *  textures embedded. `gltf`/`obj` emit sidecar files the convert pipeline cannot yet write as
   *  one output, so the server refuses them by name.
   *
   *  `optimize` opts into mesh optimisation: redundant materials and meshes merge, degenerate faces
   *  go, and the vertices a merge duplicates are re-joined — fewer draw calls for the same picture.
   *  It collapses the node graph, so it is opt-in rather than the default. */
  | { media: "model"; format: string; optimize?: boolean };

export interface ConvertRequest {
  inputs: AssetId[];
  target: ConvertTarget;
  /** Destination directory — never a registered source tree (the backend enforces this, §5.1). */
  output_dir: string;
  /** Plan only: resolve outputs + estimate, write nothing. */
  dry_run?: boolean;
  on_collision?: CollisionRule;
}

/** How one input resolved (mirrors `dam-api` `Disposition`). */
export type Disposition =
  | "write"
  | "collision"
  | "skipped"
  | "unsupported"
  | "done"
  | "failed";

export interface ConvertItemReport {
  input: AssetId;
  input_path: string;
  planned_output: string;
  disposition: Disposition;
  input_bytes: number;
  output_bytes?: number | null;
  ratio?: number | null;
  error?: string | null;
}

/** Whole-batch result. Fail-soft: the request succeeds as long as it ran; counts summarise. */
export interface ConvertReport {
  dry_run: boolean;
  output_dir: string;
  items: ConvertItemReport[];
  total_input_bytes: number;
  total_output_bytes: number;
  done: number;
  failed: number;
  collisions: number;
  unsupported: number;
}

// ── export / manifests (phase 4: json/csv/sidecar) ──────────────────────────

/** Manifest shape (mirrors `dam-api` `ExportFormat`): a single JSON doc, a single CSV, or one JSON
 *  sidecar per asset under a directory. */
export type ExportFormat = "json" | "csv" | "sidecar";

/** What to export — explicit assets, a collection, or a search (the same faceted query as browse).
 *  `output` is a file path for json/csv, a directory for sidecar (destinations are server-side; for
 *  a local-first server that is the user's own disk). */
export interface ExportRequest {
  assets?: AssetId[];
  collection?: CollectionId | null;
  query?: QueryRequest | null;
  format: ExportFormat;
  output: string;
  attribution_only?: boolean;
}
export interface ExportReport {
  format: ExportFormat;
  output: string;
  assets: number;
  files_written: number;
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
  after?: string | null;
}

export interface DupMembershipRequest {
  assets: AssetId[];
}

export interface DupMembership {
  asset: AssetId;
  group: string;
  count: number;
}

export interface DupGroupMembersRequest {
  group: string;
  after?: string | null;
  limit?: number;
}

/** A cluster of duplicates for the review view. 3DAM only *groups* — nothing is auto-deleted. */
export interface DupGroup {
  kind: DupKind;
  media: MediaType;
  group: string | null;
  members: AssetSummary[];
  /** Complete visible group size; `members` is capped by the server. */
  total_members: number;
  members_cursor: string | null;
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
  /** Federation (phase 6): skip the peer fan-out and rank against local embeddings only. */
  local_only?: boolean;
}
/** One neighbour: the asset plus its cosine score and the embedding space it was ranked in. */
export interface SimilarHit {
  asset: AssetSummary;
  score: number; // cosine similarity in [0,1] (1 = identical direction)
  space: string; // the EmbeddingSpace id the ranking happened in
}

// ── folder navigation (issue #66) ────────────────────────────────────────────

/** Enumerate the immediate subfolders under `prefix` in one source. `prefix` is source-relative,
 *  empty (root) or ending in `/`. Backend route: POST /api/v1/folders. */
export interface FolderListing {
  source: SourceId;
  prefix: string;
}
/** One immediate subfolder: its segment `name` and the asset count of its whole subtree. */
export interface FolderEntry {
  name: string;
  asset_count: number;
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
  /**
   * Can this source accept an upload right now (issue #80)? Probed server-side, not inferred from
   * `kind` — a local source can sit on a read-only mount, and a federated peer is never writable.
   * Optional so an older server reads as read-only rather than advertising a write that would fail.
   */
  writable?: boolean;
  /** Explicit authorization/backend reason when `writable` is false. */
  writable_reason?: string | null;
}
/**
 * How an upload resolves a name that is already taken (issue #80).
 *
 * Deliberately *not* the convert pipeline's `CollisionRule`: there is no `overwrite` value, and
 * that absence is the feature — upload is the one path that writes inside a source, and clobbering
 * an existing file is not expressible in the API at all (tech-spec 08 §5.1).
 */
export type UploadCollision = 'fail' | 'suffix' | 'skip';

/** Query parameters for `POST /api/v1/upload`; the request body is the raw file bytes. */
export interface UploadRequest {
  source: SourceId;
  /** Source-relative destination directory. Empty means the source root. Created if missing. */
  folder?: string;
  /** The bare filename to create — rejected rather than sanitised if it is not a legal name. */
  name: string;
  collision?: UploadCollision;
}

/** What became of one uploaded file. */
export interface UploadOutcome {
  /** The path actually written — differs from `name` under `suffix`, so never assume the request. */
  path: string;
  /** `skip` left an existing file in place; nothing was written. */
  skipped: boolean;
  size: number;
  /** The catalogued asset, when the file was one 3DAM understands. */
  asset?: AssetId | null;
  /**
   * Set when the file was stored but deliberately not catalogued — an unsupported format, or
   * content on the blocklist. Render this: the upload succeeded, but the asset will not appear in
   * the library, and treating it as a plain success is how that becomes a confusing bug report.
   */
  uncatalogued_reason?: string | null;
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

// ── remove / blocklist (issue #21) ──────────────────────────────────────────

/** Remove one asset; `block` also records its content hash so a later scan never re-imports it. */
export interface RemoveAsset {
  block?: boolean;
}

/** One blocked content hash — the "removed + blocked" management surface. `label` is the filename
 *  captured at block time so the entry is recognisable after its asset row is gone. */
export interface BlockEntry {
  hash: ContentHash;
  label?: string | null;
  blocked_at: number;
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
/** Force-rebuild the derived preview cache for specific assets (mirrors `dam-api`
 *  `ThumbnailRegenRequest`): drop each asset's cached thumbnail/preview so the next view re-renders
 *  from source. Non-destructive; the source bytes are untouched. */
export interface ThumbnailRegenRequest {
  assets: AssetId[];
}
export interface ThumbnailRegenReport {
  assets: number;
  files_deleted: number;
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
export interface JobArtifact {
  label: string;
  /** Application-relative report or asset route; never a filesystem path. */
  route?: string | null;
}
export interface JobStatus {
  id: JobId;
  kind: JobKind;
  state: JobState;
  progress: Progress;
  error: string | null;
  summary?: string | null;
  warnings?: string[];
  result_artifacts?: JobArtifact[];
  result?: JobResult | null;
  created_at: number;
  updated_at: number;
  initiator?: string | null;
  /** Every source this job touches. `progress.current` names a live file path, so a
   *  visibility-restricted session is shown a job only when all of these are within its ceiling —
   *  jobs outside it are simply absent from `/jobs` and from the event stream (issue #42). */
  sources: SourceId[];
  collections: CollectionId[];
}
export type JobResult =
  | { kind: "convert"; report: ConvertReport }
  | { kind: "export"; report: ExportReport };
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
  /** Most-used confirmed tags (asset count per tag) — the vocabulary behind the Tags filter facet. */
  tags: Record<string, number>;
  unanalyzed: number;
  sources: number;
}

// ── live events (WebSocket) ─────────────────────────────────────────────────

export type ChangeKind =
  | "reanalyzed"
  | "retagged"
  | "license_set"
  | "metadata"
  | "note_set"
  | "commented";
/** Each per-asset variant carries the asset's `source_id`. That attribution is what lets the server
 *  evaluate a share-based visibility ceiling per event, so a restricted session gets a live stream
 *  for the sources it may reach instead of a silent socket (issue #42). `null` means unattributed;
 *  the server treats that as outside every restricted ceiling. */
export type LibraryEvent =
  | ({ type: "asset_added" } & AssetSummary)
  | { type: "asset_changed"; id: AssetId; source_id: SourceId | null; kind: ChangeKind }
  | { type: "asset_removed"; id: AssetId; source_id: SourceId | null }
  | { type: "source_state"; id: SourceId; state: SourceState }
  | ({ type: "job_progress" } & JobStatus)
  | { type: "stream_lagged" } // bounded subscriber missed events — refresh visible caches once
  | { type: "catalog_reset" }; // whole catalog wiped (maintenance) — drop caches and refetch

// ── user accounts (issue #42) ───────────────────────────────────────────────

/** An account's capability tier: `admin` (everything), `editor` (read+write), `viewer` (read). */
export type AccountRole = "admin" | "editor" | "viewer";

/** The signed-in account, as `/whoami` and the login/claim replies report it. */
export interface AccountRef {
  account_id: string;
  username: string;
  role: AccountRole;
}

/** Public accounts posture — `GET /api/v1/auth/status` (404 while the flag is off). */
export interface AuthStatus {
  enabled: boolean;
  /** No admin account exists yet: the first-run claim screen applies (localhost only). */
  unclaimed: boolean;
}

/** First-run claim of the initial admin account — `POST /api/v1/auth/claim`. */
export interface ClaimRequest {
  username: string;
  password: string;
  display_name?: string | null;
}

/** Username/password sign-in — `POST /api/v1/auth/login`. */
export interface LoginRequest {
  username: string;
  password: string;
}

/** Login/claim reply. The session rides an HttpOnly cookie; `csrf` (also mirrored in the readable
 *  `dam_csrf` cookie) must accompany every non-GET request as the `x-dam-csrf` header. */
export interface LoginReply {
  account: AccountRef;
  csrf: string;
}

/** One of the caller's live sessions — `GET /api/v1/auth/sessions`. */
export interface SessionInfo {
  session_id: string;
  created: number;
  last_seen: number;
  user_agent: string | null;
  /** The session this request rode in on — revoking it signs this browser out. */
  current: boolean;
}

// ── OIDC / OAuth2 login (issue #41) ─────────────────────────────────────────

/** What to do with a verified subject that no local account is linked to. The v1 default is
 *  `linked` — reject unless an admin has linked it, because for most issuers *anyone* can hold a
 *  valid account, so "the provider vouched for this token" is not authority to create one here. */
export type OidcProvisioning = "linked" | "auto_viewer" | "auto_editor";

/** What an operator may configure about the OIDC provider. */
export interface OidcConfig {
  /** Issuer URL; its discovery document supplies the endpoints and the JWKS URI. */
  issuer: string;
  client_id: string;
  /** Where the issuer sends the browser back. Must match what is registered with the provider. */
  redirect_url: string;
  /** Extra scopes beyond `openid`, which is always requested. */
  scopes: string[];
  provisioning: OidcProvisioning;
}

/** `GET /admin/api/oidc` — the config as read back. Flat on the wire (the Rust side flattens
 *  `OidcConfig` into it), and with **no field for the client secret**: "never returned by a GET"
 *  (tech-spec 10 §5) is enforced by the shape, not by remembering to strip it. */
export interface OidcConfigInfo extends OidcConfig {
  /** Whether a secret is on file. Never the secret. */
  client_secret_set: boolean;
}

/** `PUT /admin/api/oidc`. The secret is write-only and *optional on update*: omitting it keeps the
 *  stored one, so an operator can edit the issuer or scopes without re-entering a value the UI is
 *  never allowed to read back. An empty string explicitly clears it (a public client). */
export interface SetOidcConfig extends OidcConfig {
  client_secret?: string;
}

/** A link between a provider subject and a local account. The key is `(issuer, subject)` — `sub`
 *  is only unique *within* an issuer. */
export interface OidcIdentity {
  issuer: string;
  subject: string;
  account_id: string;
  /** The linked account's current username, denormalised so a list needs no second request. */
  username: string;
  linked_at: number;
}

/** `POST /admin/api/oidc/identities`. No issuer field: it comes from the configured provider, so a
 *  link can only ever name an issuer this instance actually accepts tokens from. */
export interface LinkOidcIdentity {
  subject: string;
  account_id: string;
}

// ── error envelope ──────────────────────────────────────────────────────────

export interface ErrorBody {
  code: string;
  message: string;
  detail?: unknown;
}
