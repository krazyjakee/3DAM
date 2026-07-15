// Front-door auth (tech-spec 10 §4.2 posture, owner model): one gate at the connection, permissions
// decide after it. The gate wraps the router — in token mode nothing (no workspace, thumbnails, WS,
// media loads) renders or fetches until a credential validates; in anonymous mode the app loads
// read-only (the operator's explicit choice) with a Sign in affordance in the StatusBar; with auth
// off there is no gate at all. Today the credential is a bearer token; the form is where an
// account/password login slots in later (milestone 6).

import { useState, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@/api/client";
import { useVersion } from "@/api/queries";
import { getServer, resolveUrl, serverLabel, setServer } from "@/lib/server";
import { isUnauthorized, useAuthExpired } from "@/lib/auth";
import { bootDecision } from "@/lib/auth-policy";
import { ConnectDialog } from "./ConnectDialog";

export function AuthGate({ children }: { children: ReactNode }) {
  const version = useVersion();
  const expired = useAuthExpired();
  const auth = version.data?.auth;
  const token = getServer().token;

  // Validate a stored credential before mounting the app: in token mode it decides gate-vs-app; in
  // anonymous mode a stale token would 401 every read, which must fall back to the gate (with a
  // read-only escape), not a broken grid.
  const needsProbe = !!token && (auth === "token" || auth === "anonymous");
  const probe = useQuery({
    queryKey: ["auth-probe", token],
    queryFn: () => api.stats(),
    enabled: needsProbe,
    retry: false,
    staleTime: Infinity,
  });

  // Never render the workspace before the posture is known — a token-mode server must not see a
  // burst of unauthenticated fetches from its own UI.
  if (version.isPending) return <BootSplash />;
  // Unreachable server: the app's offline UX owns messaging; this is an auth gate, not an offline mode.
  if (version.isError) return <>{children}</>;

  if (expired)
    return (
      <LoginScreen
        reason="Your session token is no longer valid."
        allowReadOnly={auth === "anonymous"}
      />
    );
  if (auth !== "token" && auth !== "anonymous") return <>{children}</>;

  if (needsProbe) {
    if (probe.isPending) return <BootSplash />;
    if (probe.isError && isUnauthorized(probe.error))
      return (
        <LoginScreen
          reason="The saved token was rejected by this server."
          allowReadOnly={auth === "anonymous"}
        />
      );
    return <>{children}</>; // token valid (any other error is the offline UX's business)
  }
  if (bootDecision(auth, false) === "gate") return <LoginScreen reason={null} allowReadOnly={false} />;
  return <>{children}</>; // anonymous, signed out — read-only by the operator's choice
}

function BootSplash() {
  return (
    <div className="fixed inset-0 flex items-center justify-center bg-bg">
      <div className="animate-pulse text-sm text-fg-dim">3DAM</div>
    </div>
  );
}

/** The full-screen login gate: the token form over a bare background — no interface, no assets. */
function LoginScreen({ reason, allowReadOnly }: { reason: string | null; allowReadOnly: boolean }) {
  return (
    <div className="fixed inset-0 flex items-center justify-center bg-bg p-4">
      <div className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl">
        <TokenLoginForm reason={reason} allowReadOnly={allowReadOnly} />
      </div>
    </div>
  );
}

/** The token sign-in form — shared by the full-screen gate and the StatusBar sign-in modal. The
 *  token is validated against the server before it is persisted, so a typo lands back here with a
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
        setError("Token rejected — check it and try again.");
        return;
      }
      if (res.status === 403) {
        setError("Token accepted but lacks the read scope needed to browse.");
        return;
      }
      // Valid (or the server is mid-hiccup — the app's offline UX owns that): persist and restart
      // every transport with the credential (queries, WS, media `?token=` URLs).
      setServer(getServer().base, t);
      location.reload();
    } catch {
      setError("Could not reach the server.");
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
      <h2 className="mb-1 font-medium">Sign in to {serverLabel()}</h2>
      <p className="mb-3 text-[12px] text-fg-dim">
        This server requires a token. Paste one issued by its operator (
        <code className="font-mono">3dam admin token add</code>).
      </p>

      {error && <p className="mb-2 text-[12px] text-danger">{error}</p>}

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
