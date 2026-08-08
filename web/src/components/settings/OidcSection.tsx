// Single sign-on (issue #41) — the OIDC/OAuth2 provider configuration and the identity links that
// say which provider subject signs in as which account. Extracted out of Settings.tsx by issue #162.
//
// Unlike the token pane, this section loads its own two reads: they are gated on the `oidc` flag's
// presence, so the parent's blanket administration refresh must not issue them (a server without
// the routes would answer the SPA fallback), and nothing else on the screen depends on them.

import { useEffect, useState } from "react";
import {
  useAdminOidcConfig,
  useAdminOidcIdentities,
  useLinkAdminOidcIdentity,
  useSetAdminOidcConfig,
  useUnlinkAdminOidcIdentity,
} from "@/api/admin-queries";
import type { OidcConfigInfo, OidcIdentity, OidcProvisioning } from "@/api/types";
import { useDialogs } from "@/lib/dialogs";
import { errorMessage, toast } from "@/lib/toast";
import { SectionError, SectionLoading } from "./SectionState";

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
export function OidcSection({
  oidcEnabled,
  accountsEnabled,
}: {
  oidcEnabled: boolean;
  accountsEnabled: boolean;
}) {
  const { confirm } = useDialogs();
  const configQuery = useAdminOidcConfig();
  const identitiesQuery = useAdminOidcIdentities();
  const saveMutation = useSetAdminOidcConfig();
  const linkMutation = useLinkAdminOidcIdentity();
  const unlinkMutation = useUnlinkAdminOidcIdentity();
  const cfg: OidcConfigInfo | null = configQuery.data ?? null;
  const identities = identitiesQuery.data ?? null;
  const configError = configQuery.error ? errorMessage(configQuery.error) : null;
  const identitiesError = identitiesQuery.error ? errorMessage(identitiesQuery.error) : null;
  // Success — including a `null` payload — distinguishes "no provider" from a failed read. Saving
  // stays unavailable until the query has actually answered, so a blank form cannot replace an
  // unseen configuration.
  const loaded = configQuery.isSuccess;
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

  // Query data seeds editable UI state; the client secret is deliberately absent from the read
  // shape and therefore can never be copied back into the form.
  useEffect(() => {
    if (!configQuery.data) return;
    setIssuer(configQuery.data.issuer);
    setClientId(configQuery.data.client_id);
    setRedirectUrl(configQuery.data.redirect_url);
    setScopes(configQuery.data.scopes.join(", "));
    setProvisioning(configQuery.data.provisioning);
  }, [configQuery.data]);

  const save = async () => {
    setSaving(true);
    try {
      await saveMutation.mutateAsync({
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
      setSecret("");
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
      await linkMutation.mutateAsync({
        subject: linkSubject.trim(),
        account_id: linkAccountId.trim(),
      });
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
      await unlinkMutation.mutateAsync({ subject: i.subject, issuer: i.issuer });
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
