// Front-door auth (tech-spec 10 §4.2 posture, owner model): one gate at the connection, permissions
// decide after it. The gate wraps the router — in token mode nothing (no workspace, thumbnails, WS,
// media loads) renders or fetches until a credential validates; in anonymous mode the app loads
// read-only (the operator's explicit choice) with a Sign in affordance in the StatusBar; with auth
// off there is no gate at all. The credential is a bearer token, or — with user accounts enabled
// (issue #42) — a username/password session cookie; an unclaimed accounts-on server renders the
// first-run claim screen instead.
//
// The three sign-in forms themselves live in `./auth/` (issue #169). What is left here is the
// *routing*: which of boot splash, claim screen, login screen, or the app itself a given posture
// gets. The forms are re-exported below because the gate stays the app's one named entry point for
// signing in — the StatusBar modal, Administration's token swap, and Profile each host a form
// outside the gate, and importing it from here says "this is the same sign-in" rather than making
// every surface reach into a directory of forms.

import { useState, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "@/api/client";
import { useVersion } from "@/api/queries";
import {
  getServer,
  hasBearerCredential,
  isRemote,
  resolveUrl,
  takeCredentialMigrationNotice,
} from "@/lib/server";
import { AUTH_COPY, isUnauthorized, useAuthExpired } from "@/lib/auth";
import { bootDecision } from "@/lib/auth-policy";
import { useFocusTrap } from "@/lib/use-focus-trap";
import { AccountLoginForm } from "./auth/AccountLoginForm";
import { ClaimScreen } from "./auth/ClaimScreen";
import { TokenLoginForm } from "./auth/TokenLoginForm";

export { AccountLoginForm } from "./auth/AccountLoginForm";
export { TokenLoginForm } from "./auth/TokenLoginForm";

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
export function oidcStartUrl(): string {
  const returnTo = `${window.location.pathname}${window.location.search}`;
  return resolveUrl(`/api/v1/auth/oidc/start?return_to=${encodeURIComponent(returnTo)}`);
}

export function AuthGate({ children }: { children: ReactNode }) {
  const [credentialMigrated] = useState(takeCredentialMigrationNotice);
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
  const hasBearer = hasBearerCredential();

  // Validate a stored credential before mounting the app: in token mode it decides gate-vs-app; in
  // anonymous mode a stale token would 401 every read, which must fall back to the gate (with a
  // read-only escape), not a broken grid. With accounts enabled the credential may be a session
  // cookie instead of a stored token, so the probe runs even token-less (cookies ride along).
  const needsProbe = (auth === "token" || auth === "anonymous") && (hasBearer || accounts);
  const probe = useQuery({
    // Never put the bearer itself in a query/cache key: diagnostics and devtools inspect keys.
    queryKey: ["auth-probe", getServer().base, hasBearer ? "bearer" : "session"],
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
        reason={
          credentialMigrated
            ? "A previously saved browser token was removed for security. Paste it again to continue."
            : hasBearer || hasSessionHint()
              ? AUTH_COPY.sessionExpired
              : null
        }
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
            credentialMigrated
              ? "A previously saved browser token was removed for security. Paste it again to continue."
              : hasBearer
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
        reason={
          credentialMigrated
            ? "A previously saved browser token was removed for security. Paste it again to continue."
            : null
        }
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
