// Connect-to-server dialog (hosted mode, issue #74): point the web client at an arbitrary 3DAM
// server, or return to the origin that served it. Mirrors the native GUI's Connect dialog (#70).
//
// Switching backends changes the base URL every transport (REST/WS/media) resolves against, so the
// simplest correct reset is a full reload — TanStack Query, the WebSocket, and all element `src`s
// re-initialise against the new server rather than mixing caches from two backends. Before we commit
// to that reload, we probe the target so a typo lands back here with an inline message instead of a
// broken, reloading app (mirrors TokenLoginForm's validate-before-persist).

import { useState } from "react";
import { clearServer, getServer, isRemote, setServer } from "@/lib/server";
import { AUTH_COPY } from "@/lib/auth";
import { useEscape, useFocusTrap } from "@/lib/use-focus-trap";

/** Normalise a typed server address into a base URL, or return an error string. Auto-prepends
 *  `https://` when no scheme is given, trims, strips a trailing slash, and rejects anything that
 *  isn't a parseable http(s) origin. */
function normalizeBase(raw: string): { base: string } | { error: string } {
  const trimmed = raw.trim();
  if (!trimmed) return { error: "Enter a server address." };
  const withScheme = /^https?:\/\//i.test(trimmed) ? trimmed : `https://${trimmed}`;
  let url: URL;
  try {
    url = new URL(withScheme);
  } catch {
    return { error: "That doesn't look like a valid server address." };
  }
  if (url.protocol !== "http:" && url.protocol !== "https:")
    return { error: "The address must be an http:// or https:// URL." };
  if (!url.host) return { error: "That doesn't look like a valid server address." };
  // Keep an explicit path (reverse-proxy mounts), drop a trailing slash to match server.ts.
  return { base: url.toString().replace(/\/$/, "") };
}

export function ConnectDialog({ onClose }: { onClose: () => void }) {
  const current = getServer();
  const [base, setBase] = useState(current.base);
  const [token, setToken] = useState(current.token);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const ref = useFocusTrap<HTMLDivElement>(true);
  useEscape(onClose);

  const connect = async () => {
    if (busy) return;
    setError(null);
    const norm = normalizeBase(base);
    if ("error" in norm) {
      setError(norm.error);
      return;
    }
    setBusy(true);
    try {
      // Probe the target before committing to the reload. `/api/version` needs no auth and reveals
      // the server's posture; a token-gated server that 401s here is still reachable (the AuthGate /
      // sign-in owns the credential afterward), so only a network failure is a hard "can't reach".
      const headers: Record<string, string> = { accept: "application/json" };
      if (token.trim()) headers.authorization = `Bearer ${token.trim()}`;
      const res = await fetch(`${norm.base}/api/version`, { headers });
      if (!res.ok && res.status !== 401 && res.status !== 403) {
        setError(`Server responded ${res.status} — check the address.`);
        return;
      }
      // Reachable: persist and restart every transport against the new backend.
      setServer(norm.base, token);
      location.reload();
    } catch {
      setError(AUTH_COPY.unreachable);
    } finally {
      setBusy(false);
    }
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
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby="connect-title"
        className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id="connect-title" className="mb-1 font-medium">
          Connect to server
        </h2>
        <p className="mb-3 text-[12px] text-fg-dim">
          Point this client at a remote 3DAM server, or use the one that served this page.
        </p>

        {error && (
          <p role="alert" aria-live="polite" className="mb-2 text-[12px] text-danger">
            {error}
          </p>
        )}

        <form
          onSubmit={(e) => {
            e.preventDefault();
            void connect();
          }}
        >
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
              <button type="button" className="btn mr-auto" onClick={useLocal}>
                Use this server
              </button>
            )}
            <button type="button" className="btn" onClick={onClose}>
              Cancel
            </button>
            <button
              type="submit"
              className="btn btn-accent disabled:opacity-40"
              disabled={!base.trim() || busy}
            >
              {busy ? "Connecting…" : "Connect"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
