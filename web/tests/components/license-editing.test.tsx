// Licence editing + usage-right filtering (issue #106).
//
// Three things are worth a regression guard here, and they are all about *absence* being meaningful:
// an unknown licence must render rather than vanish, an untouched bulk field must not reach the
// wire, and the safe-to-ship query must survive the trip into a smart folder unchanged.

import { fireEvent, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, test } from "vitest";
import { qk } from "../../src/api/queries";
import { ActiveFilters } from "../../src/components/ActiveFilters";
import { AdvancedSearch } from "../../src/components/AdvancedSearch";
import { Inspector } from "../../src/components/Inspector";
import { LicenseBadge } from "../../src/components/LicenseBadge";
import { LicenseDialog } from "../../src/components/LicenseDialog";
import { SmartFolderDialog } from "../../src/components/SmartFolderDialog";
import type { NewCollection, QueryRequest, SetLicenseRequest } from "../../src/api/types";
import { savedQuery } from "../../src/lib/query-summary";
import { useViewState } from "../../src/lib/view-state";
import { asset, assetSummary } from "./fixtures";
import { renderApp, testQueryClient } from "./render";
import { server } from "./server";

/** Inspector needs a warm cache to render a detail panel without a network round trip. */
function inspectorClient(detail: ReturnType<typeof asset>) {
  const client = testQueryClient();
  client.setQueryData(qk.asset("asset-a"), detail);
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
  return client;
}

function renderInspector(
  client: ReturnType<typeof testQueryClient>,
  detail: ReturnType<typeof asset>,
) {
  server.use(
    http.post("http://localhost/api/v1/tags/list", () => HttpResponse.json([])),
    // A successful licence write invalidates the detail cache; serve the refetch it triggers.
    http.get("http://localhost/api/v1/assets/asset-a", () => HttpResponse.json(detail)),
    http.get(
      "http://localhost/api/v1/assets/asset-a/content",
      () =>
        new HttpResponse(new Uint8Array([1, 2, 3]), {
          headers: { "content-type": "image/png" },
        }),
    ),
  );
  return renderApp(
    <Inspector
      open={false}
      onClose={() => {}}
      collapsed={false}
      onCollapse={() => {}}
      onExpand={() => {}}
    />,
    { route: "/?sel=asset-a", client },
  );
}

test("an unknown licence is visible in compact contexts, not silently dropped", () => {
  renderApp(<LicenseBadge badge={{ id: null, status: "unknown" }} />);
  // The grid/table dot used to render nothing at all here, making "nobody established what you may
  // do with this" indistinguishable from "fine" (DESIGN_GUIDELINES §3.1).
  const badge = screen.getByTitle(/License unknown/i);
  expect(badge).toBeInTheDocument();
  expect(badge).toHaveTextContent("Unknown");
});

test("a named licence with unestablished rights is labelled unverified, not settled", () => {
  renderApp(<LicenseBadge badge={{ id: "CC-BY-4.0", status: "unknown" }} prominent />);
  expect(screen.getByTitle(/rights not established/i)).toHaveTextContent("CC-BY-4.0");
});

