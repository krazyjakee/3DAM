// Thin typed client over the file-03 REST surface exposed by `3dam serve` (tech-spec 03 §8,
// mounted in crates/3dam-server/src/lib.rs). The DOM owns all networking/auth (tech-spec 09 §B.1);
// components never touch the engine directly. Requests are relative — same origin as the SPA,
// whether embedded in the binary or proxied through Vite in dev.

import type {
  AddSource,
  AnalyzeRequest,
  Asset,
  AssetId,
  BlockEntry,
  Collection,
  CollectionId,
  CollectionMembers,
  ContentHash,
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
  PrefetchRequest,
  AssetSummary,
  FavoriteRequest,
  Comment,
  FolderEntry,
  FolderListing,
  Note,
  QueryRequest,
  RemoveAsset,
  RemoveSource,
  ScanRequest,
  SimilarHit,
  SimilarRequest,
  SourceId,
  SourceInfo,
  UploadOutcome,
  UploadRequest,
  SuggestionReview,
  ThumbnailRegenRequest,
  ThumbnailRegenReport,
  UpdateCollection,
  ErrorBody,
} from "./types";

import { authHeaders, csrfHeaders, mediaUrl, resolveUrl } from "@/lib/server";

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
  return decode<T>(
    await fetch(resolveUrl(path), {
      headers: { accept: "application/json", ...authHeaders() },
    }),
  );
}

