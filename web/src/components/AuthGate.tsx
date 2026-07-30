// Front-door auth (tech-spec 10 §4.2 posture, owner model): one gate at the connection, permissions
// decide after it. The gate wraps the router — in token mode nothing (no workspace, thumbnails, WS,
// media loads) renders or fetches until a credential validates; in anonymous mode the app loads
// read-only (the operator's explicit choice) with a Sign in affordance in the StatusBar; with auth
// off there is no gate at all. The credential is a bearer token, or — with user accounts enabled
// (issue #42) — a username/password session cookie; an unclaimed accounts-on server renders the
// first-run claim screen instead.

import { useState, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, ApiError } from "@/api/client";
import { authApi } from "@/api/auth";
import { useVersion } from "@/api/queries";
import { getServer, resolveUrl, serverLabel, setServer } from "@/lib/server";
import { AUTH_COPY, isUnauthorized, useAuthExpired } from "@/lib/auth";
import { bootDecision } from "@/lib/auth-policy";
import { useFocusTrap } from "@/lib/use-focus-trap";
import { ConnectDialog } from "./ConnectDialog";

/** Does this browser hold (the readable half of) an account session? The session itself is an
 *  HttpOnly cookie we can't see, but its CSRF mirror `dam_csrf` is readable — enough to tell "had a
 *  session that stopped working" (say so) from "never signed in" (a plain form, no scary copy). */
function hasSessionHint(): boolean {
  return document.cookie.includes("dam_csrf=");
}

export function AuthGate({ children }: { children: ReactNode }) {
  const version = useVersion();
  const expired = useAuthExpired();
  const auth = version.data?.auth;
  // User accounts (issue #42): the gate becomes a username/password form (token as fallback), and
  // an unclaimed server gets the first-run claim screen. Accounts force auth to at least "token".
  const accounts = version.data?.accounts === true;
  const unclaimed = version.data?.unclaimed === true;
  const token = getServer().token;

  // Validate a stored credential before mounting the app: in token mode it decides gate-vs-app; in
  // anonymous mode a stale token would 401 every read, which must fall back to the gate (with a
  // read-only escape), not a broken grid. With accounts enabled the credential may be a session
  // cookie instead of a stored token, so the probe runs even token-less (cookies ride along).
  const needsProbe = (auth === "token" || auth === "anonymous") && (!!token || accounts);
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

  // First run of an accounts-on server: nobody *can* sign in until the initial admin is claimed.
  if (accounts && unclaimed) return <ClaimScreen />;

  if (expired)
    return (
      <LoginScreen
        reason={token || hasSessionHint() ? AUTH_COPY.sessionExpired : null}
        allowReadOnly={auth === "anonymous"}
        accounts={accounts}
      />
    );
  if (auth !== "token" && auth !== "anonymous") return <>{children}</>;

  if (needsProbe) {
    if (probe.isPending) return <BootSplash />;
    if (probe.isError && isUnauthorized(probe.error))
      return (
        <LoginScreen
          reason={
            token
              ? AUTH_COPY.tokenRejected
              : hasSessionHint()
                ? AUTH_COPY.sessionExpired
                : null
          }
          allowReadOnly={auth === "anonymous"}
          accounts={accounts}
        />
      );
    return <>{children}</>; // credential valid (any other error is the offline UX's business)
  }
  if (bootDecision(auth, false) === "gate")
    return <LoginScreen reason={null} allowReadOnly={false} accounts={accounts} />;
  return <>{children}</>; // anonymous, signed out — read-only by the operator's choice
}

function BootSplash() {
  return (
    <div className="fixed inset-0 flex items-center justify-center bg-bg">
      <div className="animate-pulse text-sm text-fg-dim">3DAM</div>
    </div>
  );
}

/** The full-screen login gate: the sign-in form over a bare background — no interface, no assets.
 *  It's the app's only surface here, so there's nothing to Escape to — just a focus-trapped dialog.
 *  Accounts-on servers get the username/password form (token as the fallback); otherwise the token
 *  form. */
function LoginScreen({
  reason,
  allowReadOnly,
  accounts,
}: {
  reason: string | null;
  allowReadOnly: boolean;
  accounts: boolean;
}) {
  const ref = useFocusTrap<HTMLDivElement>(true);
  return (
    <div className="fixed inset-0 flex items-center justify-center bg-bg p-4">
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby="login-title"
        className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl"
      >
        {accounts ? (
          <AccountLoginForm reason={reason} allowReadOnly={allowReadOnly} />
        ) : (
          <TokenLoginForm reason={reason} allowReadOnly={allowReadOnly} />
        )}
      </div>
    </div>
  );
}

/** Username/password sign-in (user accounts, issue #42) — shared by the boot gate and the StatusBar
 *  sign-in modal. On success the session rides an HttpOnly cookie: drop any stale stored bearer
 *  token (it would shadow the session with 401s) and reload so every transport restarts signed in.
 *  A small toggle reveals the classic token form for headless/API credentials. */
export function AccountLoginForm({
  reason,
  allowReadOnly,
  onClose,
}: {
  /** Why the user is seeing this (rejected/expired credential), or null for a plain sign-in. */
  reason: string | null;
  /** Offer "Browse read-only" (anonymous-mode servers): enters the app signed out. */
  allowReadOnly: boolean;
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
          <button
            type="button"
            className="mr-auto text-[12px] text-fg-muted hover:underline"
            onClick={() => setTokenMode(true)}
          >
            Use an API token instead…
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
            disabled={!username.trim() || !password || busy}
          >
            {busy ? "Signing in…" : "Sign in"}
          </button>
        </div>
      </form>
    </>
  );
}

/** First-run claim (user accounts, issue #42): an accounts-on server with no admin account yet.
 *  Full-screen like the login gate — nothing else is usable until the library is claimed. The
 *  server only accepts the claim from localhost while unclaimed. */
function ClaimScreen() {
  const ref = useFocusTrap<HTMLDivElement>(true);
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const mismatch = confirm.length > 0 && password !== confirm;
  const canSubmit = !!username.trim() && !!password && password === confirm && !busy;

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
          server runs on; further accounts are created in Settings afterwards.
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

          <div className="flex items-center justify-end">
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
        setError(AUTH_COPY.tokenRejected);
        return;
      }
      if (res.status === 403) {
        setError(AUTH_COPY.lacksRead);
        return;
      }
      // Valid (or the server is mid-hiccup — the app's offline UX owns that): persist and restart
      // every transport with the credential (queries, WS, media `?token=` URLs).
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