test("the inspector edits every rights field three-state and shows the server's derived status", async () => {
  const requests: SetLicenseRequest[] = [];
  server.use(
    http.post("http://localhost/api/v1/assets/license", async ({ request }) => {
      requests.push((await request.json()) as SetLicenseRequest);
      return HttpResponse.json({
        matched: 1,
        changed: 1,
        status: [{ status: "attribution", count: 1 }],
        warnings: [],
      });
    }),
  );

  const detail = asset();
  renderInspector(inspectorClient(detail), detail);
  fireEvent.click(screen.getByRole("button", { name: "Edit licence" }));

  // A preset is a *client-side* pre-fill: it lands in the visible controls for review, and the
  // server is never asked to infer rights from an identifier (ADR 0009 §1).
  fireEvent.change(screen.getByLabelText("Pre-fill from a known licence"), {
    target: { value: "CC-BY-4.0" },
  });
  expect(screen.getByLabelText("Commercial use")).toHaveValue("yes");
  expect(screen.getByLabelText("Attribution required")).toHaveValue("yes");

  // Every right is three-state; "unknown" is a value the user can choose, not the absence of one.
  fireEvent.change(screen.getByLabelText("Redistribute"), { target: { value: "unknown" } });
  fireEvent.change(screen.getByLabelText("Rights holder"), { target: { value: "Studio X" } });
  fireEvent.click(screen.getByRole("button", { name: "Save licence" }));

  await waitFor(() => expect(requests).toHaveLength(1));
  expect(requests[0]).toEqual({
    assets: ["asset-a"],
    license: {
      id: "CC-BY-4.0",
      commercial: true,
      modify: true,
      redistribute: null,
      attribution: true,
      holder: "Studio X",
      credit: null,
      url: null,
    },
  });
  // The badge shown afterwards is the one the server derived, not one guessed here.
  expect(await screen.findByText(/Saved — status now/)).toBeInTheDocument();
  expect(screen.getByTitle("License: Attribution")).toBeInTheDocument();
});

test("federated licence metadata is read-only and attributed to its peer", () => {
  const federated = asset({
    summary: assetSummary({ origin: { peer: "studio-peer" } }),
  });
  renderInspector(inspectorClient(federated), federated);
  const edit = screen.getByRole("button", { name: "Edit licence" });
  expect(edit).toBeDisabled();
  expect(edit).toHaveAttribute("title", expect.stringMatching(/studio-peer/));
  expect(screen.getAllByText(/Licence recorded by peer “studio-peer”/).length).toBeGreaterThan(0);
});

test("bulk licence edit previews, applies, and sends only the fields the user touched", async () => {
  const requests: SetLicenseRequest[] = [];
  server.use(
    http.post("http://localhost/api/v1/assets/license", async ({ request }) => {
      const body = (await request.json()) as SetLicenseRequest;
      requests.push(body);
      return HttpResponse.json({
        matched: 3,
        changed: 2,
        status: [
          { status: "attribution", count: 2 },
          { status: "unknown", count: 1 },
        ],
        warnings: [
          {
            subject: "peer-asset",
            code: "target_read_only",
            message: "Asset is readable but requires a write share",
          },
        ],
      });
    }),
  );

  renderApp(
    <LicenseDialog scope={{ assets: ["local"] }} excludedPeers={1} onClose={() => {}} />,
  );
  expect(screen.getByText(/1 federated target is read-only and excluded/i)).toBeInTheDocument();

  // Touch exactly two fields: the identifier and one right. Everything else stays "leave unchanged".
  fireEvent.change(screen.getByLabelText("Licence identifier action"), {
    target: { value: "set" },
  });
  fireEvent.change(screen.getByLabelText("Licence identifier"), {
    target: { value: " CC-BY-4.0 " },
  });
  fireEvent.change(screen.getByLabelText("Attribution required"), { target: { value: "yes" } });

  fireEvent.click(screen.getByRole("button", { name: "Preview" }));
  expect(await screen.findByText(/Preview: 2 of 3 assets change/i)).toBeInTheDocument();
  expect(screen.getByText(/requires a write share/i)).toBeInTheDocument();

  fireEvent.click(screen.getByRole("button", { name: "Apply" }));
  expect(await screen.findByText(/Applied: 2 of 3 assets change/i)).toBeInTheDocument();
  await waitFor(() => expect(requests).toHaveLength(2));

  expect(requests[0]).toEqual({
    assets: ["local"],
    license: { id: "CC-BY-4.0", attribution: true },
    dry_run: true,
  });
  expect(requests[1]).toEqual({
    assets: ["local"],
    license: { id: "CC-BY-4.0", attribution: true },
    dry_run: false,
  });
  // The untouched rights are absent, not `null`: "leave this alone" and "make this unknown" are
  // different instructions, and only one of them is what the user asked for.
  expect("commercial" in requests[0].license).toBe(false);
  expect("holder" in requests[0].license).toBe(false);
});

/** Renders the filter popover next to the `QueryRequest` it produces, so a test can assert on the
 *  exact query the Browser would send and the smart-folder dialog would save. */
