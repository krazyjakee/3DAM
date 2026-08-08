// The Administration surface (tech-spec 09 §B.4, 10 §5, ADR 0004) — plain DOM over the
// `/admin/api/*` routes: grouped toggle cards with a warn-and-confirm before any exposure-increasing
// change, live/restart labelling, and the audit trail. Being DOM (not canvas) is exactly why this is
// cheap to build well (DESIGN_GUIDELINES §3.6).
//
// This file is the orchestration — the access gate, the shared refresh, the flag cards and the audit
// trail. Each capability pane lives in `./settings/` (issues #161, #162) and is composed in below,
// so a pane can be mounted and tested without the whole surface.

import { useState, type ReactNode } from "react";
import { Link } from "react-router";
import {
  type AdminStatus,
  type FlagInfo,
  type FlagKey,
  type FlagValue,
  type NewTokenReply,
  type SetFlagReply,
} from "@/api/admin";
import {
  useAdminAudit,
  useAdminFlags,
  useAdminStatus,
  useSetAdminFlag,
} from "@/api/admin-queries";
import { ApiError } from "@/api/client";
import { useWhoami } from "@/api/queries";
import { getServer, setServer } from "@/lib/server";
import { errorMessage, toast } from "@/lib/toast";
import { useDialogs } from "@/lib/dialogs";
import { useEscape, useFocusTrap } from "@/lib/use-focus-trap";
import { TokenLoginForm } from "./auth/TokenLoginForm";
import { AccountsAndGroups } from "./settings/AccountsSection";
import { AdminField } from "./settings/AdminField";
import { Choice, Toggle } from "./settings/Controls";
import { OidcSection } from "./settings/OidcSection";
import {
  AdminSectionState,
  DependencyNote,
  SectionError,
  SectionLoading,
} from "./settings/SectionState";
import { StorageSection } from "./settings/StorageSection";
import { TokensSection } from "./settings/TokensSection";

type AccessState =
  | { kind: "checking" }
  | { kind: "granted" }
  | { kind: "denied"; message: string };

