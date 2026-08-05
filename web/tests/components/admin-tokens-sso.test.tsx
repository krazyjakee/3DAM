// The API-token and single-sign-on admin panes, extracted out of Settings.tsx by issue #162. Both
// are mounted directly here — no Administration wrapper — which is the point of the extraction.
//
// What is worth guarding: issuing a token sends the scope set actually ticked and shows the secret
// exactly once, revoking is confirmed and calls out the caller's own session token (don't lock
// yourself out), the OIDC form is seeded from the server but never prefills the write-only secret,
// a blank secret box omits the field entirely (meaning "keep the stored one"), a failed config read
// does not render an empty form that would replace a configuration nobody saw, and the flag notices
// say what is still missing instead of hiding the controls that fix it.

import { HttpResponse, http } from "msw";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test, vi } from "vitest";
import type { TokenInfo } from "../../src/api/admin";
import type { OidcConfigInfo, OidcIdentity } from "../../src/api/types";
import { OidcSection } from "../../src/components/settings/OidcSection";
import { TokensSection } from "../../src/components/settings/TokensSection";
import { renderApp } from "./render";
import { server } from "./server";

function token(id: string, label: string, overrides: Partial<TokenInfo> = {}): TokenInfo {
  return {
    token_id: id,
    label,
    scopes: ["read"],
    created: 0,
    expires: null,
    last_used: null,
    ...overrides,
  };
}

const TOKENS = [token("tok-me", "workstation"), token("tok-ci", "ci-reader")];

const CONFIG: OidcConfigInfo = {
  issuer: "https://accounts.example.com",
  client_id: "3dam-web",
  redirect_url: "http://localhost/api/v1/auth/oidc/callback",
  scopes: ["email", "profile"],
  provisioning: "linked",
  client_secret_set: true,
};

function identity(subject: string, username: string, issuer = CONFIG.issuer): OidcIdentity {
  return { issuer, subject, account_id: `acct-${subject}`, username, linked_at: 0 };
}

const IDENTITIES = [identity("sub-1", "owner"), identity("sub-2", "artist", "https://old.example")];

/** Both OIDC reads fire on mount, so every OidcSection test needs them handled. */
function serveOidc(config: OidcConfigInfo | null = CONFIG, identities = IDENTITIES) {
  server.use(
    http.get("http://localhost/admin/api/oidc", () => HttpResponse.json(config)),
    http.get("http://localhost/admin/api/oidc/identities", () => HttpResponse.json(identities)),
  );
}

function tokenRow(label: string): HTMLElement {
  const row = screen.getByText(label).closest("div");
  if (!row) throw new Error(`no row for ${label}`);
  return row;
}

test("issuing a token sends the ticked scopes and reveals the secret once", async () => {
  const user = userEvent.setup();
  const changed = vi.fn();
  const bodies: unknown[] = [];
  server.use(
    http.post("http://localhost/admin/api/tokens", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json({
        token_id: "tok-new",
        label: "ci-writer",
        scopes: ["read", "mcp_use", "write"],
        secret: "dam_pat_shown_once",
      });
    }),
  );

  renderApp(<TokensSection tokens={TOKENS} currentIdentity={null} onChange={changed} />);

  // No label yet, so the action is unavailable rather than a request that will 400.
  expect(screen.getByRole("button", { name: "Issue token" })).toBeDisabled();

  await user.type(screen.getByPlaceholderText("Label (e.g. ci-reader)"), "ci-writer");
  await user.click(screen.getByRole("checkbox", { name: "write" }));
  await user.click(screen.getByRole("button", { name: "Issue token" }));

  await waitFor(() => expect(changed).toHaveBeenCalledTimes(1));
  expect(bodies).toEqual([
    { label: "ci-writer", scopes: ["read", "mcp_use", "write"], expires: null },
  ]);
  expect(await screen.findByText("dam_pat_shown_once")).toBeInTheDocument();
  // The label field is cleared, so a second click can't silently reissue the same token.
  expect(screen.getByPlaceholderText("Label (e.g. ci-reader)")).toHaveValue("");
});

