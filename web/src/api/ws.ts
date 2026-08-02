// Live-update transport (tech-spec 09 §A.3). One WebSocket to `/api/ws` carries the file-03
// `LibraryEvent` firehose; on each event we invalidate the affected TanStack Query caches so the
// grid, stats, sources, and jobs stay live without polling. Auto-reconnects with backoff.

import { useEffect, useSyncExternalStore } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { LibraryEvent } from "./types";
import { LiveEventCacheBatcher } from "./live-event-cache";
import { authenticatedFetch, csrfHeaders, wsUrl as resolveWsUrl } from "@/lib/server";

// Browser WebSockets cannot attach Authorization. Mint a 30-second, one-use, WS-only ticket over
// an authenticated fetch, then put only that derived credential in the upgrade URI (issue #128).
async function wsUrl(): Promise<string> {
  const response = await authenticatedFetch("/api/v1/ws-ticket", {
    method: "POST",
    headers: { accept: "application/json", ...csrfHeaders() },
  });
  if (!response.ok) throw new Error(`live-update ticket failed (${response.status})`);
  const body = (await response.json()) as { ticket: string };
  return resolveWsUrl("/api/v1/ws", body.ticket);
}

// Live-connection state, surfaced to the UI so a silent disconnect becomes visible (issue #25).
// A tiny external store rather than context: ws.ts is a leaf effect and any region can subscribe.
let wsConnected = false;
const wsListeners = new Set<() => void>();
function setWsConnected(next: boolean) {
  if (wsConnected === next) return;
  wsConnected = next;
  wsListeners.forEach((l) => l());
}

/** `true` while the live-update WebSocket is open; `false` while down/reconnecting. */
export function useWsConnected(): boolean {
  return useSyncExternalStore(
    (l) => {
      wsListeners.add(l);
      return () => wsListeners.delete(l);
    },
    () => wsConnected,
    () => false,
  );
}

/** Subscribe to the live firehose for the lifetime of the mounted component. */
export function useLiveUpdates(): void {
  const qc = useQueryClient();

  useEffect(() => {
    let socket: WebSocket | null = null;
    let retry = 0;
    let closed = false;
    let hadGap = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const cache = new LiveEventCacheBatcher(qc);

    const connect = async () => {
      if (closed) return;
      try {
        socket = new WebSocket(await wsUrl());
      } catch {
        if (closed) return;
        hadGap = true;
        setWsConnected(false);
        const delay = Math.min(1000 * 2 ** retry, 15000);
        retry += 1;
        timer = setTimeout(() => void connect(), delay);
        return;
      }
      socket.onopen = () => {
        retry = 0;
        setWsConnected(true);
        if (hadGap) void cache.resync();
        hadGap = false;
      };
      socket.onmessage = (msg) => {
        try {
          void cache.handle(JSON.parse(msg.data) as LibraryEvent);
        } catch {
          /* ignore malformed frames — fail-soft (DESIGN_GUIDELINES §2) */
        }
      };
      socket.onclose = () => {
        setWsConnected(false);
        if (closed) return;
        hadGap = true;
        const delay = Math.min(1000 * 2 ** retry, 15000);
        retry += 1;
        timer = setTimeout(() => void connect(), delay);
      };
      socket.onerror = () => socket?.close();
    };

    void connect();
    return () => {
      closed = true;
      if (timer) clearTimeout(timer);
      cache.dispose();
      socket?.close();
    };
  }, [qc]);
}
