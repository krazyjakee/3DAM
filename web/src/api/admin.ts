// Typed client over the admin API (`/admin/api/*`, tech-spec 10 §5) — the same routes the CLI
// drives, so the web Settings surface and headless admin converge on one persisted state (ADR 0004
// decision 2). Admin calls ride the same server config as the rest of the client (front-door auth:
// one credential; when it carries the admin scope this surface opens, otherwise it 403s).

import { ApiError } from "./client";
import type {
  AccountRole,
  LinkOidcIdentity,
  OidcConfigInfo,
  OidcIdentity,
  SetOidcConfig,
} from "./types";
import { authHeaders, csrfHeaders, resolveUrl } from "@/lib/server";

const ADMIN = "/admin/api";

export type AuthMode = "off" | "anonymous" | "token";
export type McpMode = "off" | "read_only" | "read_write";
export type Scope = "read" | "write" | "admin" | "mcp_use" | "federate";

/** A flag's value is untagged JSON: an auth mode, an MCP mode, or a bool (tech-spec 10 §2.2). */
export type FlagValue = AuthMode | McpMode | boolean;

/** Every runtime feature-flag key the Settings surface can render (tech-spec 10 §2, ADR 0004). The
 *  server only *reports* the flags it supports — an older/leaner build may omit some — so Settings
 *  keys its cards off this union but renders an "unsupported on this server" note for any it asks
 *  for that the `/flags` response doesn't include, rather than a toggle that silently no-ops. */
export type FlagKey =
  | "authentication"
  | "mcp_server"
  | "network_writes"
  | "federation"
  | "user_accounts"
  | "auto_thumbnail"
  | "auto_analyze"
  | "upload"
  | "oidc";

export interface FlagInfo {
  key: FlagKey;
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
  // ── user accounts (issue #42; absent on older servers) ──
  accounts_enabled?: boolean;
  unclaimed?: boolean;
  account_count?: number;
  /** Uploads — writes of new files into a source (issue #80; absent on older servers). */
  upload_enabled?: boolean;
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

// ── user accounts / groups / shares (issue #42; 404 while the flag is off) ───

export interface AccountInfo {
  account_id: string;
  username: string;
  display_name: string | null;
  role: AccountRole;
  disabled: boolean;
  created: number;
  last_login: number | null;
}

export interface NewAccount {
  username: string;
  password: string;
  display_name?: string | null;
  role: AccountRole;
}

/** Patch an account: absent fields are unchanged. `password` resets the credential. */
export interface UpdateAccount {
  display_name?: string | null;
  role?: AccountRole;
  disabled?: boolean;
  password?: string;
}

export interface GroupInfo {
  group_id: string;
  name: string;
  created: number;
  /** Member account ids. */
  members: string[];
}

export type ShareResource = "source" | "collection";
export type ShareAccess = "read" | "write";

/** Grant one account *or* one group access to a source/collection (exactly one target set). */
export interface NewShare {
  resource: ShareResource;
  resource_id: string;
  account_id?: string | null;
  group_id?: string | null;
  access: ShareAccess;
}

export interface ShareInfo {
  share_id: string;
  resource: ShareResource;
  resource_id: string;
  account_id?: string | null;
  group_id?: string | null;
  access: ShareAccess;
  granted_by: string;
  created: number;
}

// ── storage & maintenance (Settings §Storage) ────────────────────────────────

export interface CacheUsage {
  bytes: number;
  files: number;
  budget_bytes: number;
  hits: number;
  misses: number;
  evictions: number;
  stale_deleted: number;
}

export interface StorageUsage {
  io_budgets: IoBudget[];
  io_stall_pct: number | null;
  cache_inventory_ready: boolean;
  data_dir: string;
  library_db_bytes: number;
  server_db_bytes: number;
  thumbnails: CacheUsage;
  previews: CacheUsage;
  peer_previews: CacheUsage;
  asset_count: number;
  source_count: number;
}

export interface IoBudget {
  device: string;
  kind: string;
  limit_bytes_per_sec: number;
  current_bytes_per_sec: number;
  concurrency: number;
  active: number;
  deferred: number;
  accounted_bytes: number;
  observed_bytes_per_sec: number | null;
  queue_depth: number | null;
  request_latency_ms: number | null;
  yield_reason: string;
  probes_available: boolean;
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
  // The CSRF stamp rides every admin call (cookie-session mutations need it; harmless elsewhere).
  const h: Record<string, string> = {
    accept: "application/json",
    ...authHeaders(),
    ...csrfHeaders(),
  };
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
  // ── OIDC provider + identity links (issue #41) ─────────────────────────────
  // `oidc` reads back `null` when nothing is configured, and link/unlink both answer with the
  // *whole* list so a mutation needs no follow-up read.
  oidcConfig: async () =>
    decode<OidcConfigInfo | null>(await fetch(url("/oidc"), { headers: headers(false) })),
  setOidcConfig: async (req: SetOidcConfig) =>
    decode<OidcConfigInfo | null>(
      await fetch(url("/oidc"), {
        method: "PUT",
        headers: headers(true),
        body: JSON.stringify(req),
      }),
    ),
  oidcIdentities: async () =>
    decode<OidcIdentity[]>(await fetch(url("/oidc/identities"), { headers: headers(false) })),
  linkOidcIdentity: async (req: LinkOidcIdentity) =>
    decode<OidcIdentity[]>(
      await fetch(url("/oidc/identities"), {
        method: "POST",
        headers: headers(true),
        body: JSON.stringify(req),
      }),
    ),
  /** `issuer` names *which* link to drop. Links are keyed on `(issuer, subject)` while the config
   *  holds one issuer, so a link made under a previous issuer is stale — it authenticates nobody,
   *  and without naming its issuer it could never be removed either. Omitted ⇒ the configured one. */
  unlinkOidcIdentity: async (subject: string, issuer?: string) =>
    decode<OidcIdentity[]>(
      await fetch(
        url(
          `/oidc/identities/${encodeURIComponent(subject)}` +
            (issuer ? `?issuer=${encodeURIComponent(issuer)}` : ""),
        ),
        { method: "DELETE", headers: headers(false) },
      ),
    ),

