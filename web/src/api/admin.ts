// Typed client over the admin API (`/admin/api/*`, tech-spec 10 §5) — the same routes the CLI
// drives, so the web Settings surface and headless admin converge on one persisted state (ADR 0004
// decision 2). The DOM owns networking/auth (tech-spec 09 §B.1); a bearer token, when the operator
// has one, is read from localStorage and sent on every admin call (Token-mode servers).

import { ApiError } from "./client";

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

/** The admin bearer token an operator pasted in (Token-mode servers). Persisted locally only. */
const TOKEN_KEY = "dam_admin_token";
export function getAdminToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}
export function setAdminToken(token: string | null): void {
  if (token) localStorage.setItem(TOKEN_KEY, token);
  else localStorage.removeItem(TOKEN_KEY);
}

function headers(json: boolean): HeadersInit {
  const h: Record<string, string> = { accept: "application/json" };
  if (json) h["content-type"] = "application/json";
  const t = getAdminToken();
  if (t) h["authorization"] = `Bearer ${t}`;
  return h;
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
    decode<AdminStatus>(await fetch(`${ADMIN}/status`, { headers: headers(false) })),
  flags: async () =>
    decode<FlagInfo[]>(await fetch(`${ADMIN}/flags`, { headers: headers(false) })),
  setFlag: async (key: string, req: SetFlag) =>
    decode<FlagInfo>(
      await fetch(`${ADMIN}/flags/${key}`, {
        method: "PUT",
        headers: headers(true),
        body: JSON.stringify(req),
      }),
    ),
  tokens: async () =>
    decode<TokenInfo[]>(await fetch(`${ADMIN}/tokens`, { headers: headers(false) })),
  createToken: async (req: NewToken) =>
    decode<NewTokenReply>(
      await fetch(`${ADMIN}/tokens`, {
        method: "POST",
        headers: headers(true),
        body: JSON.stringify(req),
      }),
    ),
  revokeToken: async (id: string) =>
    decode<void>(
      await fetch(`${ADMIN}/tokens/${id}`, { method: "DELETE", headers: headers(false) }),
    ),
  audit: async (limit = 100) =>
    decode<AuditEntry[]>(await fetch(`${ADMIN}/audit?limit=${limit}`, { headers: headers(false) })),
};
