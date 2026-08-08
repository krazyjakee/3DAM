// Personal account identity and session management. This surface deliberately uses only the
// self-service `/api/v1/auth/*` routes; server-wide configuration belongs to Administration.

import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router";
import { LogOut, ShieldCheck, UserRound } from "lucide-react";
import { authApi } from "@/api/auth";
import type { AccountRef, SessionInfo } from "@/api/types";
import { useVersion, useWhoami } from "@/api/queries";
import {
  ACCOUNT_ROUTES,
  profileAccessState,
  type ProfileAccessState,
} from "@/lib/account-surfaces";
import { clearToken, getServer, isRemote } from "@/lib/server";
import { useDialogs } from "@/lib/dialogs";
import { errorMessage, toast } from "@/lib/toast";
import { oidcStartUrl } from "./AuthGate";
import { AccountLoginForm } from "./auth/AccountLoginForm";

const ROLE_LABEL: Record<AccountRef["role"], string> = {
  admin: "Administrator",
  editor: "Editor",
  viewer: "Viewer",
};

export function Profile() {
  const version = useVersion();
  const whoami = useWhoami();

  if (version.isPending || (version.data?.accounts === true && whoami.isPending)) {
    return <ProfileFrame state="checking" />;
  }

  if (version.isError) {
    return <ProfileFrame state="error" message={errorMessage(version.error)} />;
  }

  const accountsEnabled = version.data?.accounts === true;
  if (accountsEnabled && whoami.isError) {
    return <ProfileFrame state="error" message={errorMessage(whoami.error)} />;
  }

  const account = whoami.data?.account ?? null;
  const state = profileAccessState(accountsEnabled, account !== null);
  if (state !== "account" || !account) {
    return (
      <ProfileFrame
        state={state}
        oidc={version.data?.oidc === true}
      />
    );
  }

  return (
    <ProfileFrame state="account">
      <IdentityCard
        account={account}
        restricted={whoami.data?.restricted === true}
      />
      <MySessionsSection />
    </ProfileFrame>
  );
}

function ProfileFrame({
  state,
  message,
  oidc = false,
  children,
}: {
  state: ProfileAccessState | "checking" | "error";
  message?: string;
  oidc?: boolean;
  children?: React.ReactNode;
}) {
  return (
    <div className="mx-auto flex min-h-dvh max-w-3xl flex-col gap-6 p-4 text-sm sm:p-6">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="text-lg font-semibold">Profile</h1>
        <Link to="/" className="text-accent hover:underline">
          ← Back to library
        </Link>
      </header>

      {state === "checking" && (
        <div className="rounded border border-border p-4 text-fg-dim" role="status">
          Checking account…
        </div>
      )}

      {state === "error" && (
        <div
          className="rounded border border-danger/40 bg-danger/10 p-4 text-danger"
          role="alert"
        >
          <p className="font-medium">Profile unavailable</p>
          <p className="mt-1 text-xs">{message}</p>
        </div>
      )}

      {state === "unavailable" && (
        <div className="rounded border border-border bg-surface p-4">
          <p className="font-medium">Personal accounts aren’t enabled</p>
          <p className="mt-1 text-xs text-fg-dim">
            This library has no personal profile or sessions for you to manage. You can continue
            browsing with the server’s current access posture.
          </p>
        </div>
      )}

      {state === "sign-in" && <ProfileSignIn oidc={oidc} />}
      {children}
    </div>
  );
}

function ProfileSignIn({ oidc }: { oidc: boolean }) {
  if (isRemote()) {
    const base = getServer().base;
    return (
      <div className="rounded border border-border bg-surface p-4">
        <p className="font-medium">Sign in to a personal account</p>
        <p className="mt-1 text-xs text-fg-dim">
          You’re connected with a non-personal credential. Account sessions can only be created on
          the server’s own origin because their secure cookie cannot cross origins.
        </p>
        <a
          className="mt-3 inline-flex text-accent hover:underline"
          href={`${base}${ACCOUNT_ROUTES.profile}`}
          rel="noreferrer"
        >
          Open the server to sign in
        </a>
      </div>
    );
  }

  return (
    <div className="rounded border border-border bg-surface p-4">
      <AccountLoginForm reason={null} allowReadOnly={false} allowApiToken={false} />
      {oidc && (
        <>
          <div className="my-3 flex items-center gap-2 text-xs text-fg-dim">
            <span className="h-px flex-1 bg-border" />
            or
            <span className="h-px flex-1 bg-border" />
          </div>
          <a className="btn w-full justify-center" href={oidcStartUrl()}>
            Sign in with your identity provider
          </a>
        </>
      )}
    </div>
  );
}

