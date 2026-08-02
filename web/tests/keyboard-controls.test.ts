import assert from "node:assert/strict";
import test from "node:test";

import { menuItemIndex, menuKeyAction } from "../src/lib/menu-keyboard.ts";
import { splitterWidthForKey } from "../src/lib/splitter-keyboard.ts";

test("menu traversal wraps and Home/End reach the menu boundaries", () => {
  assert.equal(menuItemIndex(2, 3, "next"), 0);
  assert.equal(menuItemIndex(0, 3, "previous"), 2);
  assert.equal(menuItemIndex(1, 3, "first"), 0);
  assert.equal(menuItemIndex(1, 3, "last"), 2);
  assert.equal(menuItemIndex(0, 0, "next"), null);
});

test("menu and submenu keys follow the WAI-ARIA focus pattern", () => {
  assert.equal(menuKeyAction("ArrowDown", { inSubmenu: false, hasSubmenu: false }), "next");
  assert.equal(
    menuKeyAction("ArrowRight", { inSubmenu: false, hasSubmenu: true }),
    "open-submenu",
  );
  assert.equal(
    menuKeyAction("ArrowLeft", { inSubmenu: true, hasSubmenu: false }),
    "close-submenu",
  );
  assert.equal(menuKeyAction("Escape", { inSubmenu: true, hasSubmenu: false }), "close-submenu");
  assert.equal(menuKeyAction("Escape", { inSubmenu: false, hasSubmenu: false }), "close-menu");
  assert.equal(menuKeyAction("Tab", { inSubmenu: false, hasSubmenu: false }), "tab-away");
});

test("left and right rail splitters resize in their physical growth direction", () => {
  assert.equal(
    splitterWidthForKey({ key: "ArrowRight", width: 220, min: 160, max: 480, grow: "right" }),
    230,
  );
  assert.equal(
    splitterWidthForKey({ key: "ArrowLeft", width: 300, min: 220, max: 560, grow: "left" }),
    310,
  );
  assert.equal(
    splitterWidthForKey({ key: "Home", width: 300, min: 220, max: 560, grow: "left" }),
    220,
  );
  assert.equal(
    splitterWidthForKey({ key: "End", width: 300, min: 220, max: 560, grow: "left" }),
    560,
  );
});

test("splitter resizing clamps at announced min and max values", () => {
  assert.equal(
    splitterWidthForKey({ key: "ArrowLeft", width: 160, min: 160, max: 480, grow: "right" }),
    160,
  );
  assert.equal(
    splitterWidthForKey({ key: "ArrowLeft", width: 560, min: 220, max: 560, grow: "left" }),
    560,
  );
  assert.equal(
    splitterWidthForKey({ key: "Enter", width: 220, min: 160, max: 480, grow: "right" }),
    null,
  );
});
