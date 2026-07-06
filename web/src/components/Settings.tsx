// The admin / Settings surface (tech-spec 09 §B.4, 10 §5, ADR 0004) — plain DOM over the
// `/admin/api/*` routes: grouped toggle cards with a warn-and-confirm before any exposure-increasing
// change, live/restart labelling, API-token management, and the audit trail. Being DOM (not canvas)
// is exactly why this is cheap to build well (DESIGN_GUIDELINES §3.6).

import { useCallback, useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import {
  admin,
  getAdminToken,
  setAdminToken,
  type AdminStatus,
  type AuditEntry,
  type FlagInfo,
  type FlagValue,
  type NewTokenReply,
  type Scope,
  type TokenInfo,
} from "@/api/admin";
import { ApiError } from "@/api/client";

const ALL_SCOPES: Scope[] = ["read", "write", "admin", "mcp_use", "federate"];

export function Settings() {
  const [status, setStatus] = useState<AdminStatus | null>(null);
  const [flags, setFlags] = useState<FlagInfo[]>([]);
  const [tokens, setTokens] = useState<TokenInfo[]>([]);
  const [audit, setAudit] = useState<AuditEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const promptToken = useAdminTokenPrompt();

  const refresh = useCallback(async () => {
    try {
      const [s, f, t, a] = await Promise.all([
        admin.status(),
        admin.flags(),
        admin.tokens(),
        admin.audit(25),
      ]);
      setStatus(s);
      setFlags(f);
      setTokens(t);
      setAudit(a);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  /** Set a flag, retrying with `confirm` after an explicit warning on an exposure-increasing change. */
  const setFlag = useCallback(
    async (key: string, value: FlagValue, version: number) => {
      try {
        await admin.setFlag(key, { value, expected_version: version });
        await refresh();
      } catch (e) {
        if (e instanceof ApiError && e.status === 400 && /exposure/i.test(e.message)) {
          if (window.confirm(`This increases exposure:\n\n${e.message}\n\nApply anyway?`)) {
            await admin.setFlag(key, { value, expected_version: version, confirm: true });
            await refresh();
          }
        } else {
          setError(e instanceof Error ? e.message : String(e));
        }
      }
    },
    [refresh],
  );

  const flag = (key: string) => flags.find((f) => f.key === key);

  return (
    <div className="mx-auto flex min-h-dvh max-w-3xl flex-col gap-6 p-6 text-sm">
      <header className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">Settings &amp; Administration</h1>
        <div className="flex items-center gap-4">
          <button
            type="button"
            onClick={() => {
              promptToken();
              void refresh();
            }}
            className="text-neutral-400 hover:underline"
          >
            Set admin token…
          </button>
          <Link to="/" className="text-accent hover:underline">
            ← Back to library
          </Link>
        </div>
      </header>

      {error && (
        <div className="rounded border border-red-500/40 bg-red-500/10 px-3 py-2 text-red-300">
          {error}
        </div>
      )}

      {status && <StatusCard status={status} />}

      <section className="flex flex-col gap-3">
        <h2 className="font-medium text-neutral-400">Feature flags</h2>

        <FlagCard
          title="Authentication"
          hint="Gate the API, MCP, and admin surface. Off = the local owner has full trust."
          flag={flag("authentication")}
        >
          <Choice
            value={String(flag("authentication")?.value ?? "off")}
            options={["off", "anonymous", "token"]}
            onChange={(v) => {
              const f = flag("authentication");
              if (f) void setFlag(f.key, v as FlagValue, f.version);
            }}
          />
        </FlagCard>

        <FlagCard
          title="MCP agent server"
          hint="Off removes the /mcp route entirely. Read-write exposes the write tools."
          flag={flag("mcp_server")}
        >
          <Choice
            value={String(flag("mcp_server")?.value ?? "off")}
            options={["off", "read_only", "read_write"]}
            onChange={(v) => {
              const f = flag("mcp_server");
              if (f) void setFlag(f.key, v as FlagValue, f.version);
            }}
          />
        </FlagCard>

        <FlagCard
          title="Network writes"
          hint="Read-only to the network by default. Enabling allows writes from beyond localhost."
          flag={flag("network_writes")}
        >
          <Toggle
            checked={flag("network_writes")?.value === true}
            onChange={(v) => {
              const f = flag("network_writes");
              if (f) void setFlag(f.key, v, f.version);
            }}
          />
        </FlagCard>
      </section>

      <TokensSection tokens={tokens} onChange={refresh} />

      <section className="flex flex-col gap-2">
        <h2 className="font-medium text-neutral-400">Audit log</h2>
        <div className="rounded border border-neutral-800">
          {audit.length === 0 && <div className="px-3 py-2 text-neutral-500">(no entries)</div>}
          {audit.map((e, i) => (
            <div key={i} className="flex gap-3 border-b border-neutral-800 px-3 py-1.5 last:border-0">
              <span className="w-40 shrink-0 text-neutral-500">
                {new Date(e.at).toLocaleString()}
              </span>
              <span className="w-24 shrink-0 text-neutral-400">{e.actor}</span>
              <span className="font-mono text-xs">{e.action}</span>
              <span className="truncate text-neutral-500">{e.target ?? ""}</span>
            </div>
          ))}
        </div>
      </section>
    </div>
  );
}

function StatusCard({ status }: { status: AdminStatus }) {
  return (
    <section
      className={`rounded border p-3 ${
        status.exposed_without_auth ? "border-amber-500/50 bg-amber-500/10" : "border-neutral-800"
      }`}
    >
      <div className="grid grid-cols-2 gap-x-6 gap-y-1 sm:grid-cols-3">
        <Field label="Bind" value={status.bind} />
        <Field label="Localhost only" value={String(status.localhost_only)} />
        <Field label="TLS" value={String(status.tls)} />
        <Field label="Auth" value={status.auth} />
        <Field label="MCP" value={status.mcp} />
        <Field label="Network writes" value={String(status.network_writes)} />
        <Field label="Tokens" value={String(status.token_count)} />
      </div>
      {status.exposed_without_auth && (
        <p className="mt-2 text-amber-300">
          ⚠ Exposed beyond localhost with no authentication and no TLS. Set the authentication flag.
        </p>
      )}
    </section>
  );
}

function Field({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col">
      <span className="text-xs text-neutral-500">{label}</span>
      <span className="font-mono">{value}</span>
    </div>
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
  flag?: FlagInfo;
  children: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-4 rounded border border-neutral-800 p-3">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <span className="font-medium">{title}</span>
          {flag && (
            <span className="text-xs text-neutral-600">
              {flag.live ? "live" : "restart"} · v{flag.version}
            </span>
          )}
        </div>
        <p className="text-xs text-neutral-500">{hint}</p>
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

function Choice({
  value,
  options,
  onChange,
}: {
  value: string;
  options: string[];
  onChange: (v: string) => void;
}) {
  return (
    <select
      className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
      value={value}
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

function Toggle({ checked, onChange }: { checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      className={`h-6 w-11 rounded-full transition ${checked ? "bg-accent" : "bg-neutral-700"}`}
    >
      <span
        className={`block h-5 w-5 rounded-full bg-white transition ${
          checked ? "translate-x-5" : "translate-x-0.5"
        }`}
      />
    </button>
  );
}

function TokensSection({ tokens, onChange }: { tokens: TokenInfo[]; onChange: () => void }) {
  const [label, setLabel] = useState("");
  const [scopes, setScopes] = useState<Scope[]>(["read", "mcp_use"]);
  const [created, setCreated] = useState<NewTokenReply | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const create = async () => {
    setErr(null);
    try {
      const reply = await admin.createToken({ label, scopes });
      setCreated(reply);
      setLabel("");
      onChange();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <section className="flex flex-col gap-2">
      <h2 className="font-medium text-neutral-400">API tokens</h2>

      <div className="flex flex-col gap-2 rounded border border-neutral-800 p-3">
        <div className="flex flex-wrap items-center gap-2">
          <input
            className="min-w-40 flex-1 rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
            placeholder="Label (e.g. ci-reader)"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
          />
          <button
            type="button"
            disabled={!label.trim()}
            onClick={() => void create()}
            className="rounded bg-accent px-3 py-1 text-black disabled:opacity-40"
          >
            Issue token
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
        {err && <p className="text-red-300">{err}</p>}
        {created && (
          <div className="rounded border border-emerald-500/40 bg-emerald-500/10 p-2">
            <p className="text-emerald-300">
              Token “{created.label}” created. Copy the secret now — it is shown once:
            </p>
            <code className="mt-1 block break-all font-mono text-xs">{created.secret}</code>
          </div>
        )}
      </div>

      <div className="rounded border border-neutral-800">
        {tokens.length === 0 && <div className="px-3 py-2 text-neutral-500">(no tokens)</div>}
        {tokens.map((t) => (
          <div
            key={t.token_id}
            className="flex items-center gap-3 border-b border-neutral-800 px-3 py-1.5 last:border-0"
          >
            <span className="w-40 truncate font-medium">{t.label}</span>
            <span className="flex-1 truncate text-xs text-neutral-500">{t.scopes.join(", ")}</span>
            <span className="text-xs text-neutral-600">
              {t.last_used ? `used ${new Date(t.last_used).toLocaleDateString()}` : "unused"}
            </span>
            <button
              type="button"
              onClick={async () => {
                await admin.revokeToken(t.token_id);
                onChange();
              }}
              className="text-red-400 hover:underline"
            >
              revoke
            </button>
          </div>
        ))}
      </div>
    </section>
  );
}

/** A small helper for the toolbar link, kept here so the admin-token prompt lives with the surface. */
export function useAdminTokenPrompt() {
  return () => {
    const cur = getAdminToken() ?? "";
    const next = window.prompt("Admin bearer token (blank to clear):", cur);
    if (next !== null) setAdminToken(next.trim() || null);
  };
}
