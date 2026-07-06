// Live-update transport (tech-spec 09 §A.3). One WebSocket to `/api/ws` carries the file-03
// `LibraryEvent` firehose; on each event we invalidate the affected TanStack Query caches so the
// grid, stats, sources, and jobs stay live without polling. Auto-reconnects with backoff.

import { useEffect } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { LibraryEvent } from "./types";
import { qk } from "./queries";

function wsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/api/v1/ws`;
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
          break;
        case "source_state":
          qc.invalidateQueries({ queryKey: qk.sources });
          break;
        case "job_progress":
          qc.invalidateQueries({ queryKey: qk.jobs });
          // A finished scan changes the catalogue — refresh the grid + counts.
          if (ev.state === "done") {
            qc.invalidateQueries({ queryKey: qk.assets });
            qc.invalidateQueries({ queryKey: qk.stats });
            qc.invalidateQueries({ queryKey: qk.sources });
          }
          break;
      }
    };

    const connect = () => {
      if (closed) return;
      socket = new WebSocket(wsUrl());
      socket.onopen = () => {
        retry = 0;
      };
      socket.onmessage = (msg) => {
        try {
          onEvent(JSON.parse(msg.data) as LibraryEvent);
        } catch {
          /* ignore malformed frames — fail-soft (DESIGN_GUIDELINES §2) */
        }
      };
      socket.onclose = () => {
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
