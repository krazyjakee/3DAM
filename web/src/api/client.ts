// Thin typed client over the file-03 REST surface exposed by `3dam serve` (tech-spec 03 §8,
// mounted in crates/3dam-server/src/lib.rs). The DOM owns all networking/auth (tech-spec 09 §B.1);
// components never touch the engine directly. Requests are relative — same origin as the SPA,
// whether embedded in the binary or proxied through Vite in dev.

import type {
  AddSource,
  AnalyzeRequest,
  Asset,
  AssetId,
  Collection,
  CollectionId,
  CollectionMembers,
  ConvertReport,
  ConvertRequest,
  DupGroup,
  DupRequest,
  ExportReport,
  ExportRequest,
  JobId,
  JobListRequest,
  JobStatus,
  LibraryStats,
  NewCollection,
  Page,
  PageParams,
  AssetSummary,
  QueryRequest,
  RemoveSource,
  ScanRequest,
  SimilarHit,
  SimilarRequest,
  SourceId,
  SourceInfo,
  SuggestionReview,
  UpdateCollection,
  ErrorBody,
} from "./types";

const API = "/api/v1";

/** A hard failure of a whole call (tech-spec 03 §5). Mirrors `LibError`'s wire code. */
export class ApiError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly status: number,
    readonly detail?: unknown,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

async function decode<T>(res: Response): Promise<T> {
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  const body = text ? JSON.parse(text) : undefined;
  if (res.ok) return body as T;
  const err = body as ErrorBody | undefined;
  throw new ApiError(
    err?.code ?? "internal",
    err?.message ?? `HTTP ${res.status}`,
    res.status,
    err?.detail,
  );
}

async function get<T>(path: string): Promise<T> {
  return decode<T>(await fetch(path, { headers: { accept: "application/json" } }));
}

async function send<T>(method: string, path: string, body?: unknown): Promise<T> {
  return decode<T>(
    await fetch(path, {
      method,
      headers: { "content-type": "application/json", accept: "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
  );
}

export interface VersionInfo {
  api: string;
  server: string;
  capabilities: string[];
}

export const api = {
  version: () => get<VersionInfo>("/api/version"),

  // browse / search
  query: (req: QueryRequest) => send<Page<AssetSummary>>("POST", `${API}/query`, req),
  getAsset: (id: AssetId) => get<Asset>(`${API}/assets/${id}`),
  stats: () => get<LibraryStats>(`${API}/stats`),

  // auto-tag suggestion lifecycle — accept/reject one suggested tag (tech-spec 05 §1.4)
  reviewSuggestion: (req: SuggestionReview) =>
    send<void>("POST", `${API}/suggestions/review`, req),

  /** "More like this" — cosine over embeddings, ranked in the asset's media space (tech-spec 05 §3). */
  findSimilar: (req: SimilarRequest) => send<Page<SimilarHit>>("POST", `${API}/similar`, req),

  /** Duplicate groups for review — exact (content hash) or near (pHash/embedding), tech-spec 05 §4. */
  listDuplicates: (req: DupRequest) => send<DupGroup[]>("POST", `${API}/duplicates`, req),

  /** Export a manifest (json/csv/sidecar) for a selection / collection / query to a server path. */
  exportAssets: (req: ExportRequest) => send<ExportReport>("POST", `${API}/export`, req),

  /** Convert assets to a target format into a server output dir (source-safe, atomic; tech-spec 08). */
  convert: (req: ConvertRequest) => send<ConvertReport>("POST", `${API}/convert`, req),

  /** URL for an asset's raw bytes — fed to the WASM viewer islands (tech-spec 09 §B.3). The DOM
   *  fetches this and hands it across the wasm-bindgen boundary; the island does no networking. */
  assetContentUrl: (id: AssetId) => `${API}/assets/${id}/content`,

  /** URL for a server-rendered PNG thumbnail (tech-spec 04 §6.4). Only images produce one; other
   *  media return an error and the caller falls back to the honest typed tile. `edge` bounds the long side. */
  assetThumbnailUrl: (id: AssetId, edge = 256) => `${API}/assets/${id}/thumbnail?edge=${edge}`,

  // collections / smart folders (tech-spec: phase 4 Reach)
  listCollections: () => get<Collection[]>(`${API}/collections`),
  createCollection: (req: NewCollection) =>
    send<{ id: CollectionId }>("POST", `${API}/collections`, req),
  updateCollection: (id: CollectionId, req: UpdateCollection) =>
    send<void>("PUT", `${API}/collections/${id}`, req),
  deleteCollection: (id: CollectionId) => send<void>("DELETE", `${API}/collections/${id}`),
  modifyCollectionMembers: (id: CollectionId, req: CollectionMembers) =>
    send<void>("POST", `${API}/collections/${id}/members`, req),
  /** The assets in a collection (manual: the member list; smart: the saved query, resolved live). */
  collectionAssets: (id: CollectionId, page: PageParams) =>
    send<Page<AssetSummary>>("POST", `${API}/collections/${id}/assets`, page),

  // sources
  listSources: () => get<SourceInfo[]>(`${API}/sources`),
  addSource: (req: AddSource) => send<{ id: SourceId }>("POST", `${API}/sources`, req),
  removeSource: (id: SourceId, req: RemoveSource = {}) =>
    send<void>("DELETE", `${API}/sources/${id}`, req),

  // jobs
  submitScan: (req: ScanRequest) => send<{ job_id: JobId }>("POST", `${API}/jobs/scan`, req),
  submitAnalyze: (req: AnalyzeRequest) =>
    send<{ job_id: JobId }>("POST", `${API}/jobs/analyze`, req),
  listJobs: (req: JobListRequest = {}) => send<Page<JobStatus>>("POST", `${API}/jobs/list`, req),
  cancelJob: (id: JobId) => send<void>("POST", `${API}/jobs/${id}/cancel`),
};