export function Administration() {
  // The bootstrap owner token, when enabling authentication just minted it (never locked out).
  const [bootstrap, setBootstrap] = useState<NewTokenReply | null>(null);
  // Which flag write is in flight — disables the flag controls so a slow admin round-trip can't be
  // double-submitted into two conflicting writes (issue #23).
  const [busyFlag, setBusyFlag] = useState<string | null>(null);
  // A 403/401 landing here (non-admin or anonymous) shouldn't dead-end — offer an in-page sign-in.
  const [signIn, setSignIn] = useState(false);
  // The current credential's identity — used to flag "this is the token you're signed in with" on
  // the revoke row, so an admin doesn't accidentally lock themselves out.
  const whoami = useWhoami();
  const { confirm } = useDialogs();

  // Status remains the access probe. Capability queries wait for it, so a denied caller produces
  // one clear response instead of a fan-out of parallel 403s. A network/status failure is not an
  // authorization decision: the other independently framed sections still get a chance to load.
  const statusQuery = useAdminStatus();
  const accessError = statusQuery.error instanceof ApiError ? statusQuery.error : null;
  const denied = accessError?.status === 401 || accessError?.status === 403;
  const access: AccessState = statusQuery.isPending
    ? { kind: "checking" }
    : denied
      ? {
          kind: "denied",
          message:
            accessError?.status === 401
              ? "Sign in with an admin token to manage this server."
              : "Your credential lacks the admin scope. Ask an admin for access or sign in with an admin token.",
        }
      : { kind: "granted" };
  const queriesEnabled = access.kind === "granted";
  const flagsQuery = useAdminFlags({ enabled: queriesEnabled });
  const auditQuery = useAdminAudit(25, { enabled: queriesEnabled });
  const flagMutation = useSetAdminFlag();

  const status = statusQuery.data ?? null;
  const statusError = statusQuery.error && !denied ? errorMessage(statusQuery.error) : null;
  const flags = flagsQuery.data ?? null;
  const flagsError = flagsQuery.error ? errorMessage(flagsQuery.error) : null;
  const audit = auditQuery.data ?? null;
  const auditError = auditQuery.error ? errorMessage(auditQuery.error) : null;

  /** Apply a flag-set reply. When enabling authentication minted the bootstrap owner token, adopt
   *  it as this client's credential in the same motion — the person flipping the switch must never
   *  be gated by their own action — and surface the secret once. */
  const applied = (reply: SetFlagReply) => {
    if (reply.bootstrap_token) {
      setServer(getServer().base, reply.bootstrap_token.secret);
      setBootstrap(reply.bootstrap_token);
    }
    toast.success("Setting updated");
  };

  /** Set a flag, retrying with `confirm` after an explicit warning on an exposure-increasing change. */
  const setFlag = async (key: FlagKey, value: FlagValue, version: number) => {
      setBusyFlag(key);
      try {
        applied(
          await flagMutation.mutateAsync({
            key,
            request: { value, expected_version: version },
          }),
        );
      } catch (e) {
        if (e instanceof ApiError && e.status === 400 && /exposure/i.test(e.message)) {
          if (
            await confirm({
              title: "Increase exposure?",
              message: e.message,
              danger: true,
              confirmLabel: "Apply anyway",
            })
          ) {
            try {
              applied(
                await flagMutation.mutateAsync({
                  key,
                  request: { value, expected_version: version, confirm: true },
                }),
              );
            } catch (e2) {
              toast.error(errorMessage(e2));
            }
          }
        } else {
          toast.error(errorMessage(e));
        }
      } finally {
        setBusyFlag(null);
      }
    };

  const flag = (key: FlagKey) => flags?.find((f) => f.key === key);

  return (
    <div className="mx-auto flex min-h-dvh max-w-3xl flex-col gap-6 p-4 text-sm sm:p-6">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="text-lg font-semibold">Administration</h1>
        <Link to="/" className="text-accent hover:underline">
          ← Back to library
        </Link>
      </header>

      {access.kind === "checking" && (
        <div className="rounded border border-border p-4 text-fg-dim" role="status">
          Checking administration access…
        </div>
      )}

      {access.kind === "denied" && (
        <div className="flex flex-wrap items-center justify-between gap-3 rounded border border-danger/40 bg-danger/10 p-4 text-danger">
          <div>
            <p className="font-medium">Administration access required</p>
            <p className="mt-1 text-xs">{access.message}</p>
          </div>
          <button
            type="button"
            className="btn shrink-0"
            onClick={() => setSignIn(true)}
          >
            Sign in with an admin token
          </button>
        </div>
      )}
      {signIn && <AdministrationSignIn onClose={() => setSignIn(false)} />}

      {access.kind === "granted" && bootstrap && (
        <div className="rounded border border-lic-permissive/40 bg-lic-permissive/10 p-3">
          <p className="text-lic-permissive">
            Authentication is on and no admin credential existed, so the owner token was created —
            this browser has adopted it and keeps working. Copy the secret now for other clients; it
            is shown once:
          </p>
          <code className="mt-1 block break-all font-mono text-xs">{bootstrap.secret}</code>
        </div>
      )}

      {access.kind === "granted" && (
        <>
          <AdminSectionState
            name="Server status"
            loading={!status && !statusError}
            error={statusError}
          >
            {status && <StatusCard status={status} />}
          </AdminSectionState>

          <section className="flex flex-col gap-3">
            <h2 className="font-medium text-fg-muted">Feature flags</h2>
            {flagsError && <SectionError name="Feature flags" message={flagsError} />}
            {!flags && !flagsError && <SectionLoading name="Feature flags" />}
            {flags && (
              <div className="flex flex-col gap-4">
                <FlagGroup title="Access">
                  <FlagCard
                    title="Network writes"
                    hint="Read-only to the network by default. Enabling allows writes from beyond localhost."
                    flag={flag("network_writes")}
                  >
                    <Toggle
                      label="Network writes"
                      checked={flag("network_writes")?.value === true}
                      disabled={busyFlag !== null}
                      onChange={(v) => {
                        const f = flag("network_writes");
                        if (f) void setFlag(f.key, v, f.version);
                      }}
                    />
                  </FlagCard>
                  <FlagCard
                    title="Uploads"
                    hint="Allows new files to be written into registered sources. Existing files are never replaced."
                    flag={flag("upload")}
                  >
                    <Toggle
                      label="Uploads"
                      checked={flag("upload")?.value === true}
                      disabled={busyFlag !== null}
                      onChange={(v) => {
                        const f = flag("upload");
                        if (f) void setFlag(f.key, v, f.version);
                      }}
                    />
                  </FlagCard>
                </FlagGroup>

                <FlagGroup title="Authentication">
                  <FlagCard
                    title="Authentication"
                    hint="Gate the API, MCP, and admin surface. Off gives the local owner full trust."
                    flag={flag("authentication")}
                  >
                    <Toggle
                      label="Authentication"
                      checked={flag("authentication")?.value !== "off"}
                      disabled={busyFlag !== null || flag("user_accounts")?.value === true}
                      onChange={(enabled) => {
                        const f = flag("authentication");
                        if (f) void setFlag(f.key, enabled ? "token" : "off", f.version);
                      }}
                    />
                    {flag("authentication")?.value !== "off" && flag("authentication") && (
                      <DependentOption label="Authentication mode">
                        <Choice
                          label="Authentication mode"
                          value={String(flag("authentication")?.value)}
                          options={
                            flag("user_accounts")?.value === true
                              ? ["token"]
                              : ["anonymous", "token"]
                          }
                          disabled={busyFlag !== null}
                          onChange={(v) => {
                            const f = flag("authentication");
                            if (f) void setFlag(f.key, v as FlagValue, f.version);
                          }}
                        />
                      </DependentOption>
                    )}
                    {flag("user_accounts")?.value === true && (
                      <DependencyNote>
                        User accounts require token authentication. Turn accounts off before
                        disabling this gate.
                      </DependencyNote>
                    )}
                  </FlagCard>
                  {flag("user_accounts")?.value === true ? (
                    <FlagCard
                      title="Single sign-on"
                      hint="Accept logins from the configured OIDC provider alongside passwords and API tokens."
                      flag={flag("oidc")}
                    >
                      <Toggle
                        label="Single sign-on"
                        checked={flag("oidc")?.value === true}
                        disabled={busyFlag !== null}
                        onChange={(v) => {
                          const f = flag("oidc");
                          if (f) void setFlag(f.key, v, f.version);
                        }}
                      />
                    </FlagCard>
                  ) : (
                    flag("oidc") && (
                      <DependencyNote>Enable User accounts to configure single sign-on.</DependencyNote>
                    )
                  )}
                </FlagGroup>

                <FlagGroup title="Accounts">
                  <FlagCard
                    title="User accounts"
                    hint="Full login accounts with groups and sharing; raises authentication to at least token."
                    flag={flag("user_accounts")}
                  >
                    <Toggle
                      label="User accounts"
                      checked={flag("user_accounts")?.value === true}
                      disabled={busyFlag !== null}
                      onChange={(v) => {
                        const f = flag("user_accounts");
                        if (f) void setFlag(f.key, v, f.version);
                      }}
                    />
                  </FlagCard>
                  {flag("user_accounts")?.value !== true && flag("user_accounts") && (
                    <DependencyNote>
                      Enable User accounts to reveal account, group, sharing, and SSO controls.
                    </DependencyNote>
                  )}
                </FlagGroup>

                <FlagGroup title="Agents / MCP">
                  <FlagCard
                    title="MCP agent server"
                    hint="Off removes the /mcp route entirely. Enable it to choose the tool access level."
                    flag={flag("mcp_server")}
                  >
                    <Toggle
                      label="MCP agent server"
                      checked={flag("mcp_server")?.value !== "off"}
                      disabled={busyFlag !== null}
                      onChange={(enabled) => {
                        const f = flag("mcp_server");
                        if (f) void setFlag(f.key, enabled ? "read_only" : "off", f.version);
                      }}
                    />
                    {flag("mcp_server")?.value !== "off" && flag("mcp_server") && (
                      <DependentOption label="Tool access">
                        <Choice
                          label="MCP tool access"
                          value={String(flag("mcp_server")?.value)}
                          options={["read_only", "read_write"]}
                          disabled={busyFlag !== null}
                          onChange={(v) => {
                            const f = flag("mcp_server");
                            if (f) void setFlag(f.key, v as FlagValue, f.version);
                          }}
                        />
                      </DependentOption>
                    )}
                  </FlagCard>
                </FlagGroup>

                <FlagGroup title="Federation">
                  <FlagCard
                    title="Federation peer"
                    hint="Serve this instance's catalog to other 3DAM instances."
                    flag={flag("federation")}
                  >
                    <Toggle
                      label="Federation peer"
                      checked={flag("federation")?.value === true}
                      disabled={busyFlag !== null}
                      onChange={(v) => {
                        const f = flag("federation");
                        if (f) void setFlag(f.key, v, f.version);
                      }}
                    />
                  </FlagCard>
                </FlagGroup>

                <FlagGroup title="Analysis">
                  <FlagCard
                    title="Auto-generate previews"
                    hint="Render thumbnails and 3D previews on ingest instead of on first request."
                    flag={flag("auto_thumbnail")}
                  >
                    <Toggle
                      label="Auto-generate previews"
                      checked={flag("auto_thumbnail")?.value === true}
                      disabled={busyFlag !== null}
                      onChange={(v) => {
                        const f = flag("auto_thumbnail");
                        if (f) void setFlag(f.key, v, f.version);
                      }}
                    />
                  </FlagCard>
                  <FlagCard
                    title="Auto-analyze on ingest"
                    hint="Run embeddings, auto-tags, and derived-attribute analysis when assets arrive."
                    flag={flag("auto_analyze")}
                  >
                    <Toggle
                      label="Auto-analyze on ingest"
                      checked={flag("auto_analyze")?.value === true}
                      disabled={busyFlag !== null}
                      onChange={(v) => {
                        const f = flag("auto_analyze");
                        if (f) void setFlag(f.key, v, f.version);
                      }}
                    />
                  </FlagCard>
                </FlagGroup>
              </div>
            )}
          </section>
          <StorageSection enabled={queriesEnabled} />

      {/* User accounts + groups (issue #42) — only while the flag is on (the routes 404 off). */}
          {flag("user_accounts")?.value === true && (
            <AccountsAndGroups currentAccountId={whoami.data?.account?.account_id ?? null} />
          )}

          <TokensSection
            currentIdentity={whoami.data?.identity ?? null}
            enabled={queriesEnabled}
          />

      {/* Single sign-on (issue #41). Gated on the flag's *presence*, not its value — the same
          "does this server support it" signal `FlagCard` uses. Deliberately not its value: the
          provider is configured *before* the capability is switched on, so gating on `=== true`
          would hide the thing the switch needs. Presence matters because a server that predates
          this feature has no `/admin/api/oidc` route, and unknown non-`api/` paths fall to the SPA
          fallback — so the fetch would return `index.html` with a 200 and the section would report
          a JSON parse error. It also keeps the section (and its admin-only fetches) away from a
          non-admin, who already gets one clear notice above. */}
          {flag("oidc") && flag("user_accounts")?.value === true && (
            <OidcSection oidcEnabled={flag("oidc")?.value === true} accountsEnabled />
          )}

          <AdminSectionState name="Audit log" loading={!audit && !auditError} error={auditError}>
            {audit && (
              <section className="flex flex-col gap-2">
                <h2 className="font-medium text-fg-muted">Audit log</h2>
                <div className="overflow-x-auto rounded border border-border">
                  {audit.length === 0 && <div className="px-3 py-2 text-fg-dim">(no entries)</div>}
                  {audit.map((e, i) => (
                    <div key={i} className="flex min-w-max gap-3 border-b border-border px-3 py-1.5 last:border-0">
                      <span className="w-40 shrink-0 text-fg-dim">
                        {new Date(e.at).toLocaleString()}
                      </span>
                      <span className="w-24 shrink-0 text-fg-muted">{e.actor}</span>
                      <span className="font-mono text-xs">{e.action}</span>
                      <span className="max-w-80 truncate text-fg-dim">{e.target ?? ""}</span>
                    </div>
                  ))}
                </div>
              </section>
            )}
          </AdminSectionState>
        </>
      )}
    </div>
  );
}

