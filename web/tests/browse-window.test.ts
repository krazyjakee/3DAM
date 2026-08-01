import assert from "node:assert/strict";
import test from "node:test";

import {
  BROWSE_MAX_PAGES,
  BROWSE_PAGE_SIZE,
  BrowseCursorDirectory,
  browseWindowMetrics,
  flattenBrowsePages,
  type BrowsePageParam,
} from "../src/lib/browse-window.ts";

test("cursor directory recreates an evicted page while payload retention stays bounded", () => {
  const cursors = new BrowseCursorDirectory();
  let param: BrowsePageParam = { after: null, index: 0 };
  const retained: { items: number[]; cursor: string | null; total: number; partial: { complete: true } }[] = [];

  for (let index = 0; index < 40; index += 1) {
    const next = index === 39 ? null : `cursor-${index + 1}`;
    cursors.remember(param, next);
    retained.push({
      items: Array.from({ length: BROWSE_PAGE_SIZE }, (_, offset) => index * BROWSE_PAGE_SIZE + offset),
      cursor: next,
      total: 40 * BROWSE_PAGE_SIZE,
      partial: { complete: true },
    });
    if (retained.length > BROWSE_MAX_PAGES) retained.shift();
    if (next !== null) param = { after: next, index: index + 1 };
  }

  assert.equal(retained.length, BROWSE_MAX_PAGES);
  assert.equal(flattenBrowsePages(retained).length, BROWSE_MAX_PAGES * BROWSE_PAGE_SIZE);
  assert.deepEqual(cursors.previous({ after: "cursor-31", index: 31 }), {
    after: "cursor-30",
    index: 30,
  });
});

test("virtual metrics preserve evicted height and expose only one loading runway", () => {
  assert.deepEqual(
    browseWindowMetrics(
      { after: "opaque", index: 100 },
      BROWSE_MAX_PAGES * BROWSE_PAGE_SIZE,
      true,
    ),
    {
      start: 100 * BROWSE_PAGE_SIZE,
      end: (100 + BROWSE_MAX_PAGES) * BROWSE_PAGE_SIZE,
      virtualCount: (101 + BROWSE_MAX_PAGES) * BROWSE_PAGE_SIZE,
    },
  );
});
