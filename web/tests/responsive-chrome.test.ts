import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const component = (name: string) =>
  readFile(new URL(`../src/components/${name}`, import.meta.url), "utf8");

test("Browser chrome responds to centre-pane width instead of viewport width", async () => {
  // The toolbar and selection bar were extracted into `components/browser/` (issue #164); the
  // container-query contract they share with the shell is unchanged.
  const [browser, toolbar, selectionBar, css] = await Promise.all([
    component("Browser.tsx"),
    component("browser/Toolbar.tsx"),
    component("browser/SelectionBar.tsx"),
    readFile(new URL("../src/index.css", import.meta.url), "utf8"),
  ]);
  assert.match(browser, /className="browser-shell /);
  assert.match(toolbar, /browser-toolbar-primary/);
  assert.match(toolbar, /browser-toolbar-secondary/);
  assert.match(selectionBar, /browser-selection-actions/);
  assert.match(css, /container-name: browser/);
  assert.match(css, /@container browser \(max-width: 48rem\)/);
  assert.match(css, /@container browser \(max-width: 40rem\)/);
});

test("table columns intentionally reduce at narrow pane widths", async () => {
  const [browser, css] = await Promise.all([
    component("Browser.tsx"),
    readFile(new URL("../src/index.css", import.meta.url), "utf8"),
  ]);
  assert.ok(
    browser.match(/asset-table-columns/g)?.length === 4,
    "skeleton, header, and virtual rows must share one responsive column contract",
  );
  assert.match(css, /@container browser \(max-width: 42rem\)/);
  assert.match(css, /@container browser \(max-width: 28rem\)/);
  assert.match(css, /\.asset-table-detail\s*\{\s*display: none/);
  assert.match(css, /\.asset-table-license\s*\{\s*display: none/);
  assert.doesNotMatch(browser, /grid-cols-\[1fr_64px_104px_112px_84px\]/);
});

test("phone status chrome keeps jobs, cancellation, connection, and sign-in reachable", async () => {
  const status = await component("StatusBar.tsx");
  assert.match(status, /basis-full items-center lg:basis-auto/);
  assert.match(status, /aria-label="Open background job history"/);
  assert.match(status, /`Cancel \$\{label\}`/);
  assert.match(status, /`Cancel \$\{jobLabel\}`/);
  assert.match(status, /role="status"/);
  assert.match(status, />Sign in\s*<\/span>/);
  assert.doesNotMatch(status, /<footer className="[^"]*(?:^|\s)h-7(?:\s|")/);
});

test("responsive popovers cannot exceed the phone viewport", async () => {
  const [advanced, status] = await Promise.all([
    component("AdvancedSearch.tsx"),
    component("StatusBar.tsx"),
  ]);
  assert.match(advanced, /max-w-\[calc\(100vw-1rem\)\]/);
  assert.match(status, /w-\[calc\(100vw-1rem\)\] max-w-\[380px\]/);
});
