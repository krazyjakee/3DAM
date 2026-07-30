// Sharing grants for one resource (user accounts, issue #42): the admin-only modal behind the
// "Share…" affordance on source/collection rows. Lists the current grants (the flat /admin/api/
// shares list, filtered client-side to this resource), adds a grant to an account or a group with
// read/write access, and revokes. Only mounted when the caller is an admin and the `user_accounts`
// flag is on — the routes 404 otherwise.

import { useCallback, useEffect, useState } from "react";
import { Share2, Trash2 } from "lucide-react";
import {
  admin,
  type AccountInfo,
  type GroupInfo,
  type ShareAccess,
  type ShareInfo,
  type ShareResource,
} from "@/api/admin";
import { Modal } from "@/lib/dialogs";
import { errorMessage, toast } from "@/lib/toast";

export function ShareDialog({
  resource,
  resourceId,
  resourceName,
  onClose,
}: {
  resource: ShareResource;
  resourceId: string;
  /** The source/collection's display name — for the title only. */
  resourceName: string;
  onClose: () => void;
}) {
  const [shares, setShares] = useState<ShareInfo[] | null>(null);
  const [accounts, setAccounts] = useState<AccountInfo[]>([]);
  const [groups, setGroups] = useState<GroupInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // The add-grant controls: target kind + id, and the access level.
  const [kind, setKind] = useState<"account" | "group">("account");
  const [target, setTarget] = useState("");
  const [access, setAccess] = useState<ShareAccess>("read");

  const load = useCallback(async () => {
    try {
      const [s, a, g] = await Promise.all([admin.shares(), admin.accounts(), admin.groups()]);
      setShares(s);
      setAccounts(a);
      setGroups(g);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  // The shares list is flat across the library; this dialog owns one resource.
  const grants = (shares ?? []).filter(
    (s) => s.resource === resource && s.resource_id === resourceId,
  );

  const targets = kind === "account" ? accounts : groups;
  const targetId = (t: AccountInfo | GroupInfo) =>
    "account_id" in t ? t.account_id : t.group_id;
  const targetName = (t: AccountInfo | GroupInfo) => ("username" in t ? t.username : t.name);

  /** Resolve a grant's account/group id back to a name for display. */
  const grantLabel = (s: ShareInfo): string => {
    if (s.account_id) {
      const a = accounts.find((x) => x.account_id === s.account_id);
      return a ? a.username : "(deleted account)";
    }
    const g = groups.find((x) => x.group_id === s.group_id);
    return g ? `${g.name} (group)` : "(deleted group)";
  };

  const add = async () => {
    if (!target || busy) return;
    setBusy(true);
    setError(null);
    try {
      await admin.createShare({
        resource,
        resource_id: resourceId,
        ...(kind === "account" ? { account_id: target } : { group_id: target }),
        access,
      });
      setTarget("");
      await load();
      toast.success("Share granted");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const revoke = async (s: ShareInfo) => {
    setBusy(true);
    setError(null);
    try {
      await admin.deleteShare(s.share_id);
      await load();
      toast.success("Share revoked");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={`Share “${resourceName}”`}
      icon={<Share2 size={14} className="text-accent" />}
      labelledBy="share-dialog-title"
      onClose={onClose}
      wide
      scroll
    >
      <p className="mb-3 text-xs text-fg-dim">
        Grant accounts or groups access to this {resource}. Without a grant it stays visible to
        admins only.
      </p>

      {error && <p className="mb-2 text-xs text-danger">{error}</p>}

      {/* current grants */}
      <div className="mb-3 rounded border border-border">
        {shares === null ? (
          <div className="px-3 py-2 text-xs text-fg-dim">Loading…</div>
        ) : grants.length === 0 ? (
          <div className="px-3 py-2 text-xs text-fg-dim">(not shared with anyone)</div>
        ) : (
          grants.map((s) => (
            <div
              key={s.share_id}
              className="flex items-center gap-2 border-b border-border px-3 py-1.5 text-xs last:border-0"
            >
              <span className="min-w-0 flex-1 truncate font-medium">{grantLabel(s)}</span>
              <span className="text-fg-dim">{s.access}</span>
              <button
                type="button"
                className="flex items-center justify-center text-fg-dim hover:text-danger disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
                aria-label={`Revoke share for ${grantLabel(s)}`}
                title="Revoke"
                disabled={busy}
                onClick={() => void revoke(s)}
              >
                <Trash2 size={12} />
              </button>
            </div>
          ))
        )}
      </div>

      {/* add a grant */}
      <div className="flex flex-wrap items-center gap-2">
        <select
          className="field w-auto"
          aria-label="Share with an account or a group"
          value={kind}
          onChange={(e) => {
            setKind(e.target.value as "account" | "group");
            setTarget("");
          }}
        >
          <option value="account">account</option>
          <option value="group">group</option>
        </select>
        <select
          className="field min-w-0 flex-1"
          aria-label={kind === "account" ? "Account to share with" : "Group to share with"}
          value={target}
          onChange={(e) => setTarget(e.target.value)}
        >
          <option value="">{targets.length === 0 ? `(no ${kind}s)` : `Choose a ${kind}…`}</option>
          {targets.map((t) => (
            <option key={targetId(t)} value={targetId(t)}>
              {targetName(t)}
            </option>
          ))}
        </select>
        <select
          className="field w-auto"
          aria-label="Access level"
          value={access}
          onChange={(e) => setAccess(e.target.value as ShareAccess)}
        >
          <option value="read">read</option>
          <option value="write">write</option>
        </select>
        <button
          type="button"
          className="btn btn-accent disabled:opacity-40"
          disabled={!target || busy}
          onClick={() => void add()}
        >
          {busy ? "Working…" : "Grant"}
        </button>
      </div>
    </Modal>
  );
}
