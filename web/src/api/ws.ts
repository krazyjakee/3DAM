// Live-update transport (tech-spec 09 §A.3). One WebSocket to `/api/ws` carries the file-03
// `LibraryEvent` firehose; on each event we invalidate the affected TanStack Query caches so the
// grid, stats, sources, and jobs stay live without polling. Auto-reconnects with backoff.

import { useEffect, useSyncExternalStore } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { LibraryEvent } from "./types";
import { qk } from "./queries";

function wsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/api/v1/ws`;
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
    let timer: ReturnType<typeof setTimeout> | undefined;

    const onEvent = (ev: LibraryEvent) => {
      switch (ev.type) {
        case "asset_added":
        case "asset_removed":
        case "asset_changed":
          qc.invalidateQueries({ queryKey: qk.assets });
          qc.invalidateQueries({ queryKey: qk.stats });
          qc.invalidateQueries({ queryKey: qk.duplicates });
          // The open Inspector reads a single asset under ["asset", id] and its neighbours under
          // ["similar", id] — distinct roots from the grid's ["assets"]. Invalidate them too so an
          // analyze/convert that mutates the inspected asset refreshes the panel without a reload.
          qc.invalidateQueries({ queryKey: ["asset"] });
          qc.invalidateQueries({ queryKey: ["similar"] });
          break;
        case "source_state":
          qc.invalidateQueries({ queryKey: qk.sources });
          break;
        case "catalog_reset":
          // A maintenance wipe/factory-reset cleared the whole catalog — invalidate everything so
          // open grids/inspectors empty live rather than showing stale rows.
          qc.invalidateQueries({ queryKey: qk.assets });
          qc.invalidateQueries({ queryKey: qk.stats });
          qc.invalidateQueries({ queryKey: qk.sources });
          qc.invalidateQueries({ queryKey: qk.duplicates });
          qc.invalidateQueries({ queryKey: qk.jobs });
          qc.invalidateQueries({ queryKey: ["asset"] });
          qc.invalidateQueries({ queryKey: ["similar"] });
          break;
        case "job_progress":
          qc.invalidateQueries({ queryKey: qk.jobs });
          // A finished job (scan, analyze, convert…) changes the catalogue — refresh the grid +
          // counts, plus the inspected asset and its neighbours so an analyze run surfaces its new
          // attributes / tags / similarity results in the open Inspector without a manual reload.
          if (ev.state === "done") {
            qc.invalidateQueries({ queryKey: qk.assets });
            qc.invalidateQueries({ queryKey: qk.stats });
            qc.invalidateQueries({ queryKey: qk.sources });
            qc.invalidateQueries({ queryKey: qk.duplicates });
            qc.invalidateQueries({ queryKey: ["asset"] });
            qc.invalidateQueries({ queryKey: ["similar"] });
          }
          break;
      }
    };

    const connect = () => {
      if (closed) return;
      socket = new WebSocket(wsUrl());
      socket.onopen = () => {
        retry = 0;
        setWsConnected(true);
      };
      socket.onmessage = (msg) => {
        try {
          onEvent(JSON.parse(msg.data) as LibraryEvent);
        } catch {
          /* ignore malformed frames — fail-soft (DESIGN_GUIDELINES §2) */
        }
      };
      socket.onclose = () => {
        setWsConnected(false);
        if (closed) return;
        const delay = Math.min(1000 * 2 ** retry, 15000);
        retry += 1;
        timer = setTimeout(connect, delay);
      };
      socket.onerror = () => socket?.close();
    };

    connect();
    return () => {
      closed = true;
      if (timer) clearTimeout(timer);
      socket?.close();
    };
  }, [qc]);
}
