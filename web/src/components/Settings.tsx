// The admin / Settings surface (tech-spec 09 §B.4, 10 §5, ADR 0004) — plain DOM over the
// `/admin/api/*` routes: grouped toggle cards with a warn-and-confirm before any exposure-increasing
// change, live/restart labelling, API-token management, and the audit trail. Being DOM (not canvas)
// is exactly why this is cheap to build well (DESIGN_GUIDELINES §3.6).

import { useCallback, useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router";
import {
  admin,
  type AccountInfo,
  type AdminStatus,
  type AuditEntry,
  type FlagInfo,
  type FlagKey,
  type FlagValue,
  type GroupInfo,
  type NewTokenReply,
  type Scope,
  type SetFlagReply,
  type StorageUsage,
  type TokenInfo,
} from "@/api/admin";
import { authApi } from "@/api/auth";
import type {
  AccountRole,
  OidcConfigInfo,
  OidcIdentity,
  OidcProvisioning,
  SessionInfo,
} from "@/api/types";
import { ApiError } from "@/api/client";
import { useWhoami } from "@/api/queries";
import { getServer, setServer } from "@/lib/server";
import { errorMessage, toast } from "@/lib/toast";
import { useDialogs } from "@/lib/dialogs";
import { useEscape, useFocusTrap } from "@/lib/use-focus-trap";
import { TokenLoginForm } from "./AuthGate";
import { AdminField } from "./settings/AdminField";
import { StorageSection } from "./settings/StorageSection";

const ALL_SCOPES: Scope[] = ["read", "write", "admin", "mcp_use", "federate"];

type AccessState =
  | { kind: "checking" }
  | { kind: "granted" }
  | { kind: "denied"; message: string };

export function Settings() {
  const [status, setStatus] = useState<AdminStatus | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [flags, setFlags] = useState<FlagInfo[] | null>(null);
  const [flagsError, setFlagsError] = useState<string | null>(null);
  const [tokens, setTokens] = useState<TokenInfo[] | null>(null);
  const [tokensError, setTokensError] = useState<string | null>(null);
  const [audit, setAudit] = useState<AuditEntry[] | null>(null);
  const [auditError, setAuditError] = useState<string | null>(null);
  const [usage, setUsage] = useState<StorageUsage | null>(null);
  const [usageError, setUsageError] = useState<string | null>(null);
  const [access, setAccess] = useState<AccessState>({ kind: "checking" });
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

  /** Refetch the admin surface. Each section renders as its call lands — the storage-usage walk
   *  can take a while on a large cold cache, and it must not hold the flags/tokens/status
   *  sections (or the whole screen) hostage. `withUsage: false` skips it for refreshes that
   *  can't change storage (flag/token writes). */
  const refresh = useCallback(async (opts?: { withUsage?: boolean }) => {
    const settle = async <T,>(
      p: Promise<T>,
      set: (v: T) => void,
      setSectionError: (message: string | null) => void,
    ): Promise<void> => {
      try {
        set(await p);
        setSectionError(null);
      } catch (e) {
        if (e instanceof ApiError && (e.status === 401 || e.status === 403)) {
          setAccess({
            kind: "denied",
            message:
              e.status === 401
                ? "Sign in with an admin token to manage this server."
                : "Your credential lacks the admin scope. Ask an admin for access or sign in with an admin token.",
          });
          return;
        }
        setSectionError(errorMessage(e));
      }
    };

    // Status is the access probe. Do not reveal a populated-looking admin body until this request
    // proves the caller may administer the server; it also prevents parallel 403 responses racing
    // successful state updates and briefly reopening the body.
    try {
      setStatus(await admin.status());
      setStatusError(null);
      setAccess({ kind: "granted" });
    } catch (e) {
      if (e instanceof ApiError && (e.status === 401 || e.status === 403)) {
        setAccess({
          kind: "denied",
          message:
            e.status === 401
              ? "Sign in with an admin token to manage this server."
              : "Your credential lacks the admin scope. Ask an admin for access or sign in with an admin token.",
        });
        return;
      }
      setStatusError(errorMessage(e));
      // A status-specific/network failure is not evidence of denied access. Let the independently
      // loaded sections report their own results instead of mislabelling it as a permissions issue.
      setAccess({ kind: "granted" });
    }

    const calls = [
      settle(admin.flags(), setFlags, setFlagsError),
      settle(admin.tokens(), setTokens, setTokensError),
      settle(admin.audit(25), setAudit, setAuditError),
    ];
    if (opts?.withUsage !== false)
      calls.push(settle(admin.storageUsage(), setUsage, setUsageError));
    await Promise.all(calls);
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  /** Apply a flag-set reply. When enabling authentication minted the bootstrap owner token, adopt
   *  it as this client's credential in the same motion — the person flipping the switch must never
   *  be gated by their own action — and surface the secret once. */
  const applied = useCallback(async (reply: SetFlagReply) => {
    if (reply.bootstrap_token) {
      setServer(getServer().base, reply.bootstrap_token.secret);
      setBootstrap(reply.bootstrap_token);
    }
    await refresh({ withUsage: false });
    toast.success("Setting updated");
  }, [refresh]);

  /** Set a flag, retrying with `confirm` after an explicit warning on an exposure-increasing change. */
  const setFlag = useCallback(
    async (key: string, value: FlagValue, version: number) => {
      setBusyFlag(key);
      try {
        await applied(await admin.setFlag(key, { value, expected_version: version }));
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
              await applied(
                await admin.setFlag(key, { value, expected_version: version, confirm: true }),
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
    },
    [applied, confirm],
  );

  const flag = (key: FlagKey) => flags?.find((f) => f.key === key);

  return (
    <div className="mx-auto flex min-h-dvh max-w-3xl flex-col gap-6 p-4 text-sm sm:p-6">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="text-lg font-semibold">Settings &amp; Administration</h1>
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
      {signIn && <SettingsSignIn onClose={() => setSignIn(false)} />}

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
          <StorageSection usage={usage} error={usageError} onChange={refresh} />

      {/* User accounts + groups (issue #42) — only while the flag is on (the routes 404 off). */}
          {flag("user_accounts")?.value === true && (
            <AccountsAndGroups currentAccountId={whoami.data?.account?.account_id ?? null} />
          )}

          <AdminSectionState name="API tokens" loading={!tokens && !tokensError} error={tokensError}>
            {tokens && (
              <TokensSection
                tokens={tokens}
                currentIdentity={whoami.data?.identity ?? null}
                onChange={() => void refresh({ withUsage: false })}
              />
            )}
          </AdminSectionState>

      {/* The signed-in account's own sessions (issue #42) — self-service, not an admin surface,
          but Settings is where credential management lives today. */}
          {whoami.data?.account && <MySessionsSection />}

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

/** In-page sign-in for the Settings surface (issue: don't dead-end a non-admin at a 403). Wraps the
 *  shared TokenLoginForm in a focus-trapped, Escape-dismissable modal; a successful sign-in reloads,
 *  re-fetching the admin surface with the new (hopefully admin-scoped) credential. */
function SettingsSignIn({ onClose }: { onClose: () => void }) {
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

function SectionLoading({ name }: { name: string }) {
  return (
    <div className="rounded border border-border px-3 py-2 text-fg-dim" role="status">
      Loading {name.toLowerCase()}…
    </div>
  );
}

function SectionError({ name, message }: { name: string; message: string }) {
  return (
    <div className="rounded border border-danger/40 bg-danger/10 px-3 py-2 text-danger" role="alert">
      <span className="font-medium">{name} unavailable.</span> {message}
    </div>
  );
}

/** A section may keep rendering its last successful payload beneath a later refresh error. */
function AdminSectionState({
  name,
  loading,
  error,
  children,
}: {
  name: string;
  loading: boolean;
  error: string | null;
  children: ReactNode;
}) {
  return (
    <>
      {loading && <SectionLoading name={name} />}
      {error && <SectionError name={name} message={error} />}
      {children}
    </>
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

function DependencyNote({ children }: { children: ReactNode }) {
  return (
    <p className="rounded border border-border px-3 py-2 text-xs text-fg-dim">
      {children}
    </p>
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

function Choice({
  value,
  options,
  onChange,
  disabled,
  label,
}: {
  value: string;
  options: string[];
  onChange: (v: string) => void;
  disabled?: boolean;
  /** Accessible name — the flag's title (a11y: axe select-name, issue #44). */
  label: string;
}) {
  return (
    <select
      className="field w-auto disabled:opacity-50"
      aria-label={label}
      value={value}
      disabled={disabled}
      onChange={(e) => onChange(e.target.value)}
    >
      {options.map((o) => (
        <option key={o} value={o}>
          {o}
        </option>
      ))}
    </select>
  );
}

function Toggle({
  checked,
  onChange,
  disabled,
  label,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  /** Accessible name — the flag's title (a11y: axe button-name, issue #44). */
  label: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`h-6 w-11 self-end rounded-full transition disabled:opacity-50 coarse:h-11 coarse:w-14 ${
        checked ? "bg-accent" : "bg-surface-2"
      }`}
    >
      <span
        className={`block h-5 w-5 rounded-full bg-fg transition coarse:h-6 coarse:w-6 ${
          checked ? "translate-x-5 coarse:translate-x-7" : "translate-x-0.5 coarse:translate-x-1"
        }`}
      />
    </button>
  );
}

function TokensSection({
  tokens,
  currentIdentity,
  onChange,
}: {
  tokens: TokenInfo[];
  /** The label of the token this browser is signed in with (from /whoami), so its row can be
   *  flagged and its revoke warned about — never lock yourself out by accident. */
  currentIdentity: string | null;
  onChange: () => void;
}) {
  const [label, setLabel] = useState("");
  const [scopes, setScopes] = useState<Scope[]>(["read", "mcp_use"]);
  // Optional expiry (the API's NewToken.expires) — a local `datetime-local` value, "" for no expiry.
  const [expiry, setExpiry] = useState("");
  const [created, setCreated] = useState<NewTokenReply | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  // Which token is being revoked — keeps its row's button disabled during the round-trip so a
  // second click can't fire a duplicate revoke (issue #23).
  const [revoking, setRevoking] = useState<string | null>(null);
  const { confirm } = useDialogs();

  const create = async () => {
    setErr(null);
    setCreating(true);
    try {
      // datetime-local is local wall-clock with no zone; parse to epoch ms for the API. No expiry → null.
      const expires = expiry ? new Date(expiry).getTime() : null;
      const reply = await admin.createToken({ label, scopes, expires });
      setCreated(reply);
      setLabel("");
      setExpiry("");
      onChange();
      toast.success(`Token “${reply.label}” issued`);
    } catch (e) {
      setErr(errorMessage(e));
      toast.error(errorMessage(e));
    } finally {
      setCreating(false);
    }
  };

  const revoke = async (t: TokenInfo) => {
    // Confirm every revoke; call out extra-loudly when it looks like the caller's own session token,
    // since revoking it signs this browser out (and, if it's the last admin, may lock everyone out).
    const isSelf = currentIdentity != null && t.label === currentIdentity;
    const ok = await confirm({
      title: `Revoke token “${t.label}”?`,
      message: isSelf
        ? "This looks like the token you're signed in with — revoking it will sign this browser out immediately, and if it's the only admin token you could lock yourself out. This cannot be undone."
        : "Any client using this token loses access immediately. This cannot be undone.",
      danger: true,
      confirmLabel: "Revoke token",
    });
    if (!ok) return;
    setRevoking(t.token_id);
    try {
      await admin.revokeToken(t.token_id);
      onChange();
      toast.success("Token revoked");
    } catch (e) {
      toast.error(errorMessage(e));
    } finally {
      setRevoking(null);
    }
  };

  return (
    <section className="flex flex-col gap-2">
      <h2 className="font-medium text-fg-muted">API tokens</h2>

      <div className="flex flex-col gap-2 rounded border border-border p-3">
        <div className="flex flex-wrap items-center gap-2">
          <input
            className="field min-w-40 flex-1"
            placeholder="Label (e.g. ci-reader)"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
          />
          <label className="flex items-center gap-1 text-xs text-fg-dim">
            <span>Expires</span>
            <input
              type="datetime-local"
              className="field w-auto"
              aria-label="Token expiry (optional)"
              title="Optional — leave blank for a token that never expires"
              value={expiry}
              onChange={(e) => setExpiry(e.target.value)}
            />
          </label>
          <button
            type="button"
            disabled={!label.trim() || creating}
            onClick={() => void create()}
            className="btn btn-accent disabled:opacity-40"
          >
            {creating ? "Issuing…" : "Issue token"}
          </button>
        </div>
        <div className="flex flex-wrap gap-3">
          {ALL_SCOPES.map((s) => (
            <label key={s} className="flex items-center gap-1 text-xs">
              <input
                type="checkbox"
                checked={scopes.includes(s)}
                onChange={(e) =>
                  setScopes((cur) =>
                    e.target.checked ? [...cur, s] : cur.filter((x) => x !== s),
                  )
                }
              />
              {s}
            </label>
          ))}
        </div>
        {err && <p className="text-danger">{err}</p>}
        {created && (
          <div className="rounded border border-lic-permissive/40 bg-lic-permissive/10 p-2">
            <p className="text-lic-permissive">
              Token “{created.label}” created. Copy the secret now — it is shown once:
            </p>
            <code className="mt-1 block break-all font-mono text-xs">{created.secret}</code>
          </div>
        )}
      </div>

      <div className="rounded border border-border">
        {tokens.length === 0 && <div className="px-3 py-2 text-fg-dim">(no tokens)</div>}
        {tokens.map((t) => {
          const isSelf = currentIdentity != null && t.label === currentIdentity;
          return (
            <div
              key={t.token_id}
              className="flex items-center gap-3 border-b border-border px-3 py-1.5 last:border-0"
            >
              <span className="flex w-40 min-w-0 items-center gap-1.5 truncate font-medium">
                <span className="truncate">{t.label}</span>
                {isSelf && (
                  <span
                    className="shrink-0 rounded bg-accent-muted px-1 text-[10px] tracking-wide text-accent uppercase"
                    title="The token this browser is signed in with"
                  >
                    this session
                  </span>
                )}
              </span>
              <span className="flex-1 truncate text-xs text-fg-dim">{t.scopes.join(", ")}</span>
              <span className="text-xs text-fg-dim">
                {t.expires ? `expires ${new Date(t.expires).toLocaleDateString()}` : "no expiry"}
              </span>
              <span className="text-xs text-fg-dim">
                {t.last_used ? `used ${new Date(t.last_used).toLocaleDateString()}` : "unused"}
              </span>
              <button
                type="button"
                disabled={revoking === t.token_id}
                onClick={() => void revoke(t)}
                className="text-danger hover:underline disabled:opacity-40"
              >
                {revoking === t.token_id ? "revoking…" : "revoke"}
              </button>
            </div>
          );
        })}
      </div>
    </section>
  );
}

// ── user accounts / groups (issue #42) ───────────────────────────────────────

const ROLES: AccountRole[] = ["admin", "editor", "viewer"];

/** Loads accounts + groups once (they cross-reference: group membership lists accounts) and feeds
 *  both admin panes. Only mounted while the `user_accounts` flag is on — the routes 404 off. */
function AccountsAndGroups({ currentAccountId }: { currentAccountId: string | null }) {
  const [accounts, setAccounts] = useState<AccountInfo[] | null>(null);
  const [groups, setGroups] = useState<GroupInfo[] | null>(null);
  const [accountsError, setAccountsError] = useState<string | null>(null);
  const [groupsError, setGroupsError] = useState<string | null>(null);

  const load = useCallback(async () => {
    await Promise.all([
      admin.accounts().then(
        (value) => {
          setAccounts(value);
          setAccountsError(null);
        },
        (error) => setAccountsError(errorMessage(error)),
      ),
      admin.groups().then(
        (value) => {
          setGroups(value);
          setGroupsError(null);
        },
        (error) => setGroupsError(errorMessage(error)),
      ),
    ]);
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  return (
    <>
      <AdminSectionState
        name="Accounts"
        loading={!accounts && !accountsError}
        error={accountsError}
      >
        {accounts && (
          <AccountsSection accounts={accounts} currentAccountId={currentAccountId} onChange={load} />
        )}
      </AdminSectionState>
      <AdminSectionState name="Groups" loading={!groups && !groupsError} error={groupsError}>
        {groups && (
          <>
            {!accounts && accountsError && (
              <DependencyNote>
                Groups loaded successfully, but account details are unavailable. Membership names
                and editing will return when the Accounts section can be loaded.
              </DependencyNote>
            )}
            <GroupsSection groups={groups} accounts={accounts} onChange={load} />
          </>
        )}
      </AdminSectionState>
    </>
  );
}

function AccountsSection({
  accounts,
  currentAccountId,
  onChange,
}: {
  accounts: AccountInfo[];
  /** The signed-in account (from /whoami), so its row is flagged — don't lock yourself out. */
  currentAccountId: string | null;
  onChange: () => void;
}) {
  const { confirm, prompt } = useDialogs();
  // Conflicts (409 — e.g. the last-admin guard) and other failures surface here, visibly, instead
  // of only as a transient toast.
  const [err, setErr] = useState<string | null>(null);
  // Which row action is in flight — disables the row's controls against double-submits (issue #23).
  const [busy, setBusy] = useState<string | null>(null);
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [role, setRole] = useState<AccountRole>("viewer");
  const [creating, setCreating] = useState(false);

  const run = async (key: string, fn: () => Promise<string | null>) => {
    setBusy(key);
    setErr(null);
    try {
      const msg = await fn();
      onChange();
      if (msg) toast.success(msg);
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const create = async () => {
    setCreating(true);
    setErr(null);
    try {
      const a = await admin.createAccount({ username: username.trim(), password, role });
      setUsername("");
      setPassword("");
      setRole("viewer");
      onChange();
      toast.success(`Account “${a.username}” created`);
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setCreating(false);
    }
  };

  const resetPassword = async (a: AccountInfo) => {
    const pw = await prompt({
      title: `Reset password for “${a.username}”`,
      message: "Their current sessions keep working; only the password changes.",
      password: true,
      placeholder: "New password",
      confirmLabel: "Reset password",
    });
    if (!pw) return;
    void run(`pw:${a.account_id}`, async () => {
      await admin.updateAccount(a.account_id, { password: pw });
      return "Password reset";
    });
  };

  const signOutEverywhere = async (a: AccountInfo) => {
    if (
      !(await confirm({
        title: `Sign out “${a.username}” everywhere?`,
        message:
          a.account_id === currentAccountId
            ? "This includes the session you're using. Every browser and device signed in to this account must sign in again."
            : "Every browser and device signed in to this account must sign in again.",
        danger: true,
        confirmLabel: "Sign out everywhere",
      }))
    )
      return;
    void run(`sess:${a.account_id}`, async () => {
      const r = await admin.revokeAccountSessions(a.account_id);
      return `Signed out ${r.revoked} session${r.revoked === 1 ? "" : "s"}`;
    });
  };

  const remove = async (a: AccountInfo) => {
    const isSelf = a.account_id === currentAccountId;
    if (
      !(await confirm({
        title: `Delete account “${a.username}”?`,
        message: isSelf
          ? "This is the account you're signed in with — deleting it signs this browser out immediately. This cannot be undone."
          : "Their sessions end immediately and any shares granted to them are removed. This cannot be undone.",
        danger: true,
        confirmLabel: "Delete account",
      }))
    )
      return;
    void run(`del:${a.account_id}`, async () => {
      await admin.deleteAccount(a.account_id);
      return "Account deleted";
    });
  };

  return (
    <section className="flex flex-col gap-2">
      <h2 className="font-medium text-fg-muted">User accounts</h2>

      <div className="flex flex-col gap-2 rounded border border-border p-3">
        <div className="flex flex-wrap items-center gap-2">
          <input
            className="field min-w-32 flex-1"
            placeholder="Username"
            autoComplete="off"
            spellCheck={false}
            value={username}
            onChange={(e) => setUsername(e.target.value)}
          />
          <input
            className="field min-w-32 flex-1"
            type="password"
            placeholder="Password"
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
          <Choice
            label="Role for the new account"
            value={role}
            options={ROLES}
            disabled={creating}
            onChange={(v) => setRole(v as AccountRole)}
          />
          <button
            type="button"
            disabled={!username.trim() || !password || creating}
            onClick={() => void create()}
            className="btn btn-accent disabled:opacity-40"
          >
            {creating ? "Creating…" : "Create account"}
          </button>
        </div>
      </div>

      {err && <p className="text-danger">{err}</p>}

      <div className="rounded border border-border">
        {accounts.length === 0 && <div className="px-3 py-2 text-fg-dim">(no accounts)</div>}
        {accounts.map((a) => {
          const isSelf = a.account_id === currentAccountId;
          const rowBusy = busy !== null && busy.endsWith(`:${a.account_id}`);
          return (
            <div
              key={a.account_id}
              className="flex flex-wrap items-center gap-3 border-b border-border px-3 py-1.5 last:border-0"
            >
              <span className="flex w-40 min-w-0 items-center gap-1.5 truncate font-medium">
                <span className="truncate" title={a.display_name ?? a.username}>
                  {a.username}
                </span>
                {isSelf && (
                  <span
                    className="shrink-0 rounded bg-accent-muted px-1 text-[10px] tracking-wide text-accent uppercase"
                    title="The account you're signed in with"
                  >
                    you
                  </span>
                )}
              </span>
              <Choice
                label={`Role for ${a.username}`}
                value={a.role}
                options={ROLES}
                disabled={rowBusy}
                onChange={(v) =>
                  void run(`role:${a.account_id}`, async () => {
                    await admin.updateAccount(a.account_id, { role: v as AccountRole });
                    return null;
                  })
                }
              />
              <label className="flex items-center gap-1.5 text-xs text-fg-dim">
                <Toggle
                  label={`${a.username} enabled`}
                  checked={!a.disabled}
                  disabled={rowBusy}
                  onChange={(v) =>
                    void run(`dis:${a.account_id}`, async () => {
                      await admin.updateAccount(a.account_id, { disabled: !v });
                      return null;
                    })
                  }
                />
                <span title="Disabled accounts can't sign in; their sessions stop working">
                  {a.disabled ? "disabled" : "enabled"}
                </span>
              </label>
              <span className="flex-1 text-right text-xs text-fg-dim">
                {a.last_login
                  ? `signed in ${new Date(a.last_login).toLocaleDateString()}`
                  : "never signed in"}
              </span>
              <button
                type="button"
                disabled={rowBusy}
                onClick={() => void resetPassword(a)}
                className="text-fg-muted hover:underline disabled:opacity-40"
              >
                reset password
              </button>
              <button
                type="button"
                disabled={rowBusy}
                onClick={() => void signOutEverywhere(a)}
                className="text-fg-muted hover:underline disabled:opacity-40"
                title="Revoke every live session of this account"
              >
                sign out everywhere
              </button>
              <button
                type="button"
                disabled={rowBusy}
                onClick={() => void remove(a)}
                className="text-danger hover:underline disabled:opacity-40"
              >
                delete
              </button>
            </div>
          );
        })}
      </div>
    </section>
  );
}

function GroupsSection({
  groups,
  accounts,
  onChange,
}: {
  groups: GroupInfo[];
  accounts: AccountInfo[] | null;
  onChange: () => void;
}) {
  const { confirm, prompt } = useDialogs();
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const run = async (key: string, fn: () => Promise<string | null>) => {
    setBusy(key);
    setErr(null);
    try {
      const msg = await fn();
      onChange();
      if (msg) toast.success(msg);
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const create = async () => {
    const name = (
      await prompt({ title: "New group", placeholder: "Name", confirmLabel: "Create" })
    )?.trim();
    if (!name) return;
    void run("create", async () => {
      await admin.createGroup(name);
      return `Group “${name}” created`;
    });
  };

  const remove = async (g: GroupInfo) => {
    if (
      !(await confirm({
        title: `Delete group “${g.name}”?`,
        message: "Shares granted to this group are removed. Its member accounts are untouched.",
        danger: true,
        confirmLabel: "Delete group",
      }))
    )
      return;
    void run(`del:${g.group_id}`, async () => {
      await admin.deleteGroup(g.group_id);
      return "Group deleted";
    });
  };

  /** Membership edits PUT the *whole* member set (the API replaces, not patches). */
  const toggleMember = (g: GroupInfo, accountId: string, on: boolean) => {
    const next = on ? [...g.members, accountId] : g.members.filter((m) => m !== accountId);
    void run(`members:${g.group_id}`, async () => {
      await admin.setGroupMembers(g.group_id, next);
      return null;
    });
  };

  return (
    <section className="flex flex-col gap-2">
      <div className="flex items-center justify-between">
        <h2 className="font-medium text-fg-muted">Groups</h2>
        <button type="button" className="btn" disabled={busy === "create"} onClick={() => void create()}>
          New group
        </button>
      </div>

      {err && <p className="text-danger">{err}</p>}

      {groups.length === 0 && (
        <div className="rounded border border-border px-3 py-2 text-fg-dim">
          (no groups — share with whole teams by grouping accounts)
        </div>
      )}
      {groups.map((g) => (
        <div key={g.group_id} className="flex flex-col gap-2 rounded border border-border p-3">
          <div className="flex items-center justify-between gap-3">
            <span className="font-medium">{g.name}</span>
            <button
              type="button"
              disabled={busy === `del:${g.group_id}`}
              onClick={() => void remove(g)}
              className="text-danger hover:underline disabled:opacity-40"
            >
              delete
            </button>
          </div>
          {accounts === null ? (
            <p className="text-xs text-fg-dim">
              Account membership is unavailable while account details cannot be loaded.
            </p>
          ) : accounts.length === 0 ? (
            <p className="text-xs text-fg-dim">(no accounts to add)</p>
          ) : (
            <div className="flex flex-wrap gap-3">
              {accounts.map((a) => (
                <label key={a.account_id} className="flex items-center gap-1 text-xs">
                  <input
                    type="checkbox"
                    checked={g.members.includes(a.account_id)}
                    disabled={busy === `members:${g.group_id}`}
                    onChange={(e) => toggleMember(g, a.account_id, e.target.checked)}
                  />
                  {a.username}
                </label>
              ))}
            </div>
          )}
        </div>
      ))}
    </section>
  );
}

/** The signed-in account's own sessions (issue #42): every browser/device holding a live session,
 *  the current one marked, each revocable. Self-service — any signed-in account sees its own list
 *  (the server scopes it); it just lives on the Settings page alongside credential management. */
function MySessionsSection() {
  const { confirm } = useDialogs();
  const [sessions, setSessions] = useState<SessionInfo[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setSessions(await authApi.sessions());
      setErr(null);
    } catch (e) {
      setErr(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  const revoke = async (s: SessionInfo) => {
    if (
      !(await confirm({
        title: s.current ? "Sign out this session?" : "Revoke this session?",
        message: s.current
          ? "This is the session you're using. This browser must sign in again."
          : "That browser or device loses access immediately and must sign in again.",
        danger: true,
        confirmLabel: s.current ? "Sign out" : "Revoke session",
      }))
    )
      return;
    setRevoking(s.session_id);
    try {
      await authApi.revokeSession(s.session_id);
      if (s.current) {
        location.reload();
        return;
      }
      await load();
      toast.success("Session revoked");
    } catch (e) {
      toast.error(errorMessage(e));
    } finally {
      setRevoking(null);
    }
  };

  return (
    <section className="flex flex-col gap-2">
      <h2 className="font-medium text-fg-muted">My sessions</h2>
      {!sessions && !err && <SectionLoading name="My sessions" />}
      {err && <SectionError name="My sessions" message={err} />}
      {sessions && <div className="rounded border border-border">
        {sessions.length === 0 && (
          <div className="px-3 py-2 text-fg-dim">(no sessions)</div>
        )}
        {sessions.map((s) => (
          <div
            key={s.session_id}
            className="flex items-center gap-3 border-b border-border px-3 py-1.5 last:border-0"
          >
            <span className="flex min-w-0 flex-1 items-center gap-1.5">
              <span className="truncate" title={s.user_agent ?? undefined}>
                {s.user_agent ?? "(unknown client)"}
              </span>
              {s.current && (
                <span
                  className="shrink-0 rounded bg-accent-muted px-1 text-[10px] tracking-wide text-accent uppercase"
                  title="The session this browser is using"
                >
                  this session
                </span>
              )}
            </span>
            <span className="text-xs text-fg-dim">
              started {new Date(s.created).toLocaleDateString()}
            </span>
            <span className="text-xs text-fg-dim">
              seen {new Date(s.last_seen).toLocaleString()}
            </span>
            <button
              type="button"
              disabled={revoking === s.session_id}
              onClick={() => void revoke(s)}
              className="text-danger hover:underline disabled:opacity-40"
            >
              {revoking === s.session_id ? "revoking…" : "revoke"}
            </button>
          </div>
        ))}
      </div>}
    </section>
  );
}


/** A link's identity: the same subject can exist under two issuers, so neither half alone is a key. */
function rowKey(i: OidcIdentity): string {
  return `${i.issuer} ${i.subject}`;
}

/** OIDC/OAuth2 provider configuration and identity links (issue #41).
 *
 *  Rendered whatever the flags say, deliberately. The server lets a provider be configured *before*
 *  `oidc` is switched on — which is the sane order, since the flag is exposure-increasing and needs
 *  a confirm — so hiding this until the flag is on would mean an operator flips the switch and then
 *  cannot find where to configure the thing they just enabled. Instead the section says what is
 *  still missing.
 *
 *  The client secret is write-only end to end: `OidcConfigInfo` has no field for it, so there is
 *  nothing to prefill and nothing that could round-trip it back by accident. The input starts empty
 *  on every load, and an empty input means "keep whatever is stored".
 */
function OidcSection({
  oidcEnabled,
  accountsEnabled,
}: {
  oidcEnabled: boolean;
  accountsEnabled: boolean;
}) {
  const { confirm } = useDialogs();
  const [cfg, setCfg] = useState<OidcConfigInfo | null>(null);
  const [identities, setIdentities] = useState<OidcIdentity[] | null>(null);
  const [configError, setConfigError] = useState<string | null>(null);
  const [identitiesError, setIdentitiesError] = useState<string | null>(null);
  // False until the *config* read has actually answered. Without it a failed load is
  // indistinguishable from "no provider yet", and saving from that state silently replaces one.
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [linking, setLinking] = useState(false);
  // Keyed by issuer *and* subject, matching the row identity — the same subject can appear under
  // two issuers after an issuer change, and keying on subject alone flips both rows to "Unlinking…".
  const [unlinking, setUnlinking] = useState<string | null>(null);

  // Form state, seeded from the server on load — except the secret, which the server never sends.
  const [issuer, setIssuer] = useState("");
  const [clientId, setClientId] = useState("");
  const [redirectUrl, setRedirectUrl] = useState("");
  const [scopes, setScopes] = useState("");
  const [provisioning, setProvisioning] = useState<OidcProvisioning>("linked");
  const [secret, setSecret] = useState("");
  const [linkSubject, setLinkSubject] = useState("");
  const [linkAccountId, setLinkAccountId] = useState("");

  /** Load config and links independently.
   *
   *  Not `Promise.all`: it rejects on the first failure, so a links call that 404s would leave the
   *  *config* unread even though it succeeded — and the form would render blank, say "Add
   *  provider", and turn Save into a full replace of a configuration the operator never saw. The
   *  same per-call `settle` shape the rest of this file uses (`refresh` above), for the same
   *  reason. `loaded` gates the form so nothing is offered before the truth is known. */
  const load = useCallback(async () => {
    const [c, ids] = await Promise.allSettled([admin.oidcConfig(), admin.oidcIdentities()]);
    if (c.status === "fulfilled") {
      setCfg(c.value);
      if (c.value) {
        setIssuer(c.value.issuer);
        setClientId(c.value.client_id);
        setRedirectUrl(c.value.redirect_url);
        setScopes(c.value.scopes.join(", "));
        setProvisioning(c.value.provisioning);
      }
      setLoaded(true);
      setConfigError(null);
    } else {
      setConfigError(errorMessage(c.reason));
    }
    if (ids.status === "fulfilled") {
      setIdentities(ids.value);
      setIdentitiesError(null);
    } else {
      setIdentitiesError(errorMessage(ids.reason));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  const save = async () => {
    setSaving(true);
    try {
      const next = await admin.setOidcConfig({
        issuer: issuer.trim(),
        client_id: clientId.trim(),
        redirect_url: redirectUrl.trim(),
        scopes: scopes
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean),
        provisioning,
        // Omitted entirely when blank — that is what tells the server to keep the stored secret.
        // Trimmed first, so a stray space from a paste is "blank" rather than a one-character
        // secret that silently replaces the real one. (An operator who needs to *clear* the secret
        // — making it a public client — does that with `3dam admin oidc set --client-secret ""`;
        // there is deliberately no button for it here, since from this form an empty box already
        // means "leave it alone" and one control cannot honestly mean both.)
        ...(secret.trim() ? { client_secret: secret.trim() } : {}),
      });
      setCfg(next);
      setSecret("");
      // Re-read the links: changing the issuer re-keys which of them still apply, and the list
      // rendered above is otherwise a stale answer to a question the save just changed.
      try {
        setIdentities(await admin.oidcIdentities());
      } catch {
        /* the list is advisory here; the save itself already succeeded */
      }
      toast.success("Provider saved");
    } catch (e) {
      toast.error(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const link = async () => {
    setLinking(true);
    try {
      setIdentities(
        await admin.linkOidcIdentity({
          subject: linkSubject.trim(),
          account_id: linkAccountId.trim(),
        }),
      );
      setLinkSubject("");
      setLinkAccountId("");
      toast.success("Identity linked");
    } catch (e) {
      toast.error(errorMessage(e));
    } finally {
      setLinking(false);
    }
  };

  const unlink = async (i: OidcIdentity) => {
    const ok = await confirm({
      title: `Unlink ${i.username}?`,
      message:
        "The account is kept — this only revokes the provider's ability to sign in as it. Under " +
        "the default policy that person cannot sign in with the provider again until relinked.",
      danger: true,
      confirmLabel: "Unlink",
    });
    if (!ok) return;
    setUnlinking(rowKey(i));
    try {
      setIdentities(await admin.unlinkOidcIdentity(i.subject, i.issuer));
      toast.success("Identity unlinked");
    } catch (e) {
      toast.error(errorMessage(e));
    } finally {
      setUnlinking(null);
    }
  };

  const configured = cfg !== null;
  const canSave =
    loaded && issuer.trim() !== "" && clientId.trim() !== "" && redirectUrl.trim() !== "";

  return (
    <section className="flex flex-col gap-2">
      <h2 className="font-medium text-fg-muted">Single sign-on (OIDC)</h2>
      <p className="text-fg-dim">
        Let people sign in through an external identity provider. The session it mints here is an
        ordinary one, so password sign-in and API tokens keep working alongside it.
      </p>
      {!loaded && !configError && <SectionLoading name="OIDC provider configuration" />}
      {configError && <SectionError name="OIDC provider configuration" message={configError} />}

      {/* Say what is still missing rather than hiding the controls that fix it. */}
      {!accountsEnabled && (
        <p className="rounded border border-warn/40 bg-warn/10 p-2 text-warn">
          User accounts are off. Single sign-on needs them — a verified identity has to resolve to
          an account.
        </p>
      )}
      {accountsEnabled && !oidcEnabled && (
        <p className="rounded border border-warn/40 bg-warn/10 p-2 text-warn">
          The Single sign-on capability is still off, so the sign-in route is absent. Configure the
          provider here first, then turn it on above.
        </p>
      )}

      {loaded && <div className="flex flex-col gap-2 rounded border border-border p-3">
        <label className="flex flex-col gap-1">
          <span className="text-fg-dim">Issuer URL</span>
          <input
            className="field"
            placeholder="https://accounts.example.com"
            value={issuer}
            onChange={(e) => setIssuer(e.target.value)}
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-fg-dim">Client ID</span>
          <input className="field" value={clientId} onChange={(e) => setClientId(e.target.value)} />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-fg-dim">
            Client secret
            {configured && cfg.client_secret_set
              ? " — stored; leave blank to keep it"
              : " — not set"}
          </span>
          <input
            className="field"
            type="password"
            autoComplete="new-password"
            placeholder={configured && cfg.client_secret_set ? "••••••" : ""}
            value={secret}
            onChange={(e) => setSecret(e.target.value)}
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-fg-dim">Redirect URL</span>
          <input
            className="field"
            placeholder={`${location.origin}/api/v1/auth/oidc/callback`}
            value={redirectUrl}
            onChange={(e) => setRedirectUrl(e.target.value)}
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-fg-dim">Extra scopes (comma separated)</span>
          <input
            className="field"
            placeholder="email, profile"
            value={scopes}
            onChange={(e) => setScopes(e.target.value)}
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-fg-dim">Someone signs in who has no linked account</span>
          <select
            className="field"
            value={provisioning}
            onChange={(e) => setProvisioning(e.target.value as OidcProvisioning)}
          >
            <option value="linked">Refuse — link them below first (recommended)</option>
            <option value="auto_viewer">Create a viewer account for them</option>
            <option value="auto_editor">Create an editor account for them</option>
          </select>
          <span className="text-fg-dim">
            {provisioning === "linked"
              ? "The provider proves who someone is, not that they belong here."
              : "Only sound when everyone who can authenticate with this provider should have an account here."}
          </span>
        </label>
        <div>
          <button
            type="button"
            disabled={!canSave || saving}
            onClick={() => void save()}
            className="btn btn-accent disabled:opacity-40"
          >
            {saving ? "Saving…" : configured ? "Save provider" : "Add provider"}
          </button>
        </div>
      </div>}

      <h3 className="mt-1 font-medium text-fg-muted">Linked identities</h3>
      <p className="text-fg-dim">
        Which provider subject signs in as which account. Under the default policy this is required
        — without a link, an otherwise valid sign-in is still refused.
      </p>
      {!identities && !identitiesError && <SectionLoading name="Linked identities" />}
      {identitiesError && <SectionError name="Linked identities" message={identitiesError} />}
      {identities && <div className="rounded border border-border">
        {identities.length === 0 && <div className="px-3 py-2 text-fg-dim">(none linked)</div>}
        {identities.map((i) => (
          <div
            key={rowKey(i)}
            className="flex items-center gap-3 border-b border-border px-3 py-1.5 last:border-0"
          >
            <span className="min-w-0 flex-1 truncate" title={`${i.subject}\n${i.issuer}`}>
              <span className="text-fg-muted">{i.username}</span>{" "}
              <span className="text-fg-dim">← {i.subject}</span>
              {/* A link made under a previous issuer authenticates nobody. Say so, and show which
                  issuer it belongs to, or it reads as a working link that mysteriously fails. */}
              {cfg && i.issuer !== cfg.issuer && (
                <span className="ml-2 text-warn" title={i.issuer}>
                  (stale — {i.issuer})
                </span>
              )}
            </span>
            <button
              type="button"
              disabled={unlinking === rowKey(i)}
              onClick={() => void unlink(i)}
              className="btn disabled:opacity-40"
            >
              {unlinking === rowKey(i) ? "Unlinking…" : "Unlink"}
            </button>
          </div>
        ))}
      </div>}
      <div className="flex flex-wrap items-center gap-2 rounded border border-border p-3">
        <input
          className="field min-w-40 flex-1"
          placeholder="Provider subject (sub)"
          value={linkSubject}
          onChange={(e) => setLinkSubject(e.target.value)}
        />
        <input
          className="field min-w-40 flex-1"
          placeholder="Account id"
          value={linkAccountId}
          onChange={(e) => setLinkAccountId(e.target.value)}
        />
        <button
          type="button"
          disabled={!linkSubject.trim() || !linkAccountId.trim() || linking || !configured}
          onClick={() => void link()}
          className="btn btn-accent disabled:opacity-40"
        >
          {linking ? "Linking…" : "Link"}
        </button>
      </div>
    </section>
  );
}
