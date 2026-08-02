import { HttpResponse, http } from "msw";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import { expect, test } from "vitest";
import { Browser } from "../../src/components/Browser";
import { SelectionProvider } from "../../src/lib/selection";
import { asset, assetPage, assetSummary } from "./fixtures";
import { renderApp } from "./render";
import { server } from "./server";

test("the browser collapses exact copies and keeps selection behavior on visible assets", async () => {
  const first = assetSummary({ id: "asset-a", name: "Kick one.wav", media: "audio", format: "wav" });
  const copy = assetSummary({ id: "asset-b", name: "Kick copy.wav", media: "audio", format: "wav" });
  const other = assetSummary({ id: "asset-c", name: "Snare.wav", media: "audio", format: "wav" });

  server.use(
    http.post("http://localhost/api/v1/query", () =>
      HttpResponse.json({ ...assetPage([first, copy, other]), total: 100 }),
    ),
    http.post("http://localhost/api/v1/duplicates", () =>
      HttpResponse.json([
        {
          kind: "exact",
          media: "audio",
          members: [first, copy],
          signal: "same content hash",
          suggested_keep: first.id,
        },
      ]),
    ),
    http.post("http://localhost/api/v1/prefetch", () => new HttpResponse(null, { status: 204 })),
    http.get("http://localhost/api/v1/whoami", () =>
      HttpResponse.json({
        identity: "test",
        scopes: ["read", "write", "admin", "mcp_use", "federate"],
        anonymous: false,
      }),
    ),
    http.get("http://localhost/api/v1/sources", () => HttpResponse.json([])),
    http.get("http://localhost/api/v1/collections", () => HttpResponse.json([])),
    http.get("http://localhost/api/v1/assets/:id", ({ params }) => {
      const summary = [first, copy, other].find((item) => item.id === params.id) ?? first;
      return HttpResponse.json(asset({ summary }));
    }),
  );

  renderApp(
    <SelectionProvider>
      <Browser />
    </SelectionProvider>,
    { route: "/?view=table" },
  );

  const representative = await screen.findByRole("button", { name: /Kick one\.wav.*1 duplicate/i });
  const secondVisible = screen.getByRole("button", { name: /Snare\.wav/i });
  expect(screen.queryByRole("button", { name: /Kick copy\.wav/i })).not.toBeInTheDocument();

  fireEvent.click(representative);
  fireEvent.click(secondVisible, { ctrlKey: true });

  expect(await screen.findByText("2 selected")).toBeInTheDocument();
  await waitFor(() => {
    expect(representative).toHaveAttribute("aria-pressed", "true");
    expect(secondVisible).toHaveAttribute("aria-pressed", "true");
  });

  expect(screen.getByRole("button", { name: "Select visible" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Select loaded (2)" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Select all 100 query results" }));
  expect(await screen.findByText("100 query results selected")).toBeInTheDocument();

  fireEvent.click(screen.getByRole("button", { name: "Grid view" }));
  expect(await screen.findByText("100 query results selected")).toBeInTheDocument();
  expect(await screen.findByRole("button", { name: /Kick one\.wav.*1 duplicate/i })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
});