/** In-page sign-in for Administration (issue: don't dead-end a non-admin at a 403). Wraps the
 *  shared TokenLoginForm in a focus-trapped, Escape-dismissable modal; a successful sign-in reloads,
 *  re-fetching the admin surface with the new (hopefully admin-scoped) credential. */
function AdministrationSignIn({ onClose }: { onClose: () => void }) {
  const ref = useFocusTrap<HTMLDivElement>(true);
  useEscape(onClose);
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      onClick={onClose}
    >
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby="login-title"
        className="w-full max-w-md rounded-lg border border-border bg-surface p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <TokenLoginForm reason={null} allowReadOnly={false} onClose={onClose} />
      </div>
    </div>
  );
}

function FlagGroup({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section aria-labelledby={`flag-group-${title.toLowerCase().replaceAll(/[^a-z0-9]+/g, "-")}`}>
      <h3
        id={`flag-group-${title.toLowerCase().replaceAll(/[^a-z0-9]+/g, "-")}`}
        className="mb-2 text-xs font-medium tracking-wide text-fg-dim uppercase"
      >
        {title}
      </h3>
      <div className="flex flex-col gap-2">{children}</div>
    </section>
  );
}

function DependentOption({
  label,
  children,
}: {
  label: string;
  children: ReactNode;
}) {
  return (
    <label className="flex flex-wrap items-center justify-end gap-2 border-t border-border pt-2 text-xs text-fg-dim sm:border-0 sm:pt-0">
      <span>{label}</span>
      {children}
    </label>
  );
}

