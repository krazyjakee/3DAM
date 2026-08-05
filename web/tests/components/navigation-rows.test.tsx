// The two units that came out of `Navigation.tsx` (issue #168): the collections section, which owns
// its own CRUD, and one source row, which owns its disclosure. Both are extracted precisely because
// they stand alone — so these tests mount them without the rail, the view state, or a workspace.

import { HttpResponse, http } from "msw";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test, vi } from "vitest";
import type { Collection, SourceInfo } from "../../src/api/types";
import { Collections } from "../../src/components/navigation/Collections";
import { SourceRow } from "../../src/components/navigation/SourceRow";
import { renderApp } from "./render";
import { server } from "./server";

/** A write gate that grants write, so these tests exercise the control rather than the gate. */
const allow = (opts?: { disabled?: boolean; title?: string }) => ({
  disabled: !!opts?.disabled,
  title: opts?.title,
});

function collection(overrides: Partial<Collection> = {}): Collection {
  return {
    id: "c1",
    name: "Kicks",
    kind: "manual",
    count: 3,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  };
}

function source(overrides: Partial<SourceInfo> = {}): SourceInfo {
  return {
    id: "s1",
    kind: "local_fs",
    name: "Project",
    uri: "/srv/project",
    state: "online",
    stats: { asset_count: 42, last_scanned_at: null, last_error: null },
    watch: false,
    ...overrides,
  };
}

function serveCollections(items: Collection[]) {
  server.use(http.get("http://localhost/api/v1/collections", () => HttpResponse.json(items)));
}

test("a manual collection can be created, renamed, and deleted from the section alone", async () => {
  const user = userEvent.setup();
  const created: unknown[] = [];
  const renamed: unknown[] = [];
  const deleted: string[] = [];
  serveCollections([collection()]);
  server.use(
    http.post("http://localhost/api/v1/collections", async ({ request }) => {
      created.push(await request.json());
      return HttpResponse.json({ id: "c2" });
    }),
    http.put("http://localhost/api/v1/collections/c1", async ({ request }) => {
      renamed.push(await request.json());
      return new HttpResponse(null, { status: 204 });
    }),
    // Matched as a pattern, not the literal path, so `params.id` is the id actually deleted —
    // asserting on it is the point of the handler.
    http.delete("http://localhost/api/v1/collections/:id", ({ params }) => {
      deleted.push(String(params.id));
      return new HttpResponse(null, { status: 204 });
    }),
  );

  renderApp(<Collections gate={allow} activeId={null} onSelect={vi.fn()} />);
  expect(await screen.findByText("Kicks")).toBeVisible();

  await user.click(screen.getByRole("button", { name: "New manual collection" }));
  const newDialog = await screen.findByRole("dialog", { name: "New manual collection" });
  await user.type(within(newDialog).getByRole("textbox"), "Snares");
  await user.click(within(newDialog).getByRole("button", { name: "Create" }));
  await waitFor(() => expect(created).toEqual([{ name: "Snares", kind: "manual" }]));

  await user.click(screen.getByRole("button", { name: "Rename collection Kicks" }));
  const renameDialog = await screen.findByRole("dialog", { name: "Rename collection" });
  await user.clear(within(renameDialog).getByRole("textbox"));
  await user.type(within(renameDialog).getByRole("textbox"), "Kick drums");
  await user.click(within(renameDialog).getByRole("button", { name: "Rename" }));
  await waitFor(() => expect(renamed).toEqual([{ name: "Kick drums" }]));

  await user.click(screen.getByRole("button", { name: "Delete collection Kicks" }));
  const confirmDialog = await screen.findByRole("alertdialog", {
    name: "Delete collection “Kicks”?",
  });
  await user.click(within(confirmDialog).getByRole("button", { name: "Delete collection" }));
  await waitFor(() => expect(deleted).toEqual(["c1"]));
});

