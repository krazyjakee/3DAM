import assert from "node:assert/strict";
import test from "node:test";
import { dispatchShortcut, shortcutForEvent, shortcutLabel } from "../src/lib/shortcuts.ts";

const key = (
  value: string,
  overrides: Partial<Parameters<typeof shortcutForEvent>[0]> = {},
) => ({
  key: value,
  ctrlKey: false,
  metaKey: false,
  altKey: false,
  shiftKey: false,
  target: null,
  ...overrides,
});

test("dispatches platform and plain-key shortcuts", () => {
  assert.equal(shortcutForEvent(key("/")), "focus-search");
  assert.equal(shortcutForEvent(key("\\", { ctrlKey: true })), "toggle-view");
  assert.equal(shortcutForEvent(key("\\", { metaKey: true })), "toggle-view");
  assert.equal(shortcutForEvent(key("F10", { shiftKey: true })), "action-menu");
  assert.equal(shortcutForEvent(key("ContextMenu")), "action-menu");
  assert.equal(shortcutLabel("toggle-view", true), "⌘\\");
  assert.equal(shortcutLabel("toggle-view", false), "Ctrl+\\");
});

test("suppresses shortcuts in editable fields and modified plain-key collisions", () => {
  let calls = 0;
  assert.equal(shortcutForEvent(key("s", { target: { tagName: "INPUT" } as never })), null);
  assert.equal(
    dispatchShortcut(key("f", { target: { tagName: "TEXTAREA" } as never }), {
      "toggle-favourite": () => calls++,
    }),
    false,
  );
  assert.equal(
    shortcutForEvent(
      key("f", {
        target: { tagName: "SPAN", closest: () => ({}) } as never,
      }),
    ),
    null,
  );
  assert.equal(shortcutForEvent(key("s", { ctrlKey: true })), null);
  assert.equal(shortcutForEvent(key("f", { altKey: true })), null);
  assert.equal(
    shortcutForEvent(
      key(" ", { target: { tagName: "BUTTON", closest: () => null } as never }),
    ),
    null,
  );
  assert.equal(
    shortcutForEvent(
      key(" ", {
        target: {
          tagName: "BUTTON",
          closest: (selector: string) => (selector === "[data-asset-id]" ? {} : null),
        } as never,
      }),
    ),
    "play-pause",
  );
  assert.equal(calls, 0);
});

test("supports a browse-to-action keyboard flow", () => {
  const state = { focused: "", selectedId: "", favourites: new Set<string>() };
  const search = { focus: () => (state.focused = "search") };
  const assetCell = {
    tagName: "BUTTON",
    closest: (selector: string) => (selector === "[data-asset-id]" ? assetCell : null),
    focus: () => (state.focused = "asset-1"),
    // Enter activation is native button behaviour in Browser's grid/table cells.
    activate: () => {
      if (state.focused === "asset-1") state.selectedId = "asset-1";
    },
  };
  const handlers = {
    "focus-search": search.focus,
    "toggle-favourite": () => {
      if (state.selectedId) state.favourites.add(state.selectedId);
    },
  };

  assert.equal(dispatchShortcut(key("/"), handlers), true);
  assetCell.focus();
  assetCell.activate();
  assert.equal(
    dispatchShortcut(key("f", { target: assetCell as never }), handlers),
    true,
  );
  assert.equal(state.focused, "asset-1");
  assert.equal(state.selectedId, "asset-1");
  assert.deepEqual([...state.favourites], ["asset-1"]);
});
