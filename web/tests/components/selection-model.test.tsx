import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import { MobileSelectionBar } from "../../src/components/Workspace";
import {
  SelectionProvider,
  assetSelectionKey,
  useSelection,
} from "../../src/lib/selection";
import { assetSummary } from "./fixtures";
import { renderApp } from "./render";

const local = assetSummary({ id: "shared-id", name: "Local.wav", media: "audio" });
const peer = assetSummary({
  id: "shared-id",
  name: "Peer.wav",
  media: "audio",
  origin: { peer: "peer-a" },
  source_id: "peer-source-a",
});

const oldQuery = {
  text: "old",
  filters: [],
  sort: { field: "name" as const, dir: "asc" as const },
};
const newQuery = { ...oldQuery, text: "new" };

function ModelHarness() {
  const selection = useSelection();
  return (
    <>
      <output aria-label="Selection count">{selection.count}</output>
      <output aria-label="Selection kind">{selection.selected.kind}</output>
      <output aria-label="Focused owner">{selection.focused?.owner ?? "local"}</output>
      <output aria-label="Local selected">{String(selection.isSelected(local))}</output>
      <output aria-label="Peer selected">{String(selection.isSelected(peer))}</output>
      <button onClick={() => selection.selectAsset(local, { meta: false, shift: false }, [local, peer])}>
        Select local
      </button>
      <button onClick={() => selection.selectAsset(peer, { meta: true, shift: false }, [local, peer])}>
        Toggle peer
      </button>
      <button onClick={() => selection.selectResults({ kind: "query", query: oldQuery }, 100, [local])}>
        Select old query
      </button>
      <button
        onClick={() =>
          selection.reconcile({
            browseScope: "query-scope",
            loaded: [local],
            visible: [local],
            selector: { kind: "query", query: oldQuery },
            total: 100,
          })
        }
      >
        Reconcile old query
      </button>
      <button
        onClick={() =>
          selection.reconcile({
            browseScope: "query-scope",
            loaded: [local],
            visible: [local],
            selector: { kind: "query", query: newQuery },
            total: 80,
          })
        }
      >
        Reconcile new query
      </button>
      <button onClick={() => selection.selectResults({ kind: "collection", collection: "one" }, 12, [local])}>
        Select first collection
      </button>
      <button
        onClick={() =>
          selection.reconcile({
            browseScope: "collection-one",
            loaded: [local],
            visible: [local],
            selector: { kind: "collection", collection: "one" },
            total: 12,
          })
        }
      >
        Reconcile first collection
      </button>
      <button
        onClick={() =>
          selection.reconcile({
            browseScope: "collection-two",
            loaded: [local],
            visible: [local],
            selector: { kind: "collection", collection: "two" },
            total: 9,
          })
        }
      >
        Reconcile second collection
      </button>
    </>
  );
}

test("mobile and browser surfaces share composite peer-aware selection and one clear action", async () => {
  const user = userEvent.setup();
  renderApp(
    <SelectionProvider>
      <ModelHarness />
      <MobileSelectionBar onInspect={() => {}} />
    </SelectionProvider>,
  );

  expect(assetSelectionKey(local)).not.toBe(assetSelectionKey(peer));
  await user.click(screen.getByRole("button", { name: "Select local" }));
  await user.click(screen.getByRole("button", { name: "Toggle peer" }));

  expect(screen.getByRole("status", { name: "Selection count" })).toHaveTextContent("2");
  expect(screen.getByText("2 assets selected")).toBeInTheDocument();
  expect(screen.getByRole("status", { name: "Local selected" })).toHaveTextContent("true");
  expect(screen.getByRole("status", { name: "Peer selected" })).toHaveTextContent("true");
  expect(screen.getByRole("status", { name: "Focused owner" })).toHaveTextContent("peer-source-a");

  await user.click(screen.getByRole("button", { name: "Clear selection" }));
  expect(screen.getByRole("status", { name: "Selection count" })).toHaveTextContent("0");
  expect(screen.queryByText(/assets selected/)).not.toBeInTheDocument();
});

test("a result-wide selector clears instead of silently retargeting to a changed query", async () => {
  const user = userEvent.setup();
  renderApp(
    <SelectionProvider>
      <ModelHarness />
    </SelectionProvider>,
  );

  await user.click(screen.getByRole("button", { name: "Select old query" }));
  await user.click(screen.getByRole("button", { name: "Reconcile old query" }));
  expect(screen.getByRole("status", { name: "Selection count" })).toHaveTextContent("100");
  expect(screen.getByRole("status", { name: "Selection kind" })).toHaveTextContent("results");

  await user.click(screen.getByRole("button", { name: "Reconcile new query" }));
  expect(screen.getByRole("status", { name: "Selection count" })).toHaveTextContent("0");
  expect(screen.getByRole("status", { name: "Selection kind" })).toHaveTextContent("explicit");
});

test("changing collection scope clears its result-wide selection", async () => {
  const user = userEvent.setup();
  renderApp(
    <SelectionProvider>
      <ModelHarness />
    </SelectionProvider>,
  );

  await user.click(screen.getByRole("button", { name: "Select first collection" }));
  await user.click(screen.getByRole("button", { name: "Reconcile first collection" }));
  expect(screen.getByRole("status", { name: "Selection count" })).toHaveTextContent("12");

  await user.click(screen.getByRole("button", { name: "Reconcile second collection" }));
  expect(screen.getByRole("status", { name: "Selection count" })).toHaveTextContent("0");
});
