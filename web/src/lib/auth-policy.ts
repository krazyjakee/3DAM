// The anonymous-mode posture — isolated here because it is an owner-level product decision
// (2026-07: anonymous servers load read-only with a Sign in affordance; a hard gate with a
// browse-only escape would be a two-file change: this function + the StatusBar chip).

import type { AuthMode } from "@/api/admin";

export type BootDecision = "app" | "gate";

/** What the client renders at boot for a given server auth posture.
 *  - `off` (or an older server not reporting a mode): the app, no gates anywhere.
 *  - `token`: the login gate until a token validates.
 *  - `anonymous`: the app — reads are public by the operator's explicit choice; a stored-but-
 *    rejected token falls back to signed-out browsing rather than a wall. */
export function bootDecision(auth: AuthMode | undefined, hasValidToken: boolean): BootDecision {
  if (auth === "token" && !hasValidToken) return "gate";
  return "app";
}
