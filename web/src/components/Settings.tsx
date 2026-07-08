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
  type CacheTarget,
  type FlagInfo,
  type FlagValue,
  type NewTokenReply,
  type Scope,
  type StorageUsage,
  type TokenInfo,
} from "@/api/admin";
import { ApiError } from "@/api/client";
import { useScan } from "@/api/queries";
import { errorMessage, toast } from "@/lib/toast";
import { useDialogs } from "@/lib/dialogs";

const ALL_SCOPES: Scope[] = ["read", "write", "admin", "mcp_use", "federate"];

export function Settings() {
  const [status, setStatus] = useState<AdminStatus | null>(null);
  const [flags, setFlags] = useState<FlagInfo[]>([]);
  const [tokens, setTokens] = useState<TokenInfo[]>([]);
  const [audit, setAudit] = useState<AuditEntry[]>([]);
  const [usage, setUsage] = useState<StorageUsage | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Which flag write is in flight — disables the flag controls so a slow admin round-trip can't be
  // double-submitted into two conflicting writes (issue #23).
  const [busyFlag, setBusyFlag] = useState<string | null>(null);
  const { confirm } = useDialogs();
  const promptToken = useAdminTokenPrompt();

  const refresh = useCallback(async () => {
    try {
      const [s, f, t, a, u] = await Promise.all([
        admin.status(),
        admin.flags(),
        admin.tokens(),
        admin.audit(25),
        admin.storageUsage(),
      ]);
      setStatus(s);
      setFlags(f);
      setTokens(t);
      setAudit(a);
      setUsage(u);
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
      setBusyFlag(key);
      try {
        await admin.setFlag(key, { value, expected_version: version });
        await refresh();
        toast.success("Setting updated");
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
              await admin.setFlag(key, { value, expected_version: version, confirm: true });
              await refresh();
              toast.success("Setting updated");
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
    [refresh, confirm],
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
            className="text-fg-muted hover:underline"
          >
            Set admin token…
          </button>
          <Link to="/" className="text-accent hover:underline">
            ← Back to library
          </Link>
        </div>
      </header>

      {error && (
        <div className="rounded border border-danger/40 bg-danger/10 px-3 py-2 text-danger">
          {error}
        </div>
      )}

      {status && <StatusCard status={status} />}

      <section className="flex flex-col gap-3">
        <h2 className="font-medium text-fg-muted">Feature flags</h2>

        <FlagCard
          title="Authentication"
          hint="Gate the API, MCP, and admin surface. Off = the local owner has full trust."
          flag={flag("authentication")}
        >
          <Choice
            label="Authentication"
            value={String(flag("authentication")?.value ?? "off")}
            options={["off", "anonymous", "token"]}
            disabled={busyFlag !== null}
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
            label="MCP agent server"
            value={String(flag("mcp_server")?.value ?? "off")}
            options={["off", "read_only", "read_write"]}
            disabled={busyFlag !== null}
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
            label="Network writes"
            checked={flag("network_writes")?.value === true}
            disabled={busyFlag !== null}
            onChange={(v) => {
              const f = flag("network_writes");
              if (f) void setFlag(f.key, v, f.version);
            }}
          />
        </FlagCard>
      </section>

      <StorageSection usage={usage} onChange={refresh} />

      <TokensSection tokens={tokens} onChange={refresh} />

      <section className="flex flex-col gap-2">
        <h2 className="font-medium text-fg-muted">Audit log</h2>
        <div className="rounded border border-border">
          {audit.length === 0 && <div className="px-3 py-2 text-fg-dim">(no entries)</div>}
          {audit.map((e, i) => (
            <div key={i} className="flex gap-3 border-b border-border px-3 py-1.5 last:border-0">
              <span className="w-40 shrink-0 text-fg-dim">
                {new Date(e.at).toLocaleString()}
              </span>
              <span className="w-24 shrink-0 text-fg-muted">{e.actor}</span>
              <span className="font-mono text-xs">{e.action}</span>
              <span className="truncate text-fg-dim">{e.target ?? ""}</span>
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
        status.exposed_without_auth ? "border-warn/50 bg-warn/10" : "border-border"
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
        <p className="mt-2 text-warn">
          ⚠ Exposed beyond localhost with no authentication and no TLS. Set the authentication flag.
        </p>
      )}
    </section>
  );
}

function Field({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col">
      <span className="text-xs text-fg-dim">{label}</span>
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
    <div className="flex items-center justify-between gap-4 rounded border border-border p-3">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <span className="font-medium">{title}</span>
          {flag && (
            <span className="text-xs text-fg-dim">
              {flag.live ? "live" : "restart"} · v{flag.version}
            </span>
          )}
        </div>
        <p className="text-xs text-fg-dim">{hint}</p>
      </div>
      <div className="shrink-0">{children}</div>
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
      className={`h-6 w-11 rounded-full transition disabled:opacity-50 ${
        checked ? "bg-accent" : "bg-surface-2"
      }`}
    >
      <span
        className={`block h-5 w-5 rounded-full bg-fg transition ${
          checked ? "translate-x-5" : "translate-x-0.5"
        }`}
      />
    </button>
  );
}

/** Human-readable bytes (1024-based), e.g. `12.4 MiB`. */
function fmtBytes(n: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${units[i]}`;
}

/** One maintenance action: a labelled row with a hint and a single button. */
function ActionRow({
  title,
  hint,
  button,
  onClick,
  busy,
  danger,
}: {
  title: string;
  hint: string;
  button: string;
  onClick: () => void;
  busy: boolean;
  danger?: boolean;
}) {
  return (
    <div className="flex items-center justify-between gap-4 rounded border border-border p-3">
      <div className="min-w-0">
        <div className="font-medium">{title}</div>
        <p className="text-xs text-fg-dim">{hint}</p>
      </div>
      <button
        type="button"
        disabled={busy}
        onClick={onClick}
        className={`btn shrink-0 disabled:opacity-40 ${danger ? "text-danger" : ""}`}
      >
        {busy ? "Working…" : button}
      </button>
    </div>
  );
}

/** Storage overview + the maintenance actions (tech-spec 10 §5). Destructive ops warn-and-confirm;
 *  everything here is non-destructive to files inside registered sources. */
function StorageSection({
  usage,
  onChange,
}: {
  usage: StorageUsage | null;
  onChange: () => void;
}) {
  const { confirm } = useDialogs();
  const scan = useScan();
  // The key of the action currently in flight, so its button (and the destructive group) disables
  // during the round-trip without blocking unrelated rows.
  const [busy, setBusy] = useState<string | null>(null);

  /** Run a maintenance call: mark busy, toast the result, refresh the usage numbers. */
  const run = async (key: string, fn: () => Promise<string>) => {
    setBusy(key);
    try {
      toast.success(await fn());
      onChange();
    } catch (e) {
      toast.error(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const clearCache = (target: CacheTarget, label: string) =>
    run(`cache:${target}`, async () => {
      const r = await admin.clearCache(target);
      return `Cleared ${label}: ${r.files_deleted} files, ${fmtBytes(r.bytes_freed)} freed`;
    });

  const rescanAll = () => {
    scan.mutate({ mode: "full" });
    toast.success("Full rescan of all sources started");
  };

  const clearAnalysis = async () => {
    if (
      !(await confirm({
        title: "Clear analysis?",
        message:
          "Drops auto-tag/dedup suggestions and derived analysis, and marks every asset for re-analysis. Your confirmed tags are kept.",
        danger: true,
        confirmLabel: "Clear analysis",
      }))
    )
      return;
    void run("analysis", async () => {
      const r = await admin.clearAnalysis();
      return `Cleared ${r.suggestions_removed} suggestions and ${r.embeddings_removed} embeddings`;
    });
  };

  const vacuum = () =>
    run("vacuum", async () => {
      const r = await admin.vacuum();
      return `Database compacted — reclaimed ${fmtBytes(r.reclaimed_bytes)}`;
    });

  const resetCatalog = async () => {
    if (
      !(await confirm({
        title: "Reset the catalog?",
        message:
          "Removes every cataloged asset, source, collection, and tag from this library. Files on disk are NOT touched, and your tokens & settings are kept. This cannot be undone.",
        danger: true,
        confirmLabel: "Reset catalog",
      }))
    )
      return;
    void run("wipe", async () => {
      const r = await admin.wipe(true);
      return `Catalog reset — ${r.assets_removed} assets, ${r.sources_removed} sources removed`;
    });
  };

  const factoryReset = async () => {
    if (
      !(await confirm({
        title: "Factory reset everything?",
        message:
          "Erases the catalog AND all caches, API tokens, feature flags, and the audit log. The app returns to its first-run state and your current admin token stops working. This cannot be undone.",
        danger: true,
        confirmLabel: "Factory reset",
      }))
    )
      return;
    void run("factory", async () => {
      const r = await admin.factoryReset(true);
      return `Factory reset complete — ${r.catalog.assets_removed} assets and ${r.tokens_removed} tokens removed`;
    });
  };

  return (
    <section className="flex flex-col gap-3">
      <h2 className="font-medium text-fg-muted">Storage &amp; maintenance</h2>

      <section className="rounded border border-border p-3">
        {usage ? (
          <div className="grid grid-cols-2 gap-x-6 gap-y-1 sm:grid-cols-3">
            <Field label="Data directory" value={usage.data_dir} />
            <Field label="Catalog (library.db)" value={fmtBytes(usage.library_db_bytes)} />
            <Field label="Server config (server.db)" value={fmtBytes(usage.server_db_bytes)} />
            <Field
              label="Thumbnail cache"
              value={`${fmtBytes(usage.thumbnails.bytes)} · ${usage.thumbnails.files} files`}
            />
            <Field
              label="3D preview cache"
              value={`${fmtBytes(usage.previews.bytes)} · ${usage.previews.files} files`}
            />
            <Field label="Assets" value={String(usage.asset_count)} />
            <Field label="Sources" value={String(usage.source_count)} />
          </div>
        ) : (
          <p className="text-fg-dim">Loading storage usage…</p>
        )}
      </section>

      <ActionRow
        title="Rescan all sources"
        hint="Full re-read of every registered source (the sidebar only runs quick, changed-file scans)."
        button="Rescan all (full)"
        busy={scan.isPending}
        onClick={rescanAll}
      />
      <ActionRow
        title="Clear thumbnail cache"
        hint="Delete cached image thumbnails. They regenerate on next view."
        button="Clear thumbnails"
        busy={busy === "cache:thumbnails"}
        onClick={() => void clearCache("thumbnails", "thumbnails")}
      />
      <ActionRow
        title="Clear 3D preview cache"
        hint="Delete cached 3D preview meshes. They regenerate on next view."
        button="Clear 3D previews"
        busy={busy === "cache:previews"}
        onClick={() => void clearCache("previews", "3D previews")}
      />
      <ActionRow
        title="Clear analysis"
        hint="Drop auto-tag/dedup suggestions and embeddings; keeps confirmed tags."
        button="Clear analysis"
        busy={busy === "analysis"}
        onClick={() => void clearAnalysis()}
        danger
      />
      <ActionRow
        title="Compact database"
        hint="Reclaim disk space freed by deletions (VACUUM)."
        button="Compact"
        busy={busy === "vacuum"}
        onClick={vacuum}
      />

      <div className="mt-2 flex flex-col gap-3 rounded border border-danger/40 bg-danger/5 p-3">
        <div className="text-xs font-medium uppercase tracking-wide text-danger">Danger zone</div>
        <ActionRow
          title="Reset catalog"
          hint="Wipe all cataloged assets/sources/collections. Files on disk are untouched; tokens & settings are kept."
          button="Reset catalog"
          busy={busy === "wipe"}
          onClick={() => void resetCatalog()}
          danger
        />
        <ActionRow
          title="Factory reset"
          hint="Erase everything: catalog, caches, tokens, feature flags, and audit log. Returns to first-run."
          button="Factory reset"
          busy={busy === "factory"}
          onClick={() => void factoryReset()}
          danger
        />
      </div>
    </section>
  );
}

function TokensSection({ tokens, onChange }: { tokens: TokenInfo[]; onChange: () => void }) {
  const [label, setLabel] = useState("");
  const [scopes, setScopes] = useState<Scope[]>(["read", "mcp_use"]);
  const [created, setCreated] = useState<NewTokenReply | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  // Which token is being revoked — keeps its row's button disabled during the round-trip so a
  // second click can't fire a duplicate revoke (issue #23).
  const [revoking, setRevoking] = useState<string | null>(null);

  const create = async () => {
    setErr(null);
    setCreating(true);
    try {
      const reply = await admin.createToken({ label, scopes });
      setCreated(reply);
      setLabel("");
      onChange();
      toast.success(`Token “${reply.label}” issued`);
    } catch (e) {
      setErr(errorMessage(e));
      toast.error(errorMessage(e));
    } finally {
      setCreating(false);
    }
  };

  const revoke = async (id: string) => {
    setRevoking(id);
    try {
      await admin.revokeToken(id);
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
        {tokens.map((t) => (
          <div
            key={t.token_id}
            className="flex items-center gap-3 border-b border-border px-3 py-1.5 last:border-0"
          >
            <span className="w-40 truncate font-medium">{t.label}</span>
            <span className="flex-1 truncate text-xs text-fg-dim">{t.scopes.join(", ")}</span>
            <span className="text-xs text-fg-dim">
              {t.last_used ? `used ${new Date(t.last_used).toLocaleDateString()}` : "unused"}
            </span>
            <button
              type="button"
              disabled={revoking === t.token_id}
              onClick={() => void revoke(t.token_id)}
              className="text-danger hover:underline disabled:opacity-40"
            >
              {revoking === t.token_id ? "revoking…" : "revoke"}
            </button>
          </div>
        ))}
      </div>
    </section>
  );
}

/** A small helper for the toolbar link, kept here so the admin-token prompt lives with the surface.
 *  Uses the in-app prompt with a masked input rather than `window.prompt` (issue #29). */
function useAdminTokenPrompt() {
  const { prompt } = useDialogs();
  return async () => {
    const cur = getAdminToken() ?? "";
    const next = await prompt({
      title: "Admin bearer token",
      message: "Presented on admin API calls. Leave blank to clear.",
      initial: cur,
      password: true,
      allowEmpty: true,
      confirmLabel: "Save",
    });
    if (next !== null) setAdminToken(next.trim() || null);
  };
}
