/**
 * Synthetic browser-side scale profile used by `cargo xtask perf`.
 * It allocates only the active browse window while traversing every logical page.
 */
import assert from "node:assert/strict";
import { performance } from "node:perf_hooks";

import {
  BROWSE_MAX_PAGES,
  BROWSE_PAGE_SIZE,
  BrowseCursorDirectory,
  flattenBrowsePages,
  type BrowsePageParam,
} from "../src/lib/browse-window.ts";
import type { Page } from "../src/api/types.ts";

type FixtureAsset = {
  id: string;
  name: string;
  tags: string[];
  thumbnailBytes: Uint8Array;
};

const fixtureSize = Number.parseInt(process.argv[2] ?? "", 10);
assert.ok(Number.isSafeInteger(fixtureSize) && fixtureSize >= 100, "fixture size must be an integer >= 100");
const collectGarbage = globalThis.gc;
assert.ok(collectGarbage, "run with --expose-gc so heap measurements are comparable");

const pageCount = Math.ceil(fixtureSize / BROWSE_PAGE_SIZE);
const directory = new BrowseCursorDirectory();
const retained: Page<FixtureAsset>[] = [];
const retainedParams: BrowsePageParam[] = [];
const frameTimes: number[] = [];
const selectedSummaries = new Map<string, FixtureAsset>();
let middleHeap = 0;

for (let pageIndex = 0; pageIndex < pageCount; pageIndex += 1) {
  const started = performance.now();
  const after = pageIndex === 0 ? null : `cursor-${pageIndex}`;
  const next = pageIndex + 1 < pageCount ? `cursor-${pageIndex + 1}` : null;
  const param = { after, index: pageIndex };
  directory.remember(param, next);
  const count = Math.min(BROWSE_PAGE_SIZE, fixtureSize - pageIndex * BROWSE_PAGE_SIZE);
  const page: Page<FixtureAsset> = {
    items: Array.from({ length: count }, (_, offset) => {
      const id = String(pageIndex * BROWSE_PAGE_SIZE + offset);
      return {
        id,
        name: `asset-${id}`,
        tags: [`page-${pageIndex % 17}`, `slot-${offset}`],
        thumbnailBytes: new Uint8Array(256),
      };
    }),
    cursor: next,
    total: fixtureSize,
    partial: { complete: true },
  };
  retained.push(page);
  retainedParams.push(param);
  if (retained.length > BROWSE_MAX_PAGES) {
    retained.shift();
    retainedParams.shift();
  }
  const flattened = flattenBrowsePages(retained);
  const lookup = new Map(flattened.map((asset) => [asset.id, asset]));
  if (pageIndex === 2) selectedSummaries.set(page.items[5].id, page.items[5]);
  for (const id of selectedSummaries.keys()) lookup.get(id) ?? selectedSummaries.get(id);
  assert.ok(retained.length <= BROWSE_MAX_PAGES);
  assert.ok(flattened.length <= BROWSE_MAX_PAGES * BROWSE_PAGE_SIZE);
  frameTimes.push(performance.now() - started);
  if (pageIndex === Math.floor(pageCount / 2)) {
    collectGarbage();
    middleHeap = process.memoryUsage().heapUsed;
  }
}

assert.equal(selectedSummaries.size, 1);
assert.ok(directory.size <= pageCount + 1);
if (retainedParams[0].index > 0) {
  assert.equal(directory.previous(retainedParams[0])?.index, retainedParams[0].index - 1);
}
collectGarbage();
const finalHeap = process.memoryUsage().heapUsed;
const heapGrowthBytes = Math.max(0, finalHeap - middleHeap);
const sortedFrames = frameTimes.slice(Math.floor(frameTimes.length * 0.1)).sort((a, b) => a - b);
const p99Ms = sortedFrames[Math.floor(sortedFrames.length * 0.99)] ?? 0;

console.log(
  JSON.stringify({
    fixtureSize,
    retainedPages: retained.length,
    cursorEntries: directory.size,
    heapGrowthBytes,
    p99Ms,
  }),
);
