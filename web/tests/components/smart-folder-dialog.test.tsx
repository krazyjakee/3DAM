import { HttpResponse, http } from "msw";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import { Browser } from "../../src/components/Browser";
import { SmartFolderDialog } from "../../src/components/SmartFolderDialog";
import type { NewCollection, QueryRequest, UpdateCollection } from "../../src/api/types";
import { SelectionProvider } from "../../src/lib/selection";
import { assetPage } from "./fixtures";
import { renderApp } from "./render";
import { server } from "./server";

const whoami = {
  identity: "test",
  scopes: ["read", "write", "admin", "mcp_use", "federate"],
  anonymous: false,
};

test("creates and updates a smart folder after a human-readable preview", async () => {
  const user = userEvent.setup();
  const query: QueryRequest = {
    text: "kick",
    filters: [{ field: "tag", op: "eq", value: { str: "punchy" } }],
    sort: { field: "relevance", dir: "asc" },
  };
  const creates: NewCollection[] = [];
  const updates: { id: string; patch: UpdateCollection }[] = [];
  server.use(
    http.get("http://localhost/api/v1/collections", () =>
      HttpResponse.json([
        {
          id: "smart-1",
          name: "Old name",
          kind: "smart",
          query: { filters: [] },
          count: null,
          created_at: 1,
          updated_at: 1,
        },
      ]),
    ),
    http.get("http://localhost/api/v1/sources", () => HttpResponse.json([])),
    http.post("http://localhost/api/v1/collections", async ({ request }) => {
      creates.push((await request.json()) as NewCollection);
      return HttpResponse.json({ id: "smart-new" });
    }),
    http.put("http://localhost/api/v1/collections/:id", async ({ params, request }) => {
      updates.push({ id: String(params.id), patch: (await request.json()) as UpdateCollection });
      return new HttpResponse(null, { status: 204 });
    }),
  );

  const first = renderApp(<SmartFolderDialog query={query} onClose={() => {}} />);
  expect(await screen.findByText(/Keywords: “kick”/)).toBeInTheDocument();
  expect(screen.getByText(/Tag is punchy/)).toBeInTheDocument();
  await user.type(screen.getByLabelText("Name"), "Punchy kicks");
  await user.click(screen.getByRole("button", { name: "Create smart folder" }));
  await waitFor(() => expect(creates).toHaveLength(1));
  expect(creates[0]).toEqual({ name: "Punchy kicks", kind: "smart", query });
  first.unmount();

  renderApp(<SmartFolderDialog query={query} onClose={() => {}} />);
  await screen.findByRole("radio", { name: "Update existing" });
  await user.click(screen.getByRole("radio", { name: "Update existing" }));
  expect(screen.getByLabelText("Name")).toHaveValue("Old name");
  await user.clear(screen.getByLabelText("Name"));
  await user.type(screen.getByLabelText("Name"), "Renamed and live");
  await user.click(screen.getByRole("button", { name: "Replace query" }));
  await waitFor(() => expect(updates).toHaveLength(1));
  expect(updates[0]).toEqual({
    id: "smart-1",
    patch: { name: "Renamed and live", query },
  });
});

test("Browser saves the complete current faceted query", async () => {
  const user = userEvent.setup();
  let created: NewCollection | null = null;
  server.use(
    http.get("http://localhost/api/v1/whoami", () => HttpResponse.json(whoami)),
    http.get("http://localhost/api/v1/sources", () =>
      HttpResponse.json([
        {
          id: "peer-1",
          kind: "federated",
          name: "Remote studio",
          uri: "https://peer.invalid",
          state: "online",
          stats: { asset_count: 0, last_scanned_at: null, last_error: null },
          watch: false,
          writable: false,
        },
      ]),
    ),
    http.get("http://localhost/api/v1/collections", () => HttpResponse.json([])),
    http.post("http://localhost/api/v1/query", () => HttpResponse.json(assetPage([]))),
    http.post("http://localhost/api/v1/collections", async ({ request }) => {
      created = (await request.json()) as NewCollection;
      return HttpResponse.json({ id: "smart-new" });
    }),
  );
  const advanced = encodeURIComponent(
    JSON.stringify([{ field: "bpm", op: "range", value: { range: [100, 130] } }]),
  );
  renderApp(
    <SelectionProvider>
      <Browser />
    </SelectionProvider>,
    {
      route: `/?q=kick&media=audio&source=peer-1&license=permissive&tag=drum&path=Kits%2F&sub=0&adv=${advanced}&sort=size&dir=desc&mode=hybrid`,
    },
  );

  await user.click(await screen.findByRole("button", { name: "Save search as smart folder" }));
  expect(await screen.findByText(/Source is Remote studio/)).toBeInTheDocument();
  expect(screen.getByText(/Folder is Kits\//)).toBeInTheDocument();
  await user.type(screen.getByLabelText("Name"), "Remote kicks");
  await user.click(screen.getByRole("button", { name: "Create smart folder" }));
  await waitFor(() => expect(created).not.toBeNull());
  expect(created).toEqual({
    name: "Remote kicks",
    kind: "smart",
    query: {
      text: "kick",
      mode: "hybrid",
      sort: { field: "size", dir: "desc" },
      filters: [
        { field: "media_type", op: "eq", value: { str: "audio" } },
        { field: "source", op: "eq", value: { str: "peer-1" } },
        { field: "license", op: "eq", value: { str: "permissive" } },
        { field: "tag", op: "eq", value: { str: "drum" } },
        { field: "folder", op: "eq", value: { str: "Kits/" } },
        { field: "bpm", op: "range", value: { range: [100, 130] } },
      ],
    },
  });
});

test("an opened smart folder visibly warns when a saved source was removed", async () => {
  server.use(
    http.get("http://localhost/api/v1/whoami", () => HttpResponse.json(whoami)),
    http.get("http://localhost/api/v1/sources", () => HttpResponse.json([])),
    http.get("http://localhost/api/v1/collections", () =>
      HttpResponse.json([
        {
          id: "smart-gone",
          name: "Old peer search",
          kind: "smart",
          query: {
            filters: [{ field: "source", op: "eq", value: { str: "removed-peer" } }],
          },
          count: null,
          created_at: 1,
          updated_at: 1,
        },
      ]),
    ),
    http.post("http://localhost/api/v1/collections/smart-gone/assets", () =>
      HttpResponse.json(assetPage([])),
    ),
  );
  renderApp(
    <SelectionProvider>
      <Browser />
    </SelectionProvider>,
    { route: "/?col=smart-gone" },
  );

  expect(await screen.findByRole("alert")).toHaveTextContent(/source.*no longer available/i);
});
