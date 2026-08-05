// First-run claim, extracted from `AuthGate.tsx` (issue #169, parent #96).
//
// Unlike the other two forms this one owns its own full-screen frame rather than being dropped into
// `LoginScreen`'s dialog: it is reached both as the whole boot UI (an accounts-on server with no
// accounts) and as a secondary path from the login screen (the re-opened recovery window, ADR
// 0014), and in both cases nothing else is usable. Keeping the frame here is what lets the gate
// route to it with a bare `<ClaimScreen />`.

import { useState } from "react";
import { ApiError } from "@/api/client";
import { authApi } from "@/api/auth";
import { AUTH_COPY } from "@/lib/auth";
import { getServer, serverLabel, setServer } from "@/lib/server";
import { useFocusTrap } from "@/lib/use-focus-trap";
import { TokenLoginForm } from "./TokenLoginForm";

/** First-run claim (user accounts, issue #42): an accounts-on server with no admin account yet.
 *  Full-screen like the login gate — nothing else is usable until the library is claimed. The
 *  server only accepts the claim from a process on its own machine (or with the bootstrap owner
 *  token), so `onSignIn` is always offered: a remote browser that gets a 403 here must not be
 *  dead-ended, and neither must a user who can simply sign in (a re-opened claim window). */
export function ClaimScreen({ onSignIn }: { onSignIn?: () => void } = {}) {
  const ref = useFocusTrap<HTMLDivElement>(true);
  const [tokenMode, setTokenMode] = useState(false);
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const mismatch = confirm.length > 0 && password !== confirm;
  const canSubmit = !!username.trim() && !!password && password === confirm && !busy;

  // The escape hatch. With accounts already present the caller just wants the login screen
  // (`onSignIn`). With zero accounts there is nobody to sign in *as* — but the claim gate accepts
  // an Admin-scoped bearer, so pasting the bootstrap owner token here and reloading brings the
  // caller back to this form with a credential that claims from anywhere (ADR 0014 gate 2). That is
  // the documented way out of a 403 on a proxied or remote instance.
  if (tokenMode)
    return (
      <div className="fixed inset-0 flex items-center justify-center bg-bg p-4">
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="login-title"
          className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl"
        >
          <p className="mb-3 rounded border border-border bg-surface-2 px-2 py-1.5 text-[12px] text-fg-dim">
            This server can only be claimed from the machine it runs on. To claim it from here,
            paste the <strong>bootstrap owner token</strong> — the server wrote it to{" "}
            <code className="font-mono">bootstrap-owner-token.txt</code> in its data directory and
            logged the path.
          </p>
          <TokenLoginForm reason={null} allowReadOnly={false} />
          <button
            type="button"
            className="mt-3 text-[12px] text-fg-muted hover:underline"
            onClick={() => setTokenMode(false)}
          >
            ← Back to the claim form
          </button>
        </div>
      </div>
    );

  const claim = async () => {
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    try {
      await authApi.claim({
        username: username.trim(),
        password,
        display_name: displayName.trim() || null,
      });
      // Claimed and signed in (session cookie set) — drop any stale token and restart the app.
      setServer(getServer().base, "");
      location.reload();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : AUTH_COPY.unreachable);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 flex items-center justify-center bg-bg p-4">
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby="claim-title"
        className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl"
      >
        <h2 id="claim-title" className="mb-1 font-medium">
          Claim {serverLabel()}
        </h2>
        <p className="mb-3 text-[12px] text-fg-dim">
          Create the first admin account for this library. This works once, from the machine the
          server runs on; further accounts are created in Administration afterwards.
        </p>

        {error && (
          <p role="alert" aria-live="polite" className="mb-2 text-[12px] text-danger">
            {error}
          </p>
        )}

        <form
          onSubmit={(e) => {
            e.preventDefault();
            void claim();
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
          <label className="mb-2 block">
            <span className="mb-1 block text-[11px] text-fg-muted">Password</span>
            <input
              className="field w-full"
              type="password"
              autoComplete="new-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </label>
          <label className="mb-2 block">
            <span className="mb-1 block text-[11px] text-fg-muted">Confirm password</span>
            <input
              className="field w-full"
              type="password"
              autoComplete="new-password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
            />
            {mismatch && (
              <span className="mt-1 block text-[11px] text-danger">Passwords don’t match.</span>
            )}
          </label>
          <label className="mb-3 block">
            <span className="mb-1 block text-[11px] text-fg-muted">Display name (optional)</span>
            <input
              className="field w-full"
              type="text"
              value={displayName}
              onChange={(e) => setDisplayName(e.target.value)}
            />
          </label>

          <div className="flex items-center justify-end gap-2">
            <button
              type="button"
              className="mr-auto text-[12px] text-fg-muted hover:underline"
              onClick={onSignIn ?? (() => setTokenMode(true))}
            >
              {onSignIn ? "← Sign in instead" : "Claiming from another machine?"}
            </button>
            <button
              type="submit"
              className="btn btn-accent disabled:opacity-40"
              disabled={!canSubmit}
            >
              {busy ? "Creating…" : "Create admin account"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
