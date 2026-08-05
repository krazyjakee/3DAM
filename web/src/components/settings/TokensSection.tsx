// API tokens (tech-spec 10 §5) — issue a token with a scope set and an optional expiry, and revoke
// one. Extracted out of Settings.tsx by issue #162.
//
// The section is fed its list rather than loading it: the tokens read is part of the one
// administration refresh (status/flags/tokens/audit), and a token write has to re-run that refresh
// anyway — the status card reports the token count — so the parent stays the single owner of the
// fetch and this module reports back through `onChange`.

import { useState } from "react";
import { admin, type NewTokenReply, type Scope, type TokenInfo } from "@/api/admin";
import { useDialogs } from "@/lib/dialogs";
import { errorMessage, toast } from "@/lib/toast";

/** Every scope a token may carry — the checkbox set offered when issuing one. */
const ALL_SCOPES: Scope[] = ["read", "write", "admin", "mcp_use", "federate"];

export function TokensSection({
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
