// Username/password sign-in, extracted from `AuthGate.tsx` (issue #169, parent #96).
//
// The gate hosts it, but so do the StatusBar sign-in modal and the Profile page, and it is the one
// form with a *mode*: `tokenMode` swaps in `TokenLoginForm` for headless/API credentials. That
// dependency runs one way (account form → token form, never back), which is why the two are
// separate modules rather than one "login forms" file — the token form has to be importable on its
// own, and is.

import { useState } from "react";
import { ApiError } from "@/api/client";
import { authApi } from "@/api/auth";
import { AUTH_COPY } from "@/lib/auth";
import { getServer, serverLabel, setServer } from "@/lib/server";
import { TokenLoginForm } from "./TokenLoginForm";

/** Username/password sign-in (user accounts, issue #42) — shared by the boot gate and the StatusBar
 *  sign-in modal. On success the session rides an HttpOnly cookie: drop any stale stored bearer
 *  token (it would shadow the session with 401s) and reload so every transport restarts signed in.
 *  A small toggle reveals the classic token form for headless/API credentials. */
export function AccountLoginForm({
  reason,
  allowReadOnly,
  allowApiToken = true,
  onClose,
}: {
  /** Why the user is seeing this (rejected/expired credential), or null for a plain sign-in. */
  reason: string | null;
  /** Offer "Browse read-only" (anonymous-mode servers): enters the app signed out. */
  allowReadOnly: boolean;
  /** Offer the non-personal API-token alternative. Profile turns this off because a token cannot
   *  create a personal profile or sessions. */
  allowApiToken?: boolean;
  /** Present when hosted in a dismissable modal (StatusBar sign-in) rather than the boot gate. */
  onClose?: () => void;
}) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(reason);
  const [busy, setBusy] = useState(false);
  const [tokenMode, setTokenMode] = useState(false);

  if (tokenMode)
    return (
      <>
        <TokenLoginForm reason={null} allowReadOnly={allowReadOnly} onClose={onClose} />
        <button
          type="button"
          className="mt-3 text-[12px] text-fg-muted hover:underline"
          onClick={() => setTokenMode(false)}
        >
          ← Use a username &amp; password instead
        </button>
      </>
    );

  const signIn = async () => {
    const u = username.trim();
    if (!u || !password || busy) return;
    setBusy(true);
    setError(null);
    try {
      await authApi.login({ username: u, password });
      // Signed in via the session cookie — clear any stale bearer token and restart the app.
      setServer(getServer().base, "");
      location.reload();
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) setError(AUTH_COPY.wrongCredentials);
      else if (e instanceof ApiError && e.status === 429) setError(AUTH_COPY.lockedOut);
      else if (e instanceof ApiError) setError(e.message);
      else setError(AUTH_COPY.unreachable);
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
        This server uses user accounts. Sign in with the username and password its operator gave
        you.
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
        <label className="mb-2 block">
          <span className="mb-1 block text-[11px] text-fg-muted">Username</span>
          <input
            className="field w-full"
            type="text"
            autoComplete="username"
            spellCheck={false}
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            autoFocus
          />
        </label>
        <label className="mb-3 block">
          <span className="mb-1 block text-[11px] text-fg-muted">Password</span>
          <input
            className="field w-full"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </label>

        <div className="flex items-center justify-end gap-2">
          {allowApiToken && (
            <button
              type="button"
              className="mr-auto text-[12px] text-fg-muted hover:underline"
              onClick={() => setTokenMode(true)}
            >
              Use an API token instead…
            </button>
          )}
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
            disabled={!username.trim() || !password || busy}
          >
            {busy ? "Signing in…" : "Sign in"}
          </button>
        </div>
      </form>
    </>
  );
}
