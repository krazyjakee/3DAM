// Thin typed client over the file-03 REST surface exposed by `3dam serve` (tech-spec 03 §8,
// mounted in crates/3dam-server/src/lib.rs). The DOM owns all networking/auth (tech-spec 09 §B.1);
// components never touch the engine directly. Requests are relative — same origin as the SPA,
// whether embedded in the binary or proxied through Vite in dev.

import type {
  AddSource,
  Asset,
  AssetId,
  JobId,
  JobListRequest,
  JobStatus,
  LibraryStats,
  Page,
  AssetSummary,
  QueryRequest,
  RemoveSource,
  ScanRequest,
  SourceId,
  SourceInfo,
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

  /** URL for an asset's raw bytes — fed to the WASM viewer islands (tech-spec 09 §B.3). The DOM
   *  fetches this and hands it across the wasm-bindgen boundary; the island does no networking. */
  assetContentUrl: (id: AssetId) => `${API}/assets/${id}/content`,

  // sources
  listSources: () => get<SourceInfo[]>(`${API}/sources`),
  getSource: (id: SourceId) => get<SourceInfo>(`${API}/sources/${id}`),
  addSource: (req: AddSource) => send<{ id: SourceId }>("POST", `${API}/sources`, req),
  removeSource: (id: SourceId, req: RemoveSource = {}) =>
    send<void>("DELETE", `${API}/sources/${id}`, req),

  // jobs
  submitScan: (req: ScanRequest) => send<{ job_id: JobId }>("POST", `${API}/jobs/scan`, req),
  listJobs: (req: JobListRequest = {}) => send<Page<JobStatus>>("POST", `${API}/jobs/list`, req),
  getJob: (id: JobId) => get<JobStatus>(`${API}/jobs/${id}`),
  cancelJob: (id: JobId) => send<void>("POST", `${API}/jobs/${id}/cancel`),
};
