// Connect-to-server dialog (hosted mode, issue #74): point the web client at an arbitrary 3DAM
// server, or return to the origin that served it. Mirrors the native GUI's Connect dialog (#70).
//
// Switching backends changes the base URL every transport (REST/WS/media) resolves against, so the
// simplest correct reset is a full reload — TanStack Query, the WebSocket, and all element `src`s
// re-initialise against the new server rather than mixing caches from two backends.

import { useState } from "react";
import { clearServer, getServer, isRemote, setServer } from "@/lib/server";

export function ConnectDialog({ onClose }: { onClose: () => void }) {
  const current = getServer();
  const [base, setBase] = useState(current.base);
  const [token, setToken] = useState(current.token);

  const connect = () => {
    if (!base.trim()) return;
    setServer(base, token);
    location.reload();
  };
  const useLocal = () => {
    clearServer();
    location.reload();
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      onClick={onClose}
    >
      <div
        className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="mb-1 font-medium">Connect to server</h2>
        <p className="mb-3 text-[12px] text-fg-dim">
          Point this client at a remote 3DAM server, or use the one that served this page.
        </p>

        <label className="mb-2 block">
          <span className="mb-1 block text-[11px] text-fg-muted">Server URL</span>
          <input
            className="field w-full"
            placeholder="https://host:7878"
            value={base}
            onChange={(e) => setBase(e.target.value)}
            autoFocus
          />
        </label>
        <label className="mb-3 block">
          <span className="mb-1 block text-[11px] text-fg-muted">
            Token <span className="text-fg-dim">(required for token-gated servers)</span>
          </span>
          <input
            className="field w-full"
            type="password"
            placeholder="optional"
            value={token}
            onChange={(e) => setToken(e.target.value)}
          />
        </label>

        <div className="flex items-center justify-end gap-2">
          {isRemote() && (
            <button className="btn mr-auto" onClick={useLocal}>
              Use this server
            </button>
          )}
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn-accent" onClick={connect} disabled={!base.trim()}>
            Connect
          </button>
        </div>
      </div>
    </div>
  );
}
