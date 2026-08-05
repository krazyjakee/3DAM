// The grid and table renderers were extracted out of Browser.tsx behind the `ListProps` contract
// (issue #165). Both render here from a plain fixture — no Browser, no query page, no selection
// provider — which is the point of the seam: everything they need arrives as props.

import { HttpResponse, http } from "msw";
import { fireEvent, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { AssetSummary } from "../../src/api/types";
import { Grid, GridSkeleton } from "../../src/components/browser/GridList";
import { Table, TableSkeleton } from "../../src/components/browser/TableList";
import type { ListProps } from "../../src/components/browser/types";
import { assetSelectionKey } from "../../src/lib/selection";
import { assetSummary } from "./fixtures";
import { renderApp } from "./render";
import { server } from "./server";

// The shared setup runs rAF callbacks synchronously, which the virtualiser's scroll reconcile turns
// into unbounded recursion the moment an arrow key scrolls a roving target into view. Drop the
// callbacks here instead: roving focus already focuses synchronously and only *repeats* on the next
// frame, in case a large jump rendered late.
beforeEach(() => vi.stubGlobal("requestAnimationFrame", () => 0));
afterEach(() => vi.unstubAllGlobals());

function identity() {
  return http.get("http://localhost/api/v1/whoami", () =>
    HttpResponse.json({
      identity: "test",
      scopes: ["read", "write", "admin", "mcp_use", "federate"],
      anonymous: false,
    }),
  );
}

// Audio keeps the honest typed tile, so a cell fetches no thumbnail — the fixture stays offline.
const items: AssetSummary[] = [
  assetSummary({ id: "asset-a", name: "Kick.wav", media: "audio", format: "wav", size: 2048, key_attrs: { duration: "0:01", type: "one-shot" } }),
  assetSummary({ id: "asset-b", name: "Snare.wav", media: "audio", format: "wav", size: 4096, key_attrs: {} }),
  assetSummary({ id: "asset-c", name: "Hat.wav", media: "audio", format: "wav", size: 8192, key_attrs: {} }),
];

function listProps(overrides: Partial<ListProps> = {}): ListProps {
  return {
    items,
    isSelected: (asset) => asset.id === "asset-b",
    // Keyed the way both renderers look them up: by selection key, so a peer's colliding id can't
    // borrow a local asset's badge.
    dupCounts: new Map([[assetSelectionKey(items[0]), 2]]),
    onItemClick: () => {},
    onItemActivate: () => {},
    onContext: () => {},
    hasMore: false,
    loadMore: () => {},
    loading: false,
    hasPrevious: false,
    loadPrevious: () => {},
    loadingPrevious: false,
    windowStart: 0,
    virtualCount: items.length,
    ...overrides,
  };
}

test("the grid renders one cell per visible asset and reports clicks with their modifiers", async () => {
  server.use(identity());
  const onItemClick = vi.fn();
  const onItemActivate = vi.fn();

  renderApp(<Grid {...listProps({ onItemClick, onItemActivate })} />);

  const cell = await screen.findByRole("button", { name: "Kick.wav, audio, 2 duplicates" });
  // Selection is announced, not merely coloured (issue #27).
  expect(cell).toHaveAttribute("aria-pressed", "false");
  expect(screen.getByRole("button", { name: "Snare.wav, audio" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  // The collapsed-duplicate badge rides along on the representative cell only.
  expect(within(cell).getByLabelText("2 duplicates")).toBeInTheDocument();

  fireEvent.click(cell, { ctrlKey: true, shiftKey: false });
  expect(onItemClick).toHaveBeenCalledWith(items[0], { meta: true, shift: false });
  fireEvent.doubleClick(cell);
  expect(onItemActivate).toHaveBeenCalledWith(items[0]);
});

test("the grid roves a single tab stop with the arrow keys", async () => {
  server.use(identity());
  renderApp(<Grid {...listProps()} />);

  const first = await screen.findByRole("button", { name: /Kick\.wav/ });
  const second = screen.getByRole("button", { name: /Snare\.wav/ });
  // Exactly one cell is tabbable; the rest are reachable only by arrow keys (issue #27).
  expect(first).toHaveAttribute("tabindex", "0");
  expect(second).toHaveAttribute("tabindex", "-1");

  fireEvent.keyDown(screen.getByRole("group", { name: "Assets" }), { key: "ArrowRight" });
  expect(second).toHaveFocus();
  expect(second).toHaveAttribute("tabindex", "0");
  expect(first).toHaveAttribute("tabindex", "-1");

  // Home returns to the start; a move past either end is swallowed rather than wrapping.
  fireEvent.keyDown(screen.getByRole("group", { name: "Assets" }), { key: "Home" });
  expect(first).toHaveFocus();
  fireEvent.keyDown(screen.getByRole("group", { name: "Assets" }), { key: "ArrowUp" });
  expect(first).toHaveFocus();
});

test("the table renders the per-media detail column and ignores horizontal arrows", async () => {
  server.use(identity());
  renderApp(<Table {...listProps()} />);

  const row = await screen.findByRole("button", { name: /Kick\.wav/ });
  expect(within(row).getByText("0:01 · one-shot")).toBeInTheDocument();
  expect(within(row).getByText("wav")).toBeInTheDocument();
  expect(within(row).getByText("2.0 KB")).toBeInTheDocument();
  // Assets with no detail attribute keep the column aligned with an em dash.
  expect(within(screen.getByRole("button", { name: /Snare\.wav/ })).getByText("—")).toBeInTheDocument();

  const list = screen.getByRole("group", { name: "Assets" });
  fireEvent.keyDown(list, { key: "ArrowRight" });
  expect(row).not.toHaveFocus();
  fireEvent.keyDown(list, { key: "ArrowDown" });
  expect(screen.getByRole("button", { name: /Snare\.wav/ })).toHaveFocus();
  fireEvent.keyDown(list, { key: "End" });
  expect(screen.getByRole("button", { name: /Hat\.wav/ })).toHaveFocus();
});

test("both renderers refill the retained window when its runway is on screen", async () => {
  server.use(identity());
  const loadMore = vi.fn();
  const loadPrevious = vi.fn();

  const { unmount } = renderApp(
    <Grid {...listProps({ hasMore: true, loadMore, hasPrevious: true, loadPrevious })} />,
  );
  await screen.findByRole("button", { name: /Kick\.wav/ });
  expect(loadMore).toHaveBeenCalled();
  expect(loadPrevious).toHaveBeenCalled();
  unmount();

  loadMore.mockClear();
  loadPrevious.mockClear();
  renderApp(
    <Table
      {...listProps({ hasMore: true, loadMore, hasPrevious: true, loadPrevious, loading: true, loadingPrevious: true })}
    />,
  );
  await screen.findByRole("button", { name: /Kick\.wav/ });
  // An in-flight page must not be requested a second time.
  expect(loadMore).not.toHaveBeenCalled();
  expect(loadPrevious).not.toHaveBeenCalled();
  expect(screen.getByText("Loading more…")).toBeInTheDocument();
  expect(screen.getByText("Loading earlier…")).toBeInTheDocument();
});

test("the skeletons stand in for both layouts while the first page loads", () => {
  const { unmount } = renderApp(<GridSkeleton />);
  unmount();
  renderApp(<TableSkeleton />);
  expect(screen.getByText("Detail")).toBeInTheDocument();
});
