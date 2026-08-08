// The account/group admin panes (issue #42), extracted out of Settings.tsx by issue #161. They are
// mounted directly here — no Administration wrapper — which is the point of the extraction: the
// panes talk to their parent only through `onChange`/`currentAccountId`.
//
// What is worth guarding: the signed-in row is marked (don't lock yourself out), deletion is
// confirmed rather than immediate, a 409 stays visible on the pane instead of vanishing with a
// toast, and a membership edit PUTs the *whole* member set because the API replaces rather than
// patches.

import { HttpResponse, http } from "msw";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import type { AccountInfo, GroupInfo } from "../../src/api/admin";
import { AccountsAndGroups } from "../../src/components/settings/AccountsSection";
import { GroupsSection } from "../../src/components/settings/GroupsSection";
import { renderApp } from "./render";
import { server } from "./server";

function account(id: string, username: string, overrides: Partial<AccountInfo> = {}): AccountInfo {
  return {
    account_id: id,
    username,
    display_name: null,
    role: "viewer",
    disabled: false,
    created: 0,
    last_login: null,
    ...overrides,
  };
}

function group(id: string, name: string, members: string[] = []): GroupInfo {
  return { group_id: id, name, created: 0, members };
}

const ACCOUNTS = [account("acct-me", "owner", { role: "admin" }), account("acct-other", "artist")];
const GROUPS = [group("grp-1", "Level art", ["acct-other"])];

function serveDirectory(accounts = ACCOUNTS, groups = GROUPS) {
  server.use(
    http.get("http://localhost/admin/api/accounts", () => HttpResponse.json(accounts)),
    http.get("http://localhost/admin/api/groups", () => HttpResponse.json(groups)),
  );
}

/** The username cell carries a `title`; the group membership checkboxes only carry the same text,
 *  so the title is what distinguishes an account row from a membership label. */
function accountRow(username: string): HTMLElement {
  const cell = screen.getByTitle(username);
  const row = cell.closest("div");
  if (!row) throw new Error(`no row for ${username}`);
  return row;
}

test("the loader feeds both panes and flags the signed-in account", async () => {
  serveDirectory();
  renderApp(<AccountsAndGroups currentAccountId="acct-me" />);

  await screen.findByTitle("owner");
  expect(
    within(accountRow("owner")).getByTitle("The account you're signed in with"),
  ).toBeInTheDocument();
  expect(
    within(accountRow("artist")).queryByTitle("The account you're signed in with"),
  ).toBeNull();

  // The group pane renders from the same load, with membership resolved against those accounts.
  expect(await screen.findByText("Level art")).toBeInTheDocument();
  expect(screen.getByRole("checkbox", { name: "artist" })).toBeChecked();
  expect(screen.getByRole("checkbox", { name: "owner" })).not.toBeChecked();
});

test("deleting an account is confirmed, and a server conflict stays visible on the pane", async () => {
  const user = userEvent.setup();
  const deletes: string[] = [];
  serveDirectory();
  server.use(
    http.delete("http://localhost/admin/api/accounts/:id", ({ params }) => {
      deletes.push(String(params.id));
      return HttpResponse.json(
        { code: "conflict", message: "The last admin account cannot be deleted." },
        { status: 409 },
      );
    }),
  );

  renderApp(<AccountsAndGroups currentAccountId="acct-me" />);
  await screen.findByTitle("owner");

  await user.click(within(accountRow("owner")).getByRole("button", { name: "delete" }));
  let dialog = screen.getByRole("alertdialog", { name: "Delete account “owner”?" });
  await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
  expect(deletes).toHaveLength(0);

  await user.click(within(accountRow("owner")).getByRole("button", { name: "delete" }));
  dialog = screen.getByRole("alertdialog", { name: "Delete account “owner”?" });
  await user.click(within(dialog).getByRole("button", { name: "Delete account" }));

  await waitFor(() => expect(deletes).toEqual(["acct-me"]));
  expect(
    await screen.findByText("The last admin account cannot be deleted."),
  ).toBeInTheDocument();
});

test("a membership edit sends the whole member set, not a delta", async () => {
  const user = userEvent.setup();
  const bodies: unknown[] = [];
  server.use(
    http.put("http://localhost/admin/api/groups/grp-1/members", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json(group("grp-1", "Level art", ["acct-other", "acct-me"]));
    }),
  );

  renderApp(<GroupsSection groups={GROUPS} accounts={ACCOUNTS} />);

  await user.click(screen.getByRole("checkbox", { name: "owner" }));

  await waitFor(() => expect(bodies).toHaveLength(1));
  expect(bodies).toEqual([{ account_ids: ["acct-other", "acct-me"] }]);
});

test("groups stay usable when the account load failed", () => {
  renderApp(<GroupsSection groups={GROUPS} accounts={null} />);

  expect(screen.getByText("Level art")).toBeInTheDocument();
  expect(screen.queryByRole("checkbox")).toBeNull();
  expect(
    screen.getByText("Account membership is unavailable while account details cannot be loaded."),
  ).toBeInTheDocument();
});
