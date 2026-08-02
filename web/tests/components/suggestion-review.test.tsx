import { HttpResponse, http } from "msw";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import type { SuggestionReview } from "../../src/api/types";
import { qk } from "../../src/api/queries";
import { Inspector } from "../../src/components/Inspector";
import { asset } from "./fixtures";
import { renderApp, testQueryClient } from "./render";
import { server } from "./server";

test("automatic suggestions expose pending decisions, confidence, why, and reversible controls", async () => {
  const user = userEvent.setup();
  const storedAsset = asset({
    tags: [
      {
        name: "texture",
        state: "pending",
        source: "auto",
        confidence: 0.82,
        why: "Image analysis matched visual and tiling signals.",
      },
      {
        name: "rigged",
        state: "confirmed",
        source: "auto",
        confidence: 0.95,
        why: "Model metadata reports a rig.",
      },
      {
        name: "photo",
        state: "rejected",
        source: "auto",
        confidence: 0.61,
        why: "The visual classifier matched photo features.",
      },
    ],
  });
  const client = testQueryClient();
  client.setQueryData(qk.asset("asset-a"), storedAsset);
  client.setQueryData(qk.version, {
    api: "v1",
    server: "test",
    capabilities: [],
    auth: "off",
    accounts: false,
  });
  client.setQueryData([...qk.whoami, "", "session"], {
    identity: "test",
    scopes: ["read", "write", "admin", "mcp_use", "federate"],
    anonymous: false,
  });
  client.setQueryData(qk.sources, []);
  client.setQueryData(qk.collections, []);
  client.setQueryData([...qk.duplicates, "asset", "asset-a"], null);

  const reviews: SuggestionReview[] = [];
  server.use(
    http.post("http://localhost/api/v1/tags/list", () => HttpResponse.json([])),
    http.get("http://localhost/api/v1/assets/asset-a", () => HttpResponse.json(storedAsset)),
    http.get("http://localhost/api/v1/assets/asset-a/content", () =>
      new HttpResponse(new Uint8Array([1, 2, 3]), {
        headers: { "content-type": "image/png" },
      }),
    ),
    http.post("http://localhost/api/v1/suggestions/review", async ({ request }) => {
      reviews.push((await request.json()) as SuggestionReview);
      return new HttpResponse(null, { status: 204 });
    }),
  );

  renderApp(
    <Inspector
      open={false}
      onClose={() => {}}
      collapsed={false}
      onCollapse={() => {}}
      onExpand={() => {}}
    />,
    { route: "/?sel=asset-a", client },
  );

  expect(screen.getAllByText("Only accepted suggestions affect search and filters.")[0]).toBeVisible();
  expect(screen.getAllByText("82% confidence")[0]).toBeVisible();
  expect(screen.getAllByText(/Why: Image analysis matched visual/)[0]).toBeVisible();
  expect(screen.getAllByText("Pending")[0]).toBeVisible();
  expect(screen.getAllByText("Accepted")[0]).toBeVisible();
  expect(screen.getAllByText("Rejected")[0]).toBeVisible();

  const pending = screen.getAllByRole("article", { name: /texture, pending, 82% confidence/i })[0];
  pending.focus();
  await user.keyboard("y");
  await waitFor(() =>
    expect(reviews).toContainEqual({ asset: "asset-a", tag: "texture", action: "accept" }),
  );
  await user.click(screen.getAllByRole("button", { name: "Reject suggestion texture" })[0]);
  await waitFor(() =>
    expect(reviews).toContainEqual({ asset: "asset-a", tag: "texture", action: "reject" }),
  );

  await user.click(screen.getAllByRole("button", { name: "Undo confirmed suggestion rigged" })[0]);
  await waitFor(() =>
    expect(reviews).toContainEqual({ asset: "asset-a", tag: "rigged", action: "undo" }),
  );

  const rejected = screen.getAllByRole("article", { name: /photo, rejected, 61% confidence/i })[0];
  rejected.focus();
  await user.keyboard("u");
  await waitFor(() =>
    expect(reviews).toContainEqual({ asset: "asset-a", tag: "photo", action: "undo" }),
  );
});
