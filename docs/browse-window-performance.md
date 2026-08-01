# Browse-window memory profile

The asset browser retains at most nine 60-row query pages. Evicted rows keep their logical height,
so the scroll position does not jump, while a sparse opaque-cursor directory lets the leading edge
be fetched again when the user scrolls backwards. Selection IDs and the URL-owned inspected asset
are independent of the page payload; the few summaries needed for an explicit multi-selection are
kept only as long as that selection.

Derived arrays, duplicate-collapse maps, row lookups, and thumbnail-prefetch keys are built from the
retained window, never the complete scroll history. Thumbnail blob URLs are owned by virtualized
cells and revoked when those cells unmount, so decoded media retention follows the DOM overscan and
page eviction rather than catalog size.

## Long-scroll profile

Run the deterministic profile from `web/` on both supported fixture sizes:

```sh
node --expose-gc --experimental-strip-types scripts/profile-browse-window.mts 100000
node --expose-gc --experimental-strip-types scripts/profile-browse-window.mts 1000000
```

The harness generates rows a page at a time, traverses the whole fixture, rebuilds the same bounded
flattened array and lookup used by the UI, retains simulated decoded thumbnail bytes, preserves an
off-screen selection, and recreates a backwards cursor after eviction. It fails unless:

- page and derived-item retention remain at or below 9 pages / 540 rows;
- post-warmup heap growth stays below 24 MiB (allowing for the sparse cursor directory and runtime
  noise); and
- synthetic per-page processing p99 remains below one 16 ms frame.

Record Node version, operating system, CPU, fixture size, `heapGrowth`, and `p99` from the JSON output
when changing page size, overscan, duplicate collapsing, or thumbnail loading.
