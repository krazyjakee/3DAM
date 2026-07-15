// Single source of truth for "can we talk to the server, and are live updates flowing?" — folds the
// backend-reachability signal (the stats query erroring) together with the WebSocket state (ws.ts)
// so Navigation and the StatusBar surface one consistent status instead of silent zeros (issues #24, #25).

import { useStats } from "./queries";
import { useWsConnected } from "./ws";
import { isUnauthorized } from "@/lib/auth";

export type ConnState = "online" | "reconnecting" | "offline";

export interface Connection {
  /** `offline` = backend unreachable; `reconnecting` = backend up but live socket down; `online` = both. */
  state: ConnState;
  /** The REST backend can't be reached at all (queries are failing). */
  backendDown: boolean;
  /** Live WebSocket updates are flowing. */
  live: boolean;
}

export function useConnection(): Connection {
  // `stats` has no polling of its own, but the WS reconnect invalidates it, so recovery clears the error.
  const stats = useStats();
  const wsConnected = useWsConnected();
  // A 401 is the AuthGate's business (missing/revoked credential), never "offline".
  const backendDown = stats.isError && !isUnauthorized(stats.error);
  const state: ConnState = backendDown ? "offline" : wsConnected ? "online" : "reconnecting";
  return { state, backendDown, live: wsConnected && !backendDown };
}
