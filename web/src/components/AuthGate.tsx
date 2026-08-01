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
import { getServer, isRemote, resolveUrl, serverLabel, setServer } from "@/lib/server";
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

/** Where the "sign in with your identity provider" link points (issue #41).
 *
 *  `return_to` carries the path the user was on so the callback lands them back there rather than
 *  dumping them at the root. It is a *path*, never a URL: the server refuses anything else, because
 *  a login link a stranger sends must not be able to choose where you end up afterwards. Resolved
 *  against the configured server for the same reason every other API call is, though the button is
 *  only offered same-origin. */
function oidcStartUrl(): string {
  const returnTo = `${window.location.pathname}${window.location.search}`;
  return resolveUrl(`/api/v1/auth/oidc/start?return_to=${encodeURIComponent(returnTo)}`);
}

export function AuthGate({ children }: { children: ReactNode }) {
  const version = useVersion();
  const expired = useAuthExpired();
  const auth = version.data?.auth;
  // User accounts (issue #42): the gate becomes a username/password form (token as fallback), and
  // an unclaimed server gets the first-run claim screen. Accounts force auth to at least "token".
  const accounts = version.data?.accounts === true;
  // Single sign-on (issue #41). The server only reports this true when the flag is on, accounts are
  // on, *and* a provider is configured — so a button rendered from it always leads somewhere real.
  const oidc = version.data?.oidc === true;
  const unclaimed = version.data?.unclaimed === true;
  // Zero accounts = a genuinely un-claimed instance. A *re-opened* window (the lost-sole-admin
  // recovery hatch, ADR 0014) also reports `unclaimed`, but every existing user's password still
  // works — so the claim screen must not be exclusive there.
  const noAccounts = (version.data?.account_count ?? 0) === 0;
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

  // First run of an accounts-on server: nobody *can* sign in until the initial admin is claimed, so
  // the claim screen is the whole UI. Only when the instance truly has no accounts — a re-opened
  // claim window (recovery) leaves every editor and viewer able to sign in normally, and pinning
  // them all to a claim form would be the recovery hatch locking the building.
  if (accounts && unclaimed && noAccounts) return <ClaimScreen />;

  const claimable = accounts && unclaimed;
  if (expired)
    return (
      <LoginScreen
        reason={token || hasSessionHint() ? AUTH_COPY.sessionExpired : null}
        allowReadOnly={auth === "anonymous"}
        accounts={accounts}
        oidc={oidc}
        claimable={claimable}
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
          oidc={oidc}
          claimable={claimable}
        />
      );
    return <>{children}</>; // credential valid (any other error is the offline UX's business)
  }
  if (bootDecision(auth, false) === "gate")
    return (
      <LoginScreen
        reason={null}
        allowReadOnly={false}
        accounts={accounts}
        oidc={oidc}
        claimable={claimable}
      />
    );
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
 *  form.
 *
 *  `claimable` (an accounts-on server whose claim window is open, but which already has accounts —
 *  the re-opened recovery window) adds a secondary path to the claim form, so recovery is reachable
 *  without evicting everyone who can still sign in. */
function LoginScreen({
  reason,
  allowReadOnly,
  accounts,
  oidc = false,
  claimable = false,
}: {
  reason: string | null;
  allowReadOnly: boolean;
  accounts: boolean;
  oidc?: boolean;
  claimable?: boolean;
}) {
  const ref = useFocusTrap<HTMLDivElement>(true);
  const [claiming, setClaiming] = useState(false);

  if (claiming) return <ClaimScreen onSignIn={() => setClaiming(false)} />;

  // Account sign-in only works same-origin. The session is an HttpOnly, host-only cookie and the
  // server sends `Access-Control-Allow-Origin: *`, which can never carry credentials — so a
  // cross-origin POST would 200, the browser would discard the Set-Cookie, and the reloaded app
  // would land right back on this form with no error. Better to say so.
  // (The cookie is `SameSite=Lax` since issue #41 — the OIDC callback is a cross-site navigation
  // and `Strict` would withhold it there. That does not change this: the blocker here is the CORS
  // wildcard, and cross-site *POSTs* carry no cookie under `Lax` either.)
  const remote = isRemote();
  return (
    <div className="fixed inset-0 flex items-center justify-center bg-bg p-4">
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby="login-title"
        className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl"
      >
        {accounts && remote && <RemoteAccountsNotice />}
        {accounts && !remote ? (
          <AccountLoginForm reason={reason} allowReadOnly={allowReadOnly} />
        ) : (
          <TokenLoginForm reason={reason} allowReadOnly={allowReadOnly} />
        )}
        {/* Single sign-on (issue #41). Hidden when pointed at a *remote* server for exactly the
            reason account sign-in is: the callback sets a host-only session cookie on the server's
            origin, which this origin then cannot use — the button would appear to work and change
            nothing. A plain link, not a fetch: the whole point is a top-level navigation the
            browser follows to the provider and back. */}
        {oidc && !remote && (
          <>
            <div className="my-3 flex items-center gap-2 text-[12px] text-fg-dim">
              <span className="h-px flex-1 bg-border" />
              or
              <span className="h-px flex-1 bg-border" />
            </div>
            <a className="btn w-full justify-center" href={oidcStartUrl()}>
              Sign in with your identity provider
            </a>
          </>
        )}
        {claimable && (
          <button
            type="button"
            className="mt-3 text-[12px] text-fg-muted hover:underline"
            onClick={() => setClaiming(true)}
          >
            Claim this server (create a new admin account)…
          </button>
        )}
      </div>
    </div>
  );
}

/** Shown above the token form when the client is pointed at a *remote* server that uses accounts.
 *  Username/password sign-in is same-origin only (see `LoginScreen`), so the honest instruction is
 *  "open the server directly"; an API token still works cross-origin and is offered below. */
function RemoteAccountsNotice() {
  const base = getServer().base;
  return (
    <p className="mb-3 rounded border border-border bg-surface-2 px-2 py-1.5 text-[12px] text-fg-dim">
      This server uses user accounts, and account sign-in only works when the app is served by the
      server itself (the session cookie can't cross origins). Open{" "}
      <a className="text-accent hover:underline" href={base} rel="noreferrer">
        {base}
      </a>{" "}
      directly to sign in — or paste an API token below to keep browsing from here.
    </p>
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
 *  server only accepts the claim from a process on its own machine (or with the bootstrap owner
 *  token), so `onSignIn` is always offered: a remote browser that gets a 403 here must not be
 *  dead-ended, and neither must a user who can simply sign in (a re-opened claim window). */
function ClaimScreen({ onSignIn }: { onSignIn?: () => void } = {}) {
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