async function send<T>(method: string, path: string, body?: unknown): Promise<T> {
  return decode<T>(
    await fetch(resolveUrl(path), {
      method,
      headers: {
        "content-type": "application/json",
        accept: "application/json",
        ...authHeaders(),
        // Cookie-session callers (user accounts) must CSRF-stamp every mutation; harmless otherwise.
        ...csrfHeaders(),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
  );
}

export interface VersionInfo {
  api: string;
  server: string;
  capabilities: string[];
  /** The server's *effective* auth posture — lets the AuthGate render a login before provoking
   *  401s. Accounts-on forces at least "token". Absent on older servers (treated as "off"). */
  auth?: import("./admin").AuthMode;
  /** User accounts are enabled (issue #42): the gate offers username/password sign-in. */
  accounts?: boolean;
  /** The first-run claim window is open: either no account exists yet, or the operator re-opened
   *  it from the config file to recover a lost sole admin. */
  unclaimed?: boolean;
  /** How many accounts exist. Distinguishes a genuinely un-claimed instance (0 — the claim screen
   *  is the only thing to show) from a re-opened window (>0 — existing users can still sign in, so
   *  the login screen stays primary and the claim form is a secondary path). */
  account_count?: number;
  /** Uploads are enabled (issue #80). Off ⇒ POST /api/v1/upload is absent (404), so the Upload
   *  view and its nav entry hide rather than offering a drop target whose every request fails.
   *  Absent on older servers — treated as off, which is also this flag's default. */
  upload?: boolean;
}

/** Who the current credential (bearer token or session cookie) resolves to, and what it can do
 *  (front-door auth). Drives the scope-aware UI: write controls disable without `write`, the
 *  Settings link hides without `admin`. In token mode a missing/invalid credential 401s (the
 *  AuthGate owns that); anonymous mode reports the anonymous caller's scopes. */
export interface WhoAmI {
  identity: string | null;
  scopes: import("./admin").Scope[];
  anonymous: boolean;
  /** The signed-in user account, when the credential is an account session (issue #42). */
  account?: import("./types").AccountRef | null;
  /** Sharing rules scope this caller's view — some catalog content may be hidden. */
  restricted?: boolean;
}

export const api = {
  version: () => get<VersionInfo>("/api/version"),

  /** The current token's identity + scopes (front-door auth). Under token mode a missing/invalid
   *  token 401s here (the AuthGate handles it); anonymous mode returns the anonymous scopes. */
  whoami: () => get<WhoAmI>(`${API}/whoami`),

  // browse / search
  query: (req: QueryRequest) => send<Page<AssetSummary>>("POST", `${API}/query`, req),

  /** Prefetch hint (issue #72): ask the server to warm these assets' thumbnails/previews ahead of
   *  the grid's HTTP fetches. Fire-and-forget — the bytes still come over HTTP/2. */
  prefetch: (req: PrefetchRequest) => send<void>("POST", `${API}/prefetch`, req),
  getAsset: (id: AssetId) => get<Asset>(`${API}/assets/${id}`),
  /** Library aggregates. `source` scopes the counts to one source — a federated source reports
   *  the peer's own live numbers (the server proxies the read, phase 6). */
  stats: (source?: string | null) =>
    get<LibraryStats>(source ? `${API}/stats?source=${source}` : `${API}/stats`),

  /** Remove an asset from the catalog (source file untouched); `block` also blocks its content hash
   *  from any future scan (issue #21). */
  removeAsset: (id: AssetId, req: RemoveAsset = {}) =>
    send<void>("DELETE", `${API}/assets/${id}`, req),

  /** The rescan blocklist — content hashes removed with `block: true`. */
  listBlocklist: () => get<BlockEntry[]>(`${API}/blocklist`),
  /** Lift a block so the content can be re-imported by a later scan. */
  unblock: (hash: ContentHash) => send<void>("DELETE", `${API}/blocklist/${hash}`),

  // auto-tag suggestion lifecycle — accept/reject one suggested tag (tech-spec 05 §1.4)
  reviewSuggestion: (req: SuggestionReview) =>
    send<void>("POST", `${API}/suggestions/review`, req),

  /** Flag/unflag an asset as a favourite (issue #63). */
  setFavorite: (req: FavoriteRequest) => send<void>("POST", `${API}/assets/favorite`, req),

  /** Set or clear an asset's free-text note (issue #81); an empty body clears it. Resolves to the
   *  note the server actually stored (trimmed), or `null` once cleared. */
  setNote: (id: AssetId, body: string) =>
    send<Note | null>("PUT", `${API}/assets/${id}/note`, { body }),

  // per-asset discussion (issue #82) — 404s while the `user_accounts` flag is off
  listComments: (id: AssetId) => get<Comment[]>(`${API}/assets/${id}/comments`),
  postComment: (id: AssetId, body: string, replyTo?: string) =>
    send<Comment>("POST", `${API}/assets/${id}/comments`, { body, reply_to: replyTo ?? null }),
  editComment: (commentId: string, body: string) =>
    send<Comment>("PUT", `${API}/comments/${commentId}`, { body }),
  deleteComment: (commentId: string) => send<void>("DELETE", `${API}/comments/${commentId}`),

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
  assetContentUrl: (id: AssetId) => mediaUrl(`${API}/assets/${id}/content`),

  /** URL for a model asset's interactive 3D preview: the server-decoded, self-contained `DMSH` mesh
   *  blob (geometry + PBR materials + textures) the 3D island uploads directly. One Assimp decode
   *  server-side covers every format with textures, so the DOM never resolves external buffers. */
  assetPreviewMeshUrl: (id: AssetId) => mediaUrl(`${API}/assets/${id}/preview-mesh`),

  /** URL for a file referenced *relative to* an asset — a loose `.gltf`'s external `.bin`/textures
   *  (issue #56). `rel` is the glTF URI, resolved server-side against the asset's directory. */
  assetRelatedUrl: (id: AssetId, rel: string) =>
    mediaUrl(`${API}/assets/${id}/related?path=${encodeURIComponent(rel)}`),

  /** URL for a server-rendered PNG thumbnail (tech-spec 04 §6.4): a raster downscale for images, a
   *  wgpu turntable render for 3D models. Audio (and any render that fails) returns an error and the
   *  caller falls back to the honest typed tile. `edge` bounds the long side. `v` is a regeneration
   *  epoch (see thumbnail-cache.ts): bumping it after a forced regenerate defeats the browser + `Cache-Control`
   *  cache, whose key (content hash) is otherwise unchanged, so the fresh render is fetched. */
  assetThumbnailUrl: (id: AssetId, edge = 256, v = 0) =>
    mediaUrl(`${API}/assets/${id}/thumbnail?edge=${edge}${v ? `&v=${v}` : ""}`),

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

  /** Immediate subfolders under a source path — the lazy unit the folder tree expands (issue #66). */
  listFolders: (req: FolderListing) => send<FolderEntry[]>("POST", `${API}/folders`, req),

  /**
   * Write one file into a source (issue #80). The body is the raw bytes, so the file is streamed
   * rather than base64'd into JSON — a multi-hundred-megabyte asset must not be held in memory
   * twice. One request per file, which is what makes per-file progress and fail-soft natural:
   * `onProgress` rides this request's own upload progress, and a rejected file is one failed
   * promise among many rather than a batch that dies whole.
   *
   * Uses `XMLHttpRequest` rather than `fetch` for exactly one reason: `fetch` has no upload
   * progress event (request streaming is still not broadly available), and progress is the point.
   */
  upload: (req: UploadRequest, file: Blob, onProgress?: (fraction: number) => void) =>
    new Promise<UploadOutcome>((resolve, reject) => {
      const q = new URLSearchParams({ source: req.source, name: req.name });
      if (req.folder) q.set("folder", req.folder);
      if (req.collision) q.set("collision", req.collision);

      const xhr = new XMLHttpRequest();
      xhr.open("POST", resolveUrl(`${API}/upload?${q}`));
      xhr.setRequestHeader("content-type", "application/octet-stream");
      xhr.setRequestHeader("accept", "application/json");
      for (const [k, v] of Object.entries({ ...authHeaders(), ...csrfHeaders() })) {
        xhr.setRequestHeader(k, v as string);
      }
      if (onProgress) {
        xhr.upload.onprogress = (e) => {
          if (e.lengthComputable) onProgress(e.loaded / e.total);
        };
      }
      xhr.onload = () => {
        const body = xhr.responseText ? JSON.parse(xhr.responseText) : undefined;
        if (xhr.status >= 200 && xhr.status < 300) return resolve(body as UploadOutcome);
        const err = body as ErrorBody | undefined;
        reject(
          new ApiError(
            err?.code ?? "internal",
            err?.message ?? `HTTP ${xhr.status}`,
            xhr.status,
            err?.detail,
          ),
        );
      };
      xhr.onerror = () => reject(new ApiError("upstream", "upload failed", 0));
      xhr.onabort = () => reject(new ApiError("cancelled", "upload cancelled", 0));
      xhr.send(file);
    }),

  // jobs
  submitScan: (req: ScanRequest) => send<{ job_id: JobId }>("POST", `${API}/jobs/scan`, req),
  submitAnalyze: (req: AnalyzeRequest) =>
    send<{ job_id: JobId }>("POST", `${API}/jobs/analyze`, req),
  /** Force the derived preview cache for specific assets to be rebuilt (drop cached thumbnail +
   *  3D preview so the next view re-renders from source). Synchronous — returns what it dropped. */
  regenerateThumbnails: (req: ThumbnailRegenRequest) =>
    send<ThumbnailRegenReport>("POST", `${API}/thumbnails/regenerate`, req),
  listJobs: (req: JobListRequest = {}) => send<Page<JobStatus>>("POST", `${API}/jobs/list`, req),
  cancelJob: (id: JobId) => send<void>("POST", `${API}/jobs/${id}/cancel`),
};
