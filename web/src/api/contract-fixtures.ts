import type {
  AccountRole,
  Asset,
  AssetSummary,
  ChangeKind,
  Collection,
  CollectionKind,
  CollisionRule,
  ConvertRequest,
  ConvertReport,
  ConvertTarget,
  Disposition,
  DupKind,
  ErrorBody,
  ExportFormat,
  FacetField,
  FilterOp,
  FilterValue,
  ItemWarning,
  JobResult,
  JobStatus,
  JobKind,
  JobState,
  LicenseStatus,
  MediaType,
  MediaAttributes,
  Origin,
  OidcProvisioning,
  ReviewAction,
  SuggestionState,
  ScanMode,
  SearchMode,
  SortDir,
  SortField,
  SourceInfo,
  SourceKind,
  SourceState,
  LibraryEvent,
  Page,
  Progress,
  QueryRequest,
  UploadCollision,
} from "./types";
import type {
  AuthMode,
  CacheTarget,
  FlagValue,
  FlagKey,
  McpMode,
  Scope,
  ShareAccess,
  ShareResource,
  SetFlag,
  SetFlagReply,
} from "./admin";
import type { WhoAmI } from "./client";

type Values<T extends readonly unknown[]> = T[number];
type SameUnion<A, B> = [Exclude<A, B>, Exclude<B, A>] extends [never, never] ? true : false;
export type ContractAssert<T extends true> = T;
export type EventTopicWire = "assets" | "sources" | "jobs" | "analysis";
export interface SubscribeRequestWire {
  topics: EventTopicWire[];
}
export type JobEventWire =
  | ({ type: "progress" } & Progress)
  | ({ type: "warning" } & ItemWarning)
  | ({ type: "done" } & JobStatus)
  | { type: "failed"; message: string };

export const API_V1_FIELDLESS_ENUMS = {
  media_type: ["audio", "image", "model", "video", "document"],
  license_status: ["permissive", "attribution", "restricted", "unknown"],
  search_mode: ["lexical", "hybrid", "semantic"],
  facet_field: ["media_type", "format", "source", "tag", "size_bytes", "license", "usage_right", "width", "height", "color_depth", "has_alpha", "color_space", "image_class", "tileability", "tile_class", "bpm", "duration", "sample_rate", "bit_depth", "channels", "musical_key", "loudness", "brightness", "harmonicity", "audio_class", "codec", "container", "tri_count", "vertex_count", "mesh_count", "material_count", "texture_count", "dependency_bytes", "has_rig", "has_animation", "has_uv", "model_class", "fps", "bitrate", "has_audio", "video_class", "page_count", "word_count", "author", "document_class", "favorite", "path", "folder"],
  filter_op: ["eq", "ne", "lt", "lte", "gt", "gte", "in", "range", "contains", "exists"],
  sort_field: ["name", "size", "scanned", "relevance"],
  sort_dir: ["asc", "desc"],
  source_kind: ["local_fs", "sftp", "smb", "federated"],
  scan_mode: ["full", "delta"],
  job_kind: ["scan", "analyze", "convert", "export"],
  job_state: ["queued", "running", "paused", "done", "failed", "cancelled"],
  collision_rule: ["fail", "suffix", "skip", "overwrite"],
  disposition: ["write", "collision", "skipped", "unsupported", "done", "failed"],
  upload_collision: ["fail", "suffix", "skip"],
  dup_kind: ["exact", "near"],
  review_action: ["accept", "reject", "undo"],
  suggestion_state: ["pending", "confirmed", "rejected"],
  collection_kind: ["manual", "smart"],
  export_format: ["json", "csv", "sidecar"],
  change_kind: ["reanalyzed", "retagged", "license_set", "metadata", "note_set", "commented"],
  event_topic: ["assets", "sources", "jobs", "analysis"],
  scope: ["read", "write", "admin", "mcp_use", "federate"],
  auth_mode: ["off", "anonymous", "token"],
  mcp_mode: ["off", "read_only", "read_write"],
  flag_key: ["authentication", "mcp_server", "network_writes", "auto_thumbnail", "auto_analyze", "federation", "user_accounts", "upload", "oidc"],
  cache_target: ["thumbnails", "previews", "all"],
  oidc_provisioning: ["linked", "auto_viewer", "auto_editor"],
  account_role: ["admin", "editor", "viewer"],
  share_resource: ["source", "collection"],
  share_access: ["read", "write"],
} as const;