test("revoking is confirmed, and the caller's own token is flagged before it signs them out", async () => {
  const user = userEvent.setup();
  const revoked: string[] = [];
  server.use(
    http.delete("http://localhost/admin/api/tokens/:id", ({ params }) => {
      revoked.push(String(params.id));
      return new HttpResponse(null, { status: 204 });
    }),
  );

  renderApp(
    <TokensSection tokens={TOKENS} currentIdentity="workstation" onChange={() => {}} />,
  );

  expect(
    within(tokenRow("workstation")).getByTitle("The token this browser is signed in with"),
  ).toBeInTheDocument();
  expect(
    within(tokenRow("ci-reader")).queryByTitle("The token this browser is signed in with"),
  ).toBeNull();

  await user.click(within(tokenRow("workstation")).getByRole("button", { name: "revoke" }));
  const dialog = screen.getByRole("alertdialog", { name: "Revoke token “workstation”?" });
  expect(
    within(dialog).getByText(/sign this browser out immediately/),
  ).toBeInTheDocument();
  await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
  expect(revoked).toHaveLength(0);

  await user.click(within(tokenRow("ci-reader")).getByRole("button", { name: "revoke" }));
  await user.click(
    within(screen.getByRole("alertdialog", { name: "Revoke token “ci-reader”?" })).getByRole(
      "button",
      { name: "Revoke token" },
    ),
  );
  await waitFor(() => expect(revoked).toEqual(["tok-ci"]));
});

test("the SSO pane seeds itself from the server, keeps the secret write-only, and marks stale links", async () => {
  serveOidc();
  renderApp(<OidcSection oidcEnabled accountsEnabled />);

  expect(await screen.findByDisplayValue("https://accounts.example.com")).toBeInTheDocument();
  expect(screen.getByDisplayValue("3dam-web")).toBeInTheDocument();
  expect(screen.getByDisplayValue("email, profile")).toBeInTheDocument();
  // Nothing can prefill the secret — the read shape has no field for it — so the box starts empty
  // and only says that one is on file.
  expect(screen.getByText(/Client secret — stored; leave blank to keep it/)).toBeInTheDocument();
  expect(screen.getByPlaceholderText("••••••")).toHaveValue("");
  // An existing configuration is edited, not added.
  expect(screen.getByRole("button", { name: "Save provider" })).toBeInTheDocument();

  // A link made under a previous issuer authenticates nobody; it has to read as broken.
  expect(await screen.findByText("owner")).toBeInTheDocument();
  expect(screen.getByText("(stale — https://old.example)")).toBeInTheDocument();
});

test("a blank secret box omits the field, so saving keeps the stored secret", async () => {
  const user = userEvent.setup();
  const bodies: Record<string, unknown>[] = [];
  serveOidc();
  server.use(
    http.put("http://localhost/admin/api/oidc", async ({ request }) => {
      bodies.push((await request.json()) as Record<string, unknown>);
      return HttpResponse.json(CONFIG);
    }),
  );

  renderApp(<OidcSection oidcEnabled accountsEnabled />);
  await screen.findByDisplayValue("3dam-web");

  await user.click(screen.getByRole("button", { name: "Save provider" }));
  await waitFor(() => expect(bodies).toHaveLength(1));
  expect(bodies[0]).not.toHaveProperty("client_secret");
  expect(bodies[0]).toMatchObject({
    issuer: CONFIG.issuer,
    client_id: CONFIG.client_id,
    scopes: ["email", "profile"],
    provisioning: "linked",
  });

  // A typed secret does go, and the box is cleared again afterwards.
  await user.type(screen.getByPlaceholderText("••••••"), "s3cret");
  await user.click(screen.getByRole("button", { name: "Save provider" }));
  await waitFor(() => expect(bodies).toHaveLength(2));
  expect(bodies[1]).toMatchObject({ client_secret: "s3cret" });
  await waitFor(() => expect(screen.getByPlaceholderText("••••••")).toHaveValue(""));
});

test("a failed config read reports the failure instead of offering a blank replacement form", async () => {
  server.use(
    http.get("http://localhost/admin/api/oidc", () =>
      HttpResponse.json({ code: "internal", message: "provider store unreadable" }, { status: 500 }),
    ),
    http.get("http://localhost/admin/api/oidc/identities", () => HttpResponse.json([])),
  );

  renderApp(<OidcSection oidcEnabled accountsEnabled />);

  expect(await screen.findByText("provider store unreadable")).toBeInTheDocument();
  expect(screen.queryByPlaceholderText("https://accounts.example.com")).toBeNull();
  expect(screen.queryByRole("button", { name: "Add provider" })).toBeNull();
  // The links half loaded fine and stays usable — one failure doesn't take the section down.
  expect(await screen.findByText("(none linked)")).toBeInTheDocument();
});

test("the flag notices say what is still missing rather than hiding the controls that fix it", async () => {
  serveOidc(null, []);
  const { unmount } = renderApp(<OidcSection oidcEnabled={false} accountsEnabled />);

  expect(
    await screen.findByText(/The Single sign-on capability is still off/),
  ).toBeInTheDocument();
  // Unconfigured: the form is still offered, and it adds rather than saves.
  expect(screen.getByRole("button", { name: "Add provider" })).toBeDisabled();
  unmount();

  serveOidc(null, []);
  renderApp(<OidcSection oidcEnabled={false} accountsEnabled={false} />);
  expect(await screen.findByText(/User accounts are off\./)).toBeInTheDocument();
});
