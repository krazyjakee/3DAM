// User accounts (issue #42) — the admin pane over `/admin/api/accounts`, plus the small loader that
// feeds both this pane and the group pane. Only mounted while the `user_accounts` flag is on; the
// routes 404 while it is off.

import { useCallback, useEffect, useState } from "react";
import { admin, type AccountInfo, type GroupInfo } from "@/api/admin";
import type { AccountRole } from "@/api/types";
import { useDialogs } from "@/lib/dialogs";
import { errorMessage, toast } from "@/lib/toast";
import { Choice, Toggle } from "./Controls";
import { AdminSectionState, DependencyNote } from "./SectionState";
import { GroupsSection } from "./GroupsSection";

const ROLES: AccountRole[] = ["admin", "editor", "viewer"];

/** Loads accounts + groups once (they cross-reference: group membership lists accounts) and feeds
 *  both admin panes. Only mounted while the `user_accounts` flag is on — the routes 404 off. */
export function AccountsAndGroups({ currentAccountId }: { currentAccountId: string | null }) {
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

export function AccountsSection({
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
