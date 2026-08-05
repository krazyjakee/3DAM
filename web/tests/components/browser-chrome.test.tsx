// The Browser's three chrome pieces were extracted into `src/components/browser/` (issue #164).
// Each is pure presentation over `useViewState`/`useSelection`, so each renders on its own here —
// no Browser, no virtualiser, no query page.

import { HttpResponse, http } from "msw";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import type { AssetSummary, Collection, SourceInfo } from "../../src/api/types";
import { Breadcrumb } from "../../src/components/browser/Breadcrumb";
import { SelectionBar } from "../../src/components/browser/SelectionBar";
import { Toolbar } from "../../src/components/browser/Toolbar";
import { SelectionProvider, useSelection } from "../../src/lib/selection";
import { assetSummary } from "./fixtures";
import { renderApp } from "./render";
import { server } from "./server";

const whoami = {
  identity: "test",
  scopes: ["read", "write", "admin", "mcp_use", "federate"],
  anonymous: false,
};

const source: SourceInfo = {
  id: "source-a",
  kind: "local_fs",
  name: "Project",
  uri: "file:///project",
  state: "online",
  stats: { asset_count: 3, last_scanned_at: 1, last_error: null },
  watch: false,
};

const collection: Collection = {
  id: "col-1",
  name: "Hero props",
  kind: "manual",
  count: 0,
  created_at: 1,
  updated_at: 1,
};

function identity() {
  return http.get("http://localhost/api/v1/whoami", () => HttpResponse.json(whoami));
}

test("the breadcrumb renders the source and its path segments and re-scopes on click", async () => {
  const user = userEvent.setup();
  server.use(
    identity(),
    http.get("http://localhost/api/v1/sources", () => HttpResponse.json([source])),
  );

  renderApp(<Breadcrumb />, { route: "/?source=source-a&path=Textures/Wood/" });

  const crumbs = await screen.findByRole("navigation", { name: "Folder path" });
  expect(crumbs).toBeInTheDocument();
  await screen.findByRole("button", { name: "Project" });
  // The trailing crumb is the current folder and is inert.
  expect(screen.getByRole("button", { name: "Wood" })).toBeDisabled();
  expect(screen.getByRole("checkbox")).toBeChecked();

  await user.click(screen.getByRole("button", { name: "Textures" }));
  await waitFor(() => expect(screen.getByRole("button", { name: "Textures" })).toBeDisabled());
  expect(screen.queryByRole("button", { name: "Wood" })).not.toBeInTheDocument();
});

test("the breadcrumb stays hidden outside a source-scoped browse", async () => {
  server.use(
    identity(),
    http.get("http://localhost/api/v1/sources", () => HttpResponse.json([source])),
  );

  renderApp(<Breadcrumb />, { route: "/?col=col-1&source=source-a" });

  expect(screen.queryByRole("navigation", { name: "Folder path" })).not.toBeInTheDocument();
});

test("the toolbar renders the search, sort, and view controls and writes view state", async () => {
  const user = userEvent.setup();
  server.use(identity());

  renderApp(<Toolbar count={12} total={340} />, { route: "/?view=table" });

  const search = screen.getByRole("textbox", { name: "Search assets" });
  expect(screen.getByText("12 / 340")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Table view" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  expect(screen.getByRole("combobox", { name: "Sort order" })).toHaveValue("name:asc");
  // The search-mode selector only appears once text is present.
  expect(screen.queryByRole("combobox", { name: "Search mode" })).not.toBeInTheDocument();

  await user.type(search, "brick");
  await waitFor(() => expect(search).toHaveValue("brick"));
  expect(await screen.findByRole("combobox", { name: "Search mode" })).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Grid view" }));
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "Grid view" })).toHaveAttribute(
      "aria-pressed",
      "true",
    ),
  );

  await user.click(screen.getByRole("button", { name: "Clear search" }));
  await waitFor(() => expect(search).toHaveValue(""));
});

/**
 * Seeds an explicit selection on demand, then hands the live model to the bar under test.
 *
 * The seed is a button rather than a mount effect on purpose: `selectExplicit` publishes state
 * immediately but mirrors focus into the URL via `patch`, and the focus-adoption effect in
 * `lib/selection` reconciles against that URL. Seeding at mount races those two, and the effect
 * collapses the selection to the single adopted focus. Driving it through `user.click` flushes
 * the URL round-trip first — the same pattern the other selection suites use.
 */
function SelectionBarHarness({ assets }: { assets: AssetSummary[] }) {
  const selection = useSelection();
  const { selectExplicit } = selection;
  return (
    <>
      <button type="button" onClick={() => selectExplicit(assets)}>
        Seed selection
      </button>
      <SelectionBar
        selection={selection}
        loaded={assets}
        onSelectVisible={() => {}}
        resultSelector={{ kind: "query", query: { filters: [] } }}
        total={100}
        resultComplete
      />
    </>
  );
}

test("the selection bar summarises a multi-selection and offers the bulk actions", async () => {
  const user = userEvent.setup();
  const assets = [
    assetSummary({ id: "asset-a", name: "Brick.png" }),
    assetSummary({ id: "asset-b", name: "Stone.png" }),
    // A peer-owned reference is read-only and only counts toward the federated tally.
    assetSummary({ id: "asset-c", name: "Remote.png", origin: { peer: "peer-1" } }),
  ];
  server.use(
    identity(),
    http.get("http://localhost/api/v1/collections", () => HttpResponse.json([collection])),
  );

  renderApp(
    <SelectionProvider>
      <SelectionBarHarness assets={assets} />
    </SelectionProvider>,
    { route: "/" },
  );

  await user.click(screen.getByRole("button", { name: "Seed selection" }));

  expect(await screen.findByText("3 selected")).toBeInTheDocument();
  expect(screen.getByText("1 federated (read-only)")).toBeInTheDocument();
  // Every bulk action is behind the write gate, so wait for `whoami` to land before reading them.
  await waitFor(() => expect(screen.getByRole("button", { name: "Analyze" })).toBeEnabled());
  for (const label of ["Convert", "Export", "Retag", "Licence"]) {
    expect(screen.getByRole("button", { name: label })).toBeEnabled();
  }
  expect(screen.getByRole("button", { name: "Select loaded (3)" })).toBeInTheDocument();

  const addTo = await screen.findByRole("combobox", { name: "Add selection to collection" });
  await waitFor(() => expect(addTo).toBeEnabled());
  expect(screen.getByRole("option", { name: "Hero props" })).toBeInTheDocument();

  // The result-wide selector is a server-side handle, not materialised IDs.
  await user.click(screen.getByRole("button", { name: "Select all 100 query results" }));
  expect(await screen.findByText("100 query results selected")).toBeInTheDocument();
  // Explicit-only actions disable once the selection is result-wide.
  await waitFor(() => expect(screen.getByRole("button", { name: "Analyze" })).toBeDisabled());

  await user.click(screen.getByRole("button", { name: "Clear" }));
  expect(await screen.findByText("0 selected")).toBeInTheDocument();
});
