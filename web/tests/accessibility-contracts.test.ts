import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const source = (name: string) =>
  readFile(new URL(`../src/components/${name}`, import.meta.url), "utf8");

test("source and collection actions reveal while their row owns keyboard focus", async () => {
  // One assertion per row module: the two hover-reveal action groups moved out of Navigation.tsx
  // when the sidebar rows became their own files, and a count over the parent would now pass on
  // zero of them.
  for (const row of ["navigation/SourceRow.tsx", "navigation/Collections.tsx"]) {
    assert.match(
      await source(row),
      /group-focus-within:flex/,
      `${row}'s action group must reveal on focus-within`,
    );
  }
});

test("asset menus retain root and submenu WAI-ARIA contracts", async () => {
  const menu = await source("ContextMenu.tsx");
  assert.match(menu, /role="menu"/);
  assert.match(menu, /role="menuitem"/);
  assert.match(menu, /aria-haspopup=\{chevron \? "menu"/);
  assert.match(menu, /aria-expanded=\{chevron \? expanded/);
  assert.match(menu, /tabIndex=\{-1\}/);
});

test("duplicate review exposes direct keyboard actions without a context-menu dependency", async () => {
  const duplicates = await source("Duplicates.tsx");
  for (const shortcut of ["Enter", "D", "K", "R", "B"]) {
    assert.match(duplicates, new RegExp(`aria-keyshortcuts="${shortcut}"`));
  }
  assert.doesNotMatch(duplicates, /<ContextMenu/);
});

test("workspace splitters are focusable value-bearing separators", async () => {
  const workspace = await source("Workspace.tsx");
  assert.match(workspace, /role="separator"/);
  assert.match(workspace, /tabIndex=\{0\}/);
  assert.match(workspace, /aria-valuemin=\{resizable\.min\}/);
  assert.match(workspace, /aria-valuemax=\{resizable\.max\}/);
  assert.match(workspace, /aria-valuenow=\{resizable\.width\}/);
  assert.match(workspace, /event\.key === "Enter"/);
});

test("Advanced Search is a labelled focus-managed dialog without an obscuring backdrop", async () => {
  const advanced = await source("AdvancedSearch.tsx");
  assert.match(advanced, /role="dialog"/);
  assert.match(advanced, /aria-labelledby="advanced-search-title"/);
  assert.match(advanced, /panelRef\.current\?\.focus\(\)/);
  assert.match(advanced, /triggerRef\.current\?\.focus\(\)/);
  assert.doesNotMatch(advanced, /Click-away backdrop/);
});