export type _MediaTypeContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.media_type>, MediaType>>;
export type _LicenseContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.license_status>, LicenseStatus>>;
export type _SearchModeContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.search_mode>, SearchMode>>;
export type _FacetContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.facet_field>, FacetField>>;
export type _FilterOpContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.filter_op>, FilterOp>>;
export type _SortFieldContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.sort_field>, SortField>>;
export type _SortDirContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.sort_dir>, SortDir>>;
export type _SourceKindContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.source_kind>, SourceKind>>;
export type _ScanModeContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.scan_mode>, ScanMode>>;
export type _JobKindContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.job_kind>, JobKind>>;
export type _JobStateContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.job_state>, JobState>>;
export type _CollisionContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.collision_rule>, CollisionRule>>;
export type _DispositionContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.disposition>, Disposition>>;
export type _UploadCollisionContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.upload_collision>, UploadCollision>>;
export type _DupKindContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.dup_kind>, DupKind>>;
export type _ReviewContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.review_action>, ReviewAction>>;
export type _SuggestionStateContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.suggestion_state>, SuggestionState>>;
export type _CollectionKindContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.collection_kind>, CollectionKind>>;
export type _ExportFormatContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.export_format>, ExportFormat>>;
export type _ChangeKindContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.change_kind>, ChangeKind>>;
export type _EventTopicContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.event_topic>, EventTopicWire>>;
export type _ScopeContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.scope>, Scope>>;
export type _AuthModeContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.auth_mode>, AuthMode>>;
export type _McpModeContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.mcp_mode>, McpMode>>;
export type _FlagKeyContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.flag_key>, FlagKey>>;
export type _CacheTargetContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.cache_target>, CacheTarget>>;
export type _OidcContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.oidc_provisioning>, OidcProvisioning>>;
export type _RoleContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.account_role>, AccountRole>>;
export type _ShareResourceContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.share_resource>, ShareResource>>;
export type _ShareAccessContract = ContractAssert<SameUnion<Values<typeof API_V1_FIELDLESS_ENUMS.share_access>, ShareAccess>>;

export const API_V1_VARIANT_EXAMPLES = {
  origin: ["local", { peer: "studio-a" }] satisfies readonly Origin[],
  media_attributes: [
    { media: "audio", duration_ms: null, sample_rate: null, bit_depth: null, channels: null, codec: null, container: null, class: null, loudness_lufs: null, brightness: null, harmonicity: null, peaks: null },
    { media: "image", width: null, height: null, color_depth: null, has_alpha: null, color_space: null, texture_format: null, mip_levels: null, phash: null, tileability: null, repeat_period: null, tile_class: null, dominant_colors: [], class: null },
    { media: "model", vertex_count: null, triangle_count: null, mesh_count: null, material_count: null, texture_count: null, dependency_bytes: null, has_rig: null, has_animation: null, has_uvs: null, class: null },
    { media: "video", duration_ms: null, width: null, height: null, fps: null, codec: null, container: null, bitrate: null, has_audio: null, class: null },
    { media: "document", page_count: null, word_count: null, title: null, author: null, encoding: null, excerpt: null, class: null },
    { media: "none" },
  ] satisfies readonly MediaAttributes[],
  filter_value: [{ str: "drum" }, { num: 120 }, { bool: true }, { range: [80, 140] }, { list: [{ str: "wav" }] }] satisfies readonly FilterValue[],
  convert_target: [{ media: "image", format: "webp", max_edge: 1024, quality: 85 }, { media: "audio", format: "wav" }, { media: "model", format: "glb", optimize: true }] satisfies readonly ConvertTarget[],
  job_result: [
    { kind: "convert", report: { dry_run: true, output_dir: "/exports", items: [], total_input_bytes: 0, total_output_bytes: 0, done: 0, failed: 0, collisions: 0, unsupported: 0 } },
    { kind: "export", report: { format: "json", output: "/exports/assets.json", assets: 1, files_written: 1 } },
  ] satisfies readonly JobResult[],
  source_state: ["online", "offline", "scanning", { error: "unreachable" }] satisfies readonly SourceState[],
  library_event: [
    { type: "asset_added", id: "018f0000-0000-7000-8000-000000000001", name: "kick.wav", media: "audio", format: "wav", size: 12, license: { id: null, status: "unknown" }, top_tags: [], origin: "local", key_attrs: {}, favorite: false, source_id: "018f0000-0000-7000-8000-000000000002" },
    { type: "asset_changed", id: "018f0000-0000-7000-8000-000000000001", source_id: "018f0000-0000-7000-8000-000000000002", kind: "metadata" },
    { type: "asset_removed", id: "018f0000-0000-7000-8000-000000000001", source_id: null },
    { type: "source_state", id: "018f0000-0000-7000-8000-000000000002", state: "online" },
    { type: "job_progress", id: "018f0000-0000-7000-8000-000000000003", kind: "scan", state: "running", progress: { done: 0, total: 1, current: null }, error: null, created_at: 0, updated_at: 0, sources: [], collections: [] },
    { type: "stream_lagged" },
    { type: "catalog_reset" },
  ] satisfies readonly LibraryEvent[],
  job_event: [
    { type: "progress", done: 1, total: 2, current: "kick.wav" },
    { type: "warning", subject: "kick.wav", code: "decode", message: "metadata only" },
    { type: "done", id: "018f0000-0000-7000-8000-000000000003", kind: "scan", state: "done", progress: { done: 1, total: 1, current: null }, error: null, created_at: 0, updated_at: 0, sources: [], collections: [] },
    { type: "failed", message: "scan failed" },
  ] satisfies readonly JobEventWire[],
  flag_value: ["token", "read_only", true] satisfies readonly FlagValue[],
};

