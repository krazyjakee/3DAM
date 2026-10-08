import { HttpResponse, http } from "msw";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import type { QueryRequest, SourceInfo } from "../../src/api/types";
import { Browser } from "../../src/components/Browser";
import { Navigation } from "../../src/components/Navigation";
import { SelectionProvider } from "../../src/lib/selection";
import { ThemeProvider } from "../../src/lib/theme";
import { assetPage, assetSummary } from "./fixtures";
import { renderApp } from "./render";
import { server } from "./server";

const sources: SourceInfo[] = [
  {
    id: "source-a", kind: "local_fs", name: "Project", uri: "file:///project",
    state: "online", watch: false,
    stats: { asset_count: 1, last_scanned_at: null, last_error: null },
  },
  {
    id: "source-b", kind: "federated", name: "Studio peer", uri: "https://studio.example",
    state: "online", watch: false,
    stats: { asset_count: 0, last_scanned_at: null, last_error: null },
  },
];
const local = assetSummary({ id: "local", name: "Local.png" });
const remote = assetSummary({
  id: "remote", name: "Remote.png", source_id: "source-b", origin: { peer: "studio" },
});

function workspace(route = "/?view=table") {
  const queries: QueryRequest[] = [];
  const statsScopes: (string | null)[] = [];
  server.use(
    http.get("http://localhost/api/version", () =>
      HttpResponse.json({ api: "v1", server: "test", capabilities: [], auth: "off" })),
    http.get("http://localhost/api/v1/whoami", () =>
      HttpResponse.json({ identity: "test", scopes: ["read", "write"], anonymous: false })),
    http.get("http://localhost/api/v1/sources", () => HttpResponse.json(sources)),
    http.get("http://localhost/api/v1/collections", () => HttpResponse.json([])),
    http.get("http://localhost/api/v1/stats", ({ request }) => {
      statsScopes.push(new URL(request.url).searchParams.get("source"));
      return HttpResponse.json({
        total: 2, by_media: { image: 2 }, by_source: {}, tags: {}, unanalyzed: 0, sources: 2,
      });
    }),
    http.post("http://localhost/api/v1/query", async ({ request }) => {
      const query = await request.json() as QueryRequest;
      queries.push(query);
      const filter = query.filters?.find((item) => item.field === "source");
      const id = filter && "str" in filter.value ? filter.value.str : null;
      return HttpResponse.json(assetPage(
        [local, remote].filter((item) => !id || item.source_id === id),
      ));
    }),
    http.post("http://localhost/api/v1/duplicates/membership", () => HttpResponse.json([])),
    http.post("http://localhost/api/v1/prefetch", () => new HttpResponse(null, { status: 204 })),
    http.post("http://localhost/api/v1/folders", () => HttpResponse.json([
      { path: "Textures/", name: "Textures", asset_count: 1, has_children: false },
    ])),
  );
  renderApp(
    <ThemeProvider>
      <SelectionProvider>
        <Navigation />
        <Browser />
      </SelectionProvider>
    </ThemeProvider>,
    { route },
  );
  return { queries, statsScopes };
}

test("the library combines local and peer assets, narrowing only through search filters", async () => {
  const user = userEvent.setup();
  const { queries, statsScopes } = workspace();
  await screen.findByRole("button", { name: /Local\.png/ });
  await screen.findByRole("button", { name: /Remote\.png/ });
  expect(queries[0]?.filters).not.toContainEqual(expect.objectContaining({ field: "source" }));
  const navigation = screen.getByRole("navigation", { name: "Library navigation" });
  expect(within(navigation).queryByRole("button", { name: /Project|Studio peer/ })).toBeNull();
  expect(within(navigation).getByText("2 assets")).toBeVisible();

  await user.click(screen.getByRole("button", { name: "Advanced filters" }));
  const picker = await screen.findByRole("combobox", { name: "Source" });
  expect(picker).toHaveValue("");
  await user.selectOptions(picker, "source-b");
  await waitFor(() => expect(screen.queryByRole("button", { name: /Local\.png/ })).toBeNull());
  expect(screen.getByRole("button", { name: /Remote\.png/ })).toBeVisible();
  expect(screen.getByText("Source: Studio peer")).toBeVisible();
  expect(queries.at(-1)?.filters).toContainEqual({
    field: "source", op: "eq", value: { str: "source-b" },
  });
  expect(screen.queryByRole("region", { name: "Folder filter" })).toBeNull();
  expect(within(navigation).getByText("2 assets")).toBeVisible();
  expect(statsScopes.length).toBeGreaterThan(0);
  expect(statsScopes.every((scope) => scope === null)).toBe(true);

  await user.selectOptions(picker, "");
  await screen.findByRole("button", { name: /Local\.png/ });
  expect(screen.getByRole("button", { name: /Remote\.png/ })).toBeVisible();
  expect(screen.queryByText("Source: Studio peer")).toBeNull();
});

test("folder filtering is explicit and changing sources clears its relative path", async () => {
  const user = userEvent.setup();
  const { queries } = workspace();
  await screen.findByRole("button", { name: /Local\.png/ });
  await user.click(screen.getByRole("button", { name: "Advanced filters" }));
  const picker = await screen.findByRole("combobox", { name: "Source" });
  await user.selectOptions(picker, "source-a");
  const folders = await screen.findByRole("region", { name: "Folder filter" });
  await user.click(await within(folders).findByTitle("Textures/"));
  await waitFor(() => expect(queries.at(-1)?.filters).toContainEqual({
    field: "path", op: "eq", value: { str: "Textures/" },
  }));
  expect(screen.getByText("Folder: Textures/ (including subfolders)")).toBeVisible();

  await user.selectOptions(picker, "source-b");
  await waitFor(() => expect(queries.at(-1)?.filters).toEqual([
    { field: "source", op: "eq", value: { str: "source-b" } },
  ]));
  expect(screen.queryByRole("navigation", { name: "Folder path" })).toBeNull();
  await user.click(screen.getByRole("button", { name: "Clear advanced" }));
  expect(picker).toHaveValue("");
  await screen.findByRole("button", { name: /Local\.png/ });
});

test("All assets restores the combined library from a bookmarked source search", async () => {
  const user = userEvent.setup();
  workspace("/?view=table&source=source-b&media=image&fav=1");
  await screen.findByRole("button", { name: /Remote\.png/ });
  expect(screen.queryByRole("button", { name: /Local\.png/ })).toBeNull();
  expect(screen.queryByRole("navigation", { name: "Folder path" })).toBeNull();
  await user.click(screen.getByRole("button", { name: /All assets/ }));
  await screen.findByRole("button", { name: /Local\.png/ });
  expect(screen.getByRole("button", { name: /Remote\.png/ })).toBeVisible();
  expect(screen.queryByRole("region", { name: "Active filters" })).toBeNull();
});