function IdentityCard({ account, restricted }: { account: AccountRef; restricted: boolean }) {
  const [signingOut, setSigningOut] = useState(false);

  const signOut = async () => {
    if (signingOut) return;
    setSigningOut(true);
    try {
      await authApi.logout();
    } catch {
      // Still clear any browser bearer and reload. The next auth probe is the authority on whether
      // this session remains valid, matching the global identity chip's fail-safe sign-out.
    } finally {
      clearToken();
      location.reload();
    }
  };

  return (
    <section className="rounded border border-border bg-surface p-4">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="flex min-w-0 items-center gap-3">
          <span className="rounded-full bg-accent-muted p-2 text-accent" aria-hidden="true">
            <UserRound size={20} />
          </span>
          <div className="min-w-0">
            <h2 className="truncate font-medium">{account.username}</h2>
            <p className="flex items-center gap-1 text-xs text-fg-dim">
              <ShieldCheck size={12} /> {ROLE_LABEL[account.role]}
              {restricted ? " · Shared content only" : ""}
            </p>
          </div>
        </div>
        <button
          type="button"
          className="btn text-danger disabled:opacity-40"
          disabled={signingOut}
          onClick={() => void signOut()}
        >
          <LogOut size={13} /> {signingOut ? "Signing out…" : "Sign out"}
        </button>
      </div>
      <dl className="mt-4 grid gap-2 text-xs sm:grid-cols-2">
        <div>
          <dt className="text-fg-dim">Username</dt>
          <dd>{account.username}</dd>
        </div>
        <div>
          <dt className="text-fg-dim">Role</dt>
          <dd>{ROLE_LABEL[account.role]}</dd>
        </div>
      </dl>
    </section>
  );
}

/** Every browser/device holding one of the signed-in account's sessions, scoped server-side to the
 * current account. Editors and viewers use the same endpoint as administrators. */
function MySessionsSection() {
  const { confirm } = useDialogs();
  const [sessions, setSessions] = useState<SessionInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setSessions(await authApi.sessions());
      setError(null);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const revoke = async (session: SessionInfo) => {
    if (
      !(await confirm({
        title: session.current ? "Sign out this session?" : "Revoke this session?",
        message: session.current
          ? "This is the session you’re using. This browser must sign in again."
          : "That browser or device loses access immediately and must sign in again.",
        danger: true,
        confirmLabel: session.current ? "Sign out" : "Revoke session",
      }))
    )
      return;

    setRevoking(session.session_id);
    try {
      await authApi.revokeSession(session.session_id);
      if (session.current) {
        clearToken();
        location.reload();
        return;
      }
      await load();
      toast.success("Session revoked");
    } catch (cause) {
      toast.error(errorMessage(cause));
    } finally {
      setRevoking(null);
    }
  };

  return (
    <section className="flex flex-col gap-2">
      <div>
        <h2 className="font-medium text-fg-muted">My sessions</h2>
        <p className="text-xs text-fg-dim">
          Browsers and devices currently signed in to your account.
        </p>
      </div>
      {!sessions && !error && (
        <div className="rounded border border-border px-3 py-2 text-fg-dim" role="status">
          Loading sessions…
        </div>
      )}
      {error && (
        <div
          className="rounded border border-danger/40 bg-danger/10 px-3 py-2 text-danger"
          role="alert"
        >
          <span className="font-medium">Sessions unavailable.</span> {error}
        </div>
      )}
      {sessions && (
        <div className="rounded border border-border">
          {sessions.length === 0 && <div className="px-3 py-2 text-fg-dim">(no sessions)</div>}
          {sessions.map((session) => (
            <div
              key={session.session_id}
              className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-border px-3 py-2 last:border-0"
            >
              <span className="flex min-w-48 flex-1 items-center gap-1.5">
                <span className="truncate" title={session.user_agent ?? undefined}>
                  {session.user_agent ?? "(unknown client)"}
                </span>
                {session.current && (
                  <span
                    className="shrink-0 rounded bg-accent-muted px-1 text-[10px] tracking-wide text-accent uppercase"
                    title="The session this browser is using"
                  >
                    this session
                  </span>
                )}
              </span>
              <span className="text-xs text-fg-dim">
                Started {new Date(session.created).toLocaleDateString()}
              </span>
              <span className="text-xs text-fg-dim">
                Seen {new Date(session.last_seen).toLocaleString()}
              </span>
              <button
                type="button"
                disabled={revoking === session.session_id}
                onClick={() => void revoke(session)}
                className="text-danger hover:underline disabled:opacity-40"
              >
                {revoking === session.session_id ? "revoking…" : "revoke"}
              </button>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}
