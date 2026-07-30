// Typed client over the public accounts surface (`/api/v1/auth/*`, issue #42) — username/password
// sign-in with an HttpOnly session cookie. The browser carries the cookie automatically (same
// origin); mutations are CSRF-stamped from the readable `dam_csrf` mirror (lib/server.ts). Every
// route here 404s while the `user_accounts` flag is off — callers treat that as "disabled".

import { ApiError } from "./client";
import type { AuthStatus, ClaimRequest, LoginReply, LoginRequest, SessionInfo } from "./types";
import { authHeaders, csrfHeaders, resolveUrl } from "@/lib/server";

const AUTH = "/api/v1/auth";

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

async function get<T>(path: string): Promise<T> {
  return decode<T>(
    await fetch(resolveUrl(`${AUTH}${path}`), {
      headers: { accept: "application/json", ...authHeaders() },
    }),
  );
}

async function send<T>(method: string, path: string, body?: unknown): Promise<T> {
  return decode<T>(
    await fetch(resolveUrl(`${AUTH}${path}`), {
      method,
      headers: {
        accept: "application/json",
        ...(body === undefined ? {} : { "content-type": "application/json" }),
        ...authHeaders(),
        ...csrfHeaders(),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
  );
}

export const authApi = {
  /** Accounts posture — 404 (throws) while the `user_accounts` flag is off. */
  status: () => get<AuthStatus>("/status"),

  /** First-run: create the initial admin account (only while unclaimed, from localhost). Sets the
   *  session + CSRF cookies on success. */
  claim: (req: ClaimRequest) => send<LoginReply>("POST", "/claim", req),

  /** Username/password sign-in. Sets the session + CSRF cookies on success; 401 = bad credentials,
   *  429 = lockout. */
  login: (req: LoginRequest) => send<LoginReply>("POST", "/login", req),

  /** End the current session (clears the cookies server-side). Needs the CSRF header. */
  logout: () => send<void>("POST", "/logout"),

  /** The caller's live sessions, the current one marked. */
  sessions: () => get<SessionInfo[]>("/sessions"),

  /** Revoke one of the caller's sessions (revoking the current one signs this browser out). */
  revokeSession: (id: string) => send<void>("DELETE", `/sessions/${id}`),
};
