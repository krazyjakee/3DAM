/**
 * Deterministic long-scroll profile for issue #147.
 *
 * Run directly (outside the normal unit suite so CI can choose a representative Node host):
 *   node --expose-gc --experimental-strip-types scripts/profile-browse-window.mts 1000000
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

const fixtureSize = Number.parseInt(process.argv[2] ?? "1000000", 10);
assert.ok(fixtureSize === 100_000 || fixtureSize === 1_000_000, "fixture must be 100000 or 1000000");
const collectGarbage = globalThis.gc;
assert.ok(collectGarbage, "run with --expose-gc so heap plateau measurements are comparable");

const pageCount = Math.ceil(fixtureSize / BROWSE_PAGE_SIZE);
const directory = new BrowseCursorDirectory();
const retained: Page<FixtureAsset>[] = [];
const retainedParams: BrowsePageParam[] = [];
const frameTimes: number[] = [];
const selectedIds = new Set<string>();
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
        // Models decoded thumbnail retention without allocating the whole fixture up front.
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
  // Keep one selected row across eviction, mirroring Browser's selection-owned summary cache.
  if (pageIndex === 2) {
    const selected = page.items[5];
    selectedIds.add(selected.id);
    selectedSummaries.set(selected.id, selected);
  }
  for (const id of selectedIds) lookup.get(id) ?? selectedSummaries.get(id);

  assert.ok(retained.length <= BROWSE_MAX_PAGES);
  assert.ok(flattened.length <= BROWSE_MAX_PAGES * BROWSE_PAGE_SIZE);
  frameTimes.push(performance.now() - started);

  if (pageIndex === Math.floor(pageCount / 2)) {
    collectGarbage();
    middleHeap = process.memoryUsage().heapUsed;
  }
}

// Exercise backwards recreation after the first page payload has long since been evicted.
assert.deepEqual(directory.previous(retainedParams[0]), {
  index: retainedParams[0].index - 1,
  after:
    retainedParams[0].index - 1 === 0
      ? null
      : `cursor-${retainedParams[0].index - 1}`,
});
assert.equal(selectedSummaries.size, 1);
assert.ok(directory.size <= pageCount + 1, "cursor directory retained more than one entry per page");

collectGarbage();
const finalHeap = process.memoryUsage().heapUsed;
const heapGrowth = finalHeap - middleHeap;
const sortedFrames = frameTimes.slice(Math.floor(frameTimes.length * 0.1)).sort((a, b) => a - b);
const p99 = sortedFrames[Math.floor(sortedFrames.length * 0.99)] ?? 0;

// Cursor metadata grows sparsely; decoded thumbnails, pages, arrays, and maps must plateau.
assert.ok(heapGrowth < 24 * 1024 * 1024, `heap did not plateau: +${heapGrowth} bytes after midpoint`);
assert.ok(p99 < 16, `p99 synthetic browse frame ${p99.toFixed(2)}ms exceeds 16ms`);

console.log(JSON.stringify({ fixtureSize, retainedPages: retained.length, cursorEntries: directory.size, heapGrowth, p99 }, null, 2));
