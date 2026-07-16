// Front-door auth state (one gate at the connection; permissions decide after it). A tiny external
// store in the ws.ts style: the query/mutation caches raise the flag on any 401 and the AuthGate
// subscribes, swapping the workspace for the login screen — no toast spam, no per-call handling.

import { useSyncExternalStore } from "react";
import { ApiError } from "@/api/client";

// One consistent set of auth-copy strings, so the boot-probe rejection, the in-form rejection, and
// an expired session don't each phrase "bad token" differently (issue: consistent copy).
export const AUTH_COPY = {
  /** A stored/typed token the server rejected (401). */
  tokenRejected: "That token was rejected — check it and try again.",
  /** A previously-valid session token that is no longer accepted (revoked/expired). */
  sessionExpired: "Your session token is no longer valid — sign in again.",
  /** A token accepted but without the read scope needed to browse. */
  lacksRead: "That token is missing the read access needed to browse.",
  /** Could not reach the server at all. */
  unreachable: "Couldn't reach that server — check the address and that it's running.",
  /** The standard write-gate tooltip for a caller lacking the write scope. */
  needsWrite: "Requires write access — sign in with a token that has it.",
  /** A scope-denied write (403) reframed for a signed-in but under-scoped token. */
  writeDenied: "That action needs write access your token doesn't have.",
} as const;

let expired = false;
const listeners = new Set<() => void>();

/** Is this error the server saying "no/bad credential" (vs unreachable, or a scope miss)? */
export function isUnauthorized(e: unknown): e is ApiError {
  return e instanceof ApiError && e.status === 401;
}

/** Raise the auth-expired flag — the AuthGate takes over on the next render. */
export function notifyUnauthorized(): void {
  if (expired) return;
  expired = true;
  listeners.forEach((l) => l());
}

/** `true` once any request has come back 401 — the stored credential is missing/revoked. */
export function useAuthExpired(): boolean {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => expired,
    () => false,
  );
}