function StatusCard({ status }: { status: AdminStatus }) {
  return (
    <section
      className={`rounded border p-3 ${
        status.exposed_without_auth ? "border-warn/50 bg-warn/10" : "border-border"
      }`}
    >
      <div className="grid grid-cols-2 gap-x-6 gap-y-1 sm:grid-cols-3">
        <AdminField label="Bind" value={status.bind} />
        <AdminField label="Localhost only" value={String(status.localhost_only)} />
        <AdminField label="TLS" value={String(status.tls)} />
        <AdminField label="Auth" value={status.auth} />
        <AdminField label="MCP" value={status.mcp} />
        <AdminField label="Network writes" value={String(status.network_writes)} />
        <AdminField label="Tokens" value={String(status.token_count)} />
        {status.account_count != null && (
          <AdminField
            label="Accounts"
            value={status.unclaimed ? `${status.account_count} (unclaimed)` : String(status.account_count)}
          />
        )}
      </div>
      {status.exposed_without_auth && (
        <p className="mt-2 text-warn">
          ⚠ Exposed beyond localhost with no authentication and no TLS. Set the authentication flag.
        </p>
      )}
    </section>
  );
}

function FlagCard({
  title,
  hint,
  flag,
  children,
}: {
  title: string;
  hint: string;
  /** The reported flag, or undefined when this server doesn't expose it (leaner/older build). */
  flag?: FlagInfo;
  children: ReactNode;
}) {
  // Absent from the /flags response: this build doesn't support the flag, so a toggle here would be
  // a silent no-op. Say so and drop the control (issue: never a control that does nothing).
  const unsupported = !flag;
  const enabled = flag ? flag.value !== false && flag.value !== "off" : false;
  const atRisk = Boolean(flag?.exposure_increasing && enabled);
  return (
    <div
      className={`flex flex-col gap-3 rounded border p-3 sm:flex-row sm:items-center sm:justify-between ${
        atRisk ? "border-warn/50 bg-warn/10" : "border-border"
      }`}
    >
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-medium">{title}</span>
          {flag ? (
            <>
              <span
                className={`rounded px-1.5 py-0.5 text-[10px] font-medium tracking-wide uppercase ${
                  flag.live ? "bg-accent-muted text-accent" : "bg-surface-2 text-fg-muted"
                }`}
              >
                {flag.live ? "live" : "restart required"}
              </span>
              {flag.exposure_increasing && (
                <span
                  className={`rounded px-1.5 py-0.5 text-[10px] font-medium tracking-wide uppercase ${
                    atRisk ? "bg-warn/20 text-warn" : "bg-surface-2 text-fg-dim"
                  }`}
                >
                  {atRisk ? "exposure risk on" : "increases exposure"}
                </span>
              )}
              <span className="text-[10px] text-fg-dim">v{flag.version}</span>
            </>
          ) : (
            <span className="text-xs text-fg-dim italic">unsupported on this server</span>
          )}
        </div>
        <p className="text-xs text-fg-dim">{hint}</p>
      </div>
      <div className="flex shrink-0 flex-col gap-2 self-stretch sm:self-auto">
        {unsupported ? null : children}
      </div>
    </div>
  );
}
