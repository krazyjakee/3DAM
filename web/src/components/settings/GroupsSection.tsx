// Groups (issue #42) — the admin pane over `/admin/api/groups`. Membership is edited against the
// account list loaded alongside it; `accounts === null` means that sibling load failed, and the
// pane says so rather than pretending a group has no members.

import { useState } from "react";
import { admin, type AccountInfo, type GroupInfo } from "@/api/admin";
import { useDialogs } from "@/lib/dialogs";
import { errorMessage, toast } from "@/lib/toast";

export function GroupsSection({
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
