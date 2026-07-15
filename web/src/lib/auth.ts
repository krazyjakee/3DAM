// Front-door auth state (one gate at the connection; permissions decide after it). A tiny external
// store in the ws.ts style: the query/mutation caches raise the flag on any 401 and the AuthGate
// subscribes, swapping the workspace for the login screen — no toast spam, no per-call handling.

import { useSyncExternalStore } from "react";
import { ApiError } from "@/api/client";

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