export type _MediaAttributeTags = ContractAssert<SameUnion<(typeof API_V1_VARIANT_EXAMPLES.media_attributes)[number]["media"], MediaAttributes["media"]>>;
export type _ConvertTargetTags = ContractAssert<SameUnion<(typeof API_V1_VARIANT_EXAMPLES.convert_target)[number]["media"], ConvertTarget["media"]>>;
export type _JobResultTags = ContractAssert<SameUnion<(typeof API_V1_VARIANT_EXAMPLES.job_result)[number]["kind"], JobResult["kind"]>>;
export type _LibraryEventTags = ContractAssert<SameUnion<(typeof API_V1_VARIANT_EXAMPLES.library_event)[number]["type"], LibraryEvent["type"]>>;

const assetSummary = {
  id: "018f0000-0000-7000-8000-000000000001",
  name: "kick.wav",
  media: "audio",
  format: "wav",
  size: 12,
  license: { id: null, status: "unknown" },
  top_tags: ["drum"],
  origin: "local",
  key_attrs: { duration: "1s" },
  favorite: false,
  source_id: "018f0000-0000-7000-8000-000000000002",
} satisfies AssetSummary;

export const API_V1_REPRESENTATIVES = {
  query_request_minimal: {} satisfies QueryRequest,
  query_request_full: {
    text: "kick",
    filters: [{ field: "media_type", op: "eq", value: { str: "audio" } }],
    sort: { field: "relevance", dir: "desc" },
    page: { after: "next-page", limit: 24 },
    include_facets: true,
    include_total: true,
    mode: "hybrid",
    local_only: true,
  } satisfies QueryRequest,
  asset_summary: assetSummary,
  asset: {
    summary: assetSummary,
    hash: "0101010101010101010101010101010101010101010101010101010101010101",
    source_id: "018f0000-0000-7000-8000-000000000002",
    path: "kick.wav",
    timestamps: { created: 1700000000000, modified: null, scanned: 1700000001000, analyzed: null },
    attributes: { media: "none" },
    license: { id: null, status: "unknown", commercial: null, modify: null, redistribute: null, attribution: null, holder: null, credit: null, url: null, provenance: "unknown" },
    tags: [{ name: "drum", state: "confirmed", source: "user", confidence: null }],
    collections: ["018f0000-0000-7000-8000-000000000004"],
    note: { body: "usable kick", updated_at: 1700000002000, updated_by: "contract-test" },
  } satisfies Asset,
  asset_page: {
    items: [assetSummary],
    cursor: null,
    total: 1,
    partial: { complete: true },
  } satisfies Page<AssetSummary>,
  job_status: {
    id: "018f0000-0000-7000-8000-000000000003",
    kind: "scan",
    state: "done",
    progress: { done: 1, total: 1, current: null },
    error: null,
    summary: "Scanned one asset",
    created_at: 1700000000000,
    updated_at: 1700000001000,
    sources: ["018f0000-0000-7000-8000-000000000002"],
    collections: [],
  } satisfies JobStatus,
  source_info: {
    id: "018f0000-0000-7000-8000-000000000002",
    kind: "local_fs",
    name: "Samples",
    uri: "/library/samples",
    state: "online",
    stats: { asset_count: 1, last_scanned_at: 1700000000000, last_error: null },
    watch: false,
    writable: true,
  } satisfies SourceInfo,
  collection: {
    id: "018f0000-0000-7000-8000-000000000004",
    name: "Favourites",
    kind: "manual",
    count: 1,
    created_at: 1700000000000,
    updated_at: 1700000000000,
  } satisfies Collection,
  convert_request: {
    inputs: ["018f0000-0000-7000-8000-000000000001"],
    target: { media: "image", format: "webp", max_edge: 1024, quality: 85 },
    output_dir: "/exports",
    dry_run: true,
    on_collision: "suffix",
  } satisfies ConvertRequest,
  convert_report: {
    dry_run: true,
    output_dir: "/exports",
    items: [{ input: "018f0000-0000-7000-8000-000000000001", input_path: "kick.wav", planned_output: "/exports/kick.webp", disposition: "unsupported", input_bytes: 12, output_bytes: null, ratio: null, error: "media mismatch" }],
    total_input_bytes: 12,
    total_output_bytes: 0,
    done: 0,
    failed: 0,
    collisions: 0,
    unsupported: 1,
  } satisfies ConvertReport,
  whoami: {
    identity: "contract-test",
    scopes: ["read", "write"],
    anonymous: false,
    restricted: false,
  } satisfies WhoAmI,
  set_flag: {
    value: true,
    expected_version: null,
    confirm: true,
  } satisfies SetFlag,
  set_flag_reply: {
    key: "upload",
    value: true,
    version: 1,
    live: true,
    exposure_increasing: true,
  } satisfies SetFlagReply,
  error_body: {
    code: "not_found",
    message: "asset not found",
    detail: { resource: "asset" },
  } satisfies ErrorBody,
  subscribe_request: { topics: ["assets", "jobs"] } satisfies SubscribeRequestWire,
} as const;
