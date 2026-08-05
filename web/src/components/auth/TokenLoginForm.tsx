// The token sign-in form, extracted from `AuthGate.tsx` (issue #169, parent #96).
//
// This is the sign-in form with the most hosts: the full-screen boot gate, the StatusBar sign-in
// modal, `AccountLoginForm`'s "use an API token instead" mode, `ClaimScreen`'s claim-from-another-
// machine hatch, and Administration's token swap. Four of those five are outside the gate, which is
// what argues hardest for its own module — a form imported from `AuthGate` reads as if signing in
// were the gate's private business, when the gate is only one of its callers.
//
// Nothing here is shared with the other two forms: each owns its own credential fields and its own
// error copy, so the split is by form, not by field.

import { useState } from "react";
import { AUTH_COPY } from "@/lib/auth";
import { getServer, resolveUrl, serverLabel, setServer } from "@/lib/server";
import { ConnectDialog } from "../ConnectDialog";

/** The token sign-in form — shared by the full-screen gate and the StatusBar sign-in modal. The
 *  token is validated against the server before it is retained for this tab, so a typo lands back here with a
 *  message instead of in a broken, reloading app. */
export function TokenLoginForm({
  reason,
  allowReadOnly,
  onClose,
}: {
  /** Why the user is seeing this (rejected/expired token), or null for a plain sign-in. */
  reason: string | null;
  /** Offer "Browse read-only" (anonymous-mode servers): drops the stored token and enters the app. */
  allowReadOnly: boolean;
  /** Present when hosted in a dismissable modal (StatusBar sign-in) rather than the boot gate. */
  onClose?: () => void;
}) {
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(reason);
  const [busy, setBusy] = useState(false);
  const [connectOpen, setConnectOpen] = useState(false);

  const signIn = async () => {
    const t = token.trim();
    if (!t || busy) return;
    setBusy(true);
    setError(null);
    try {
      const res = await fetch(resolveUrl("/api/v1/stats"), {
        headers: { accept: "application/json", authorization: `Bearer ${t}` },
      });
      if (res.status === 401) {
        setError(AUTH_COPY.tokenRejected);
        return;
      }
      if (res.status === 403) {
        setError(AUTH_COPY.lacksRead);
        return;
      }
      // Valid (or the server is mid-hiccup — the app's offline UX owns that): retain it for this tab
      // and restart every transport. No URL or durable cache contains the credential.
      setServer(getServer().base, t);
      location.reload();
    } catch {
      setError(AUTH_COPY.unreachable);
    } finally {
      setBusy(false);
    }
  };

  const browseReadOnly = () => {
    setServer(getServer().base, "");
    location.reload();
  };

  return (
    <>
      <h2 id="login-title" className="mb-1 font-medium">
        Sign in to {serverLabel()}
      </h2>
      <p className="mb-3 text-[12px] text-fg-dim">
        This server requires a token. Paste one issued by its operator (
        <code className="font-mono">3dam admin token add</code>).
      </p>
      <p className="mb-3 rounded border border-border bg-surface-2 px-2 py-1.5 text-[11px] text-fg-dim">
        Browser tokens last only for this tab and are forgotten when it closes. Use Sign out to
        forget one sooner. The desktop app stores command-line connection tokens in the OS keychain.
      </p>

      {error && (
        <p role="alert" aria-live="polite" className="mb-2 text-[12px] text-danger">
          {error}
        </p>
      )}

      <form
        onSubmit={(e) => {
          e.preventDefault();
          void signIn();
        }}
      >
        <label className="mb-3 block">
          <span className="mb-1 block text-[11px] text-fg-muted">Token</span>
          <input
            className="field w-full"
            type="password"
            placeholder="dam_…"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoFocus
          />
        </label>

        <div className="flex items-center justify-end gap-2">
          <button
            type="button"
            className="mr-auto text-[12px] text-fg-muted hover:underline"
            onClick={() => setConnectOpen(true)}
          >
            Connect to a different server…
          </button>
          {allowReadOnly && (
            <button type="button" className="btn" onClick={browseReadOnly}>
              Browse read-only
            </button>
          )}
          {onClose && (
            <button type="button" className="btn" onClick={onClose}>
              Cancel
            </button>
          )}
          <button
            type="submit"
            className="btn btn-accent disabled:opacity-40"
            disabled={!token.trim() || busy}
          >
            {busy ? "Signing in…" : "Sign in"}
          </button>
        </div>
      </form>

      {connectOpen && <ConnectDialog onClose={() => setConnectOpen(false)} />}
    </>
  );
}