  audit: async (limit = 100) =>
    decode<AuditEntry[]>(await fetch(url(`/audit?limit=${limit}`), { headers: headers(false) })),

  // ── user accounts / groups / shares (issue #42) ────────────────────────────
  accounts: async () =>
    decode<AccountInfo[]>(await fetch(url("/accounts"), { headers: headers(false) })),
  createAccount: async (req: NewAccount) =>
    decode<AccountInfo>(
      await fetch(url("/accounts"), {
        method: "POST",
        headers: headers(true),
        body: JSON.stringify(req),
      }),
    ),
  updateAccount: async (id: string, req: UpdateAccount) =>
    decode<AccountInfo>(
      await fetch(url(`/accounts/${id}`), {
        method: "PUT",
        headers: headers(true),
        body: JSON.stringify(req),
      }),
    ),
  deleteAccount: async (id: string) =>
    decode<void>(
      await fetch(url(`/accounts/${id}`), { method: "DELETE", headers: headers(false) }),
    ),
  /** Revoke every live session of one account ("sign out everywhere"). */
  revokeAccountSessions: async (id: string) =>
    decode<{ revoked: number }>(
      await fetch(url(`/accounts/${id}/sessions`), { method: "DELETE", headers: headers(false) }),
    ),

  groups: async () =>
    decode<GroupInfo[]>(await fetch(url("/groups"), { headers: headers(false) })),
  createGroup: async (name: string) =>
    decode<GroupInfo>(
      await fetch(url("/groups"), {
        method: "POST",
        headers: headers(true),
        body: JSON.stringify({ name }),
      }),
    ),
  deleteGroup: async (id: string) =>
    decode<void>(
      await fetch(url(`/groups/${id}`), { method: "DELETE", headers: headers(false) }),
    ),
  /** Replace a group's member set (the full list, not a delta). */
  setGroupMembers: async (id: string, accountIds: string[]) =>
    decode<GroupInfo>(
      await fetch(url(`/groups/${id}/members`), {
        method: "PUT",
        headers: headers(true),
        body: JSON.stringify({ account_ids: accountIds }),
      }),
    ),

  shares: async () =>
    decode<ShareInfo[]>(await fetch(url("/shares"), { headers: headers(false) })),
  createShare: async (req: NewShare) =>
    decode<ShareInfo>(
      await fetch(url("/shares"), {
        method: "POST",
        headers: headers(true),
        body: JSON.stringify(req),
      }),
    ),
  deleteShare: async (id: string) =>
    decode<void>(
      await fetch(url(`/shares/${id}`), { method: "DELETE", headers: headers(false) }),
    ),

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