test("deleting the collection being browsed clears the selection first", async () => {
  const user = userEvent.setup();
  const onSelect = vi.fn();
  serveCollections([collection()]);
  server.use(
    http.delete("http://localhost/api/v1/collections/c1", () => new HttpResponse(null, { status: 204 })),
  );

  renderApp(<Collections gate={allow} activeId="c1" onSelect={onSelect} />);
  await user.click(await screen.findByRole("button", { name: "Delete collection Kicks" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete collection “Kicks”?" });
  await user.click(within(dialog).getByRole("button", { name: "Delete collection" }));

  await waitFor(() => expect(onSelect).toHaveBeenCalledWith(null));
});

test("selecting the active collection toggles it off; sharing is absent without the callback", async () => {
  const user = userEvent.setup();
  const onSelect = vi.fn();
  serveCollections([collection({ kind: "smart", count: null })]);

  renderApp(<Collections gate={allow} activeId="c1" onSelect={onSelect} />);
  await user.click(await screen.findByRole("button", { name: "Kicks" }));
  expect(onSelect).toHaveBeenCalledWith(null);
  expect(screen.queryByRole("button", { name: "Share collection Kicks" })).toBeNull();
  // A smart folder is flagged as query-driven rather than offering a membership affordance.
  expect(screen.getByTitle("Smart folder (saved query)")).toBeVisible();
});

test("an empty, settled collections list offers the create shortcut", async () => {
  serveCollections([]);
  renderApp(<Collections gate={allow} activeId={null} onSelect={vi.fn()} />);
  expect(await screen.findByRole("button", { name: "+ Create a manual collection" })).toBeVisible();
});

test("a local source row discloses its folder tree without scoping the browser to the source", async () => {
  const user = userEvent.setup();
  const onSelect = vi.fn();
  server.use(
    http.post("http://localhost/api/v1/folders", () =>
      HttpResponse.json([{ path: "Drums/", name: "Drums", asset_count: 7, has_children: false }]),
    ),
  );

  renderApp(
    <SourceRow
      source={source()}
      gate={allow}
      active={false}
      removing={false}
      onSelect={onSelect}
      onRescan={vi.fn()}
      onRemove={vi.fn()}
    />,
  );
  expect(screen.getByText("42")).toBeVisible();
  expect(screen.getByRole("img", { name: /Source status/ })).toBeVisible();

  await user.click(screen.getByRole("button", { name: "Expand Project folders" }));
  expect(await screen.findByText("Drums")).toBeVisible();
  expect(onSelect).not.toHaveBeenCalled();
  await user.click(screen.getByRole("button", { name: "Collapse Project folders" }));
  await waitFor(() => expect(screen.queryByText("Drums")).toBeNull());
});

test("a federated peer row offers neither folders nor a rescan", () => {
  renderApp(
    <SourceRow
      source={source({ id: "p1", kind: "federated", name: "Studio peer" })}
      gate={allow}
      active
      removing={false}
      onSelect={vi.fn()}
      onRescan={vi.fn()}
      onRemove={vi.fn()}
      onShare={vi.fn()}
    />,
  );
  expect(screen.getByText("peer")).toBeVisible();
  expect(screen.queryByRole("button", { name: /Studio peer folders/ })).toBeNull();
  expect(screen.queryByRole("button", { name: /Quick rescan this source/ })).toBeNull();
  expect(screen.getByRole("button", { name: "Share source Studio peer" })).toBeVisible();
});

test("rescan and remove fire from the row's own actions", async () => {
  const user = userEvent.setup();
  const onRescan = vi.fn();
  const onRemove = vi.fn();
  renderApp(
    <SourceRow
      source={source({ state: "scanning" })}
      gate={allow}
      active={false}
      removing={false}
      onSelect={vi.fn()}
      onRescan={onRescan}
      onRemove={onRemove}
    />,
  );
  await user.click(screen.getByRole("button", { name: /Quick rescan this source/ }));
  await user.click(screen.getByRole("button", { name: "Remove source" }));
  expect(onRescan).toHaveBeenCalledTimes(1);
  expect(onRemove).toHaveBeenCalledTimes(1);
});