/** Both halves, because the chip row is where a filter's *direction* becomes visible: the popover
 *  can show "Denied" on a control the user is looking at, but `ActiveFilters` is the only surface
 *  that states the claim once the popover closes. */
function FilterHarness({ onQuery }: { onQuery: (query: QueryRequest) => void }) {
  const { request } = useViewState();
  onQuery(request);
  return (
    <>
      <AdvancedSearch />
      <ActiveFilters onClearAll={() => {}} />
    </>
  );
}

test("the safe-to-ship query is expressible and round-trips into a smart folder", async () => {
  const user = userEvent.setup();
  const creates: NewCollection[] = [];
  server.use(
    http.get("http://localhost/api/v1/collections", () => HttpResponse.json([])),
    http.get("http://localhost/api/v1/sources", () => HttpResponse.json([])),
    http.post("http://localhost/api/v1/collections", async ({ request }) => {
      creates.push((await request.json()) as NewCollection);
      return HttpResponse.json({ id: "smart-new" });
    }),
  );

  let query: QueryRequest = { filters: [] };
  const filters = renderApp(<FilterHarness onQuery={(next) => (query = next)} />);
  await user.click(screen.getByRole("button", { name: "Advanced filters" }));
  await user.click(screen.getByRole("button", { name: /Safe to ship/ }));

  // The literal predicate, now that the store honours the op: commercial granted (`eq` → `= 1`) and
  // attribution *known* not to be required (`ne` → `= 0`). No `license = permissive` proxy — that
  // was strictly stronger, and excluded assets that genuinely satisfy the criterion.
  expect(query.filters).toContainEqual({
    field: "usage_right",
    op: "eq",
    value: { str: "commercial" },
  });
  expect(query.filters).toContainEqual({
    field: "usage_right",
    op: "ne",
    value: { str: "attribution" },
  });
  expect(query.filters?.some((filter) => filter.field === "license")).toBe(false);

  // Both directions are visible and distinct in the chip row — a safety field whose two opposite
  // claims rendered identically would be worse than no chip at all.
  expect(screen.getByText("Allows commercial use")).toBeInTheDocument();
  expect(screen.getByText("No attribution required")).toBeInTheDocument();
  filters.unmount();

  const saved = savedQuery(query);
  renderApp(<SmartFolderDialog query={saved} onClose={() => {}} />);
  expect(await screen.findByText(/Commercial use is allowed/)).toBeInTheDocument();
  expect(screen.getByText(/No attribution required/)).toBeInTheDocument();
  await user.type(screen.getByLabelText("Name"), "Safe to ship");
  await user.click(screen.getByRole("button", { name: "Create smart folder" }));
  await waitFor(() => expect(creates).toHaveLength(1));
  expect(creates[0].query).toEqual(saved);
});

test("a usage-right control settles a right in either direction, or not at all", async () => {
  const user = userEvent.setup();
  let query: QueryRequest = { filters: [] };
  renderApp(<FilterHarness onQuery={(next) => (query = next)} />);
  await user.click(screen.getByRole("button", { name: "Advanced filters" }));

  const control = screen.getByLabelText("Commercial use");
  // Three states, because the column has three. A checkbox could only say "granted", conflating
  // "known to be denied" with "nobody has established it" — the store matches neither for unknown.
  await user.selectOptions(control, "ne");
  expect(query.filters).toContainEqual({
    field: "usage_right",
    op: "ne",
    value: { str: "commercial" },
  });
  expect(screen.getByText("Commercial use denied")).toBeInTheDocument();

  await user.selectOptions(screen.getByLabelText("Commercial use"), "eq");
  expect(query.filters).toContainEqual({
    field: "usage_right",
    op: "eq",
    value: { str: "commercial" },
  });
  expect(screen.getByText("Allows commercial use")).toBeInTheDocument();

  await user.selectOptions(screen.getByLabelText("Commercial use"), "");
  expect(query.filters?.some((filter) => filter.field === "usage_right")).toBe(false);
});
