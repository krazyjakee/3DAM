// Typed client over the admin API (`/admin/api/*`, tech-spec 10 §5) — the same routes the CLI
// drives, so the web Settings surface and headless admin converge on one persisted state (ADR 0004
// decision 2). Admin calls ride the same server config as the rest of the client (front-door auth:
// one credential; when it carries the admin scope this surface opens, otherwise it 403s).

import { ApiError } from "./client";
import { authHeaders, resolveUrl } from "@/lib/server";

const ADMIN = "/admin/api";

export type AuthMode = "off" | "anonymous" | "token";
export type McpMode = "off" | "read_only" | "read_write";
export type Scope = "read" | "write" | "admin" | "mcp_use" | "federate";

/** A flag's value is untagged JSON: an auth mode, an MCP mode, or a bool (tech-spec 10 §2.2). */
export type FlagValue = AuthMode | McpMode | boolean;

export interface FlagInfo {
  key: "authentication" | "mcp_server" | "network_writes";
  value: FlagValue;
  version: number;
  live: boolean;
  exposure_increasing: boolean;
}

export interface SetFlag {
  value: FlagValue;
  expected_version?: number | null;
  confirm?: boolean;
}

/** Reply to a flag set: the flag fields (flattened) — plus, exactly when enabling authentication
 *  minted the first admin credential, the bootstrap owner token (secret shown once). */
export interface SetFlagReply extends FlagInfo {
  bootstrap_token?: NewTokenReply | null;
}

export interface AdminStatus {
  bind: string;
  localhost_only: boolean;
  tls: boolean;
  auth: AuthMode;
  mcp: McpMode;
  network_writes: boolean;
  exposed_without_auth: boolean;
  token_count: number;
}

export interface TokenInfo {
  token_id: string;
  label: string;
  scopes: Scope[];
  created: number;
  expires: number | null;
  last_used: number | null;
}

export interface NewToken {
  label: string;
  scopes: Scope[];
  expires?: number | null;
}

export interface NewTokenReply {
  token_id: string;
  label: string;
  scopes: Scope[];
  secret: string;
}

export interface AuditEntry {
  at: number;
  actor: string;
  action: string;
  target?: string | null;
  detail?: unknown;
}

// ── storage & maintenance (Settings §Storage) ────────────────────────────────

export interface CacheUsage {
  bytes: number;
  files: number;
}

export interface StorageUsage {
  data_dir: string;
  library_db_bytes: number;
  server_db_bytes: number;
  thumbnails: CacheUsage;
  previews: CacheUsage;
  asset_count: number;
  source_count: number;
}

export type CacheTarget = "thumbnails" | "previews" | "all";

export interface ClearCacheReport {
  bytes_freed: number;
  files_deleted: number;
}

export interface ClearAnalysisReport {
  suggestions_removed: number;
  embeddings_removed: number;
}

export interface VacuumReport {
  before_bytes: number;
  after_bytes: number;
  reclaimed_bytes: number;
}

export interface WipeReport {
  assets_removed: number;
  sources_removed: number;
  collections_removed: number;
  tags_removed: number;
}

export interface FactoryResetReport {
  catalog: WipeReport;
  cache: ClearCacheReport;
  tokens_removed: number;
}

function headers(json: boolean): HeadersInit {
  const h: Record<string, string> = { accept: "application/json", ...authHeaders() };
  if (json) h["content-type"] = "application/json";
  return h;
}

/** Admin route, resolved against the configured server (not just the SPA origin). */
function url(path: string): string {
  return resolveUrl(`${ADMIN}${path}`);
}

async function decode<T>(res: Response): Promise<T> {
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  const body = text ? JSON.parse(text) : undefined;
  if (res.ok) return body as T;
  throw new ApiError(
    body?.code ?? "internal",
    body?.message ?? `HTTP ${res.status}`,
    res.status,
    body?.detail,
  );
}

export const admin = {
  status: async () =>
    decode<AdminStatus>(await fetch(url("/status"), { headers: headers(false) })),
  flags: async () =>
    decode<FlagInfo[]>(await fetch(url("/flags"), { headers: headers(false) })),
  setFlag: async (key: string, req: SetFlag) =>
    decode<SetFlagReply>(
      await fetch(url(`/flags/${key}`), {
        method: "PUT",
        headers: headers(true),
        body: JSON.stringify(req),
      }),
    ),
  tokens: async () =>
    decode<TokenInfo[]>(await fetch(url("/tokens"), { headers: headers(false) })),
  createToken: async (req: NewToken) =>
    decode<NewTokenReply>(
      await fetch(url("/tokens"), {
        method: "POST",
        headers: headers(true),
        body: JSON.stringify(req),
      }),
    ),
  revokeToken: async (id: string) =>
    decode<void>(
      await fetch(url(`/tokens/${id}`), { method: "DELETE", headers: headers(false) }),
    ),
  audit: async (limit = 100) =>
    decode<AuditEntry[]>(await fetch(url(`/audit?limit=${limit}`), { headers: headers(false) })),

  // ── storage & maintenance ──────────────────────────────────────────────────
  storageUsage: async () =>
    decode<StorageUsage>(await fetch(url("/maintenance/usage"), { headers: headers(false) })),
  clearCache: async (target: CacheTarget) =>
    decode<ClearCacheReport>(
      await fetch(url("/maintenance/clear-cache"), {
        method: "POST",
        headers: headers(true),
        body: JSON.stringify({ target }),
      }),
    ),
  clearAnalysis: async () =>
    decode<ClearAnalysisReport>(
      await fetch(url("/maintenance/clear-analysis"), { method: "POST", headers: headers(false) }),
    ),
  vacuum: async () =>
    decode<VacuumReport>(
      await fetch(url("/maintenance/vacuum"), { method: "POST", headers: headers(false) }),
    ),
  wipe: async (confirm: boolean) =>
    decode<WipeReport>(
      await fetch(url("/maintenance/wipe"), {
        method: "POST",
        headers: headers(true),
        body: JSON.stringify({ confirm }),
      }),
    ),
  factoryReset: async (confirm: boolean) =>
    decode<FactoryResetReport>(
      await fetch(url("/maintenance/factory-reset"), {
        method: "POST",
        headers: headers(true),
        body: JSON.stringify({ confirm }),
      }),
    ),
};
