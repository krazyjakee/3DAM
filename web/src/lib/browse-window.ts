import type { Page } from "@/api/types";

/** One server page is deliberately small enough that nine retained pages cover several screens. */
export const BROWSE_PAGE_SIZE = 60;
export const BROWSE_MAX_PAGES = 9;

/**
 * TanStack may evict either end of the page array. The logical index keeps the remaining pages at
 * their original scroll positions; `after` is the opaque server cursor that can recreate a page.
 */
export interface BrowsePageParam {
  after: string | null;
  index: number;
}

/**
 * Pages are the expensive part of browse history. We retain only a sparse cursor directory for
 * evicted pages so backwards scrolling can refetch them. At one million assets this is about
 * 16,667 short entries rather than one million asset summaries and decoded thumbnails.
 */
export class BrowseCursorDirectory {
  private readonly cursors = new Map<number, string | null>([[0, null]]);

  remember(param: BrowsePageParam, next: string | null): void {
    this.cursors.set(param.index, param.after);
    if (next !== null) this.cursors.set(param.index + 1, next);
    else this.cursors.delete(param.index + 1);
  }

  previous(param: BrowsePageParam): BrowsePageParam | undefined {
    const index = param.index - 1;
    if (index < 0 || !this.cursors.has(index)) return undefined;
    return { index, after: this.cursors.get(index) ?? null };
  }

  get size(): number {
    return this.cursors.size;
  }
}

// A route change can unmount Browser while TanStack retains its bounded page window. Keep the
// corresponding sparse cursor chain available across that remount, but cap inactive query shapes
// so experimenting with many searches cannot create a second unbounded cache.
const CURSOR_QUERY_LRU_SIZE = 8;
const cursorDirectories = new Map<string, BrowseCursorDirectory>();

export function browseCursorDirectory(queryKey: string): BrowseCursorDirectory {
  const existing = cursorDirectories.get(queryKey);
  if (existing) {
    cursorDirectories.delete(queryKey);
    cursorDirectories.set(queryKey, existing);
    return existing;
  }
  const directory = new BrowseCursorDirectory();
  cursorDirectories.set(queryKey, directory);
  if (cursorDirectories.size > CURSOR_QUERY_LRU_SIZE) {
    const oldest = cursorDirectories.keys().next().value as string | undefined;
    if (oldest !== undefined) cursorDirectories.delete(oldest);
  }
  return directory;
}

export interface BrowseWindowMetrics {
  /** Logical slot occupied by the first retained item. */
  start: number;
  /** Logical slot immediately following the retained window. */
  end: number;
  /** Virtual slots including a one-page forward loading runway. */
  virtualCount: number;
}

/**
 * Preserve the leading height of evicted pages while bounding the retained rows. The extra page at
 * the tail lets the virtualizer request the next cursor without pretending the whole catalog is in
 * memory (or offering a scrollbar jump to a page whose cursor has not been discovered yet).
 */
export function browseWindowMetrics(
  firstParam: BrowsePageParam | undefined,
  retainedItems: number,
  hasNextPage: boolean,
): BrowseWindowMetrics {
  const start = Math.max(0, (firstParam?.index ?? 0) * BROWSE_PAGE_SIZE);
  const end = start + retainedItems;
  return {
    start,
    end,
    virtualCount: end + (hasNextPage ? BROWSE_PAGE_SIZE : 0),
  };
}

/** Flatten only the bounded query window. Kept here so the profiling harness exercises the same work. */
export function flattenBrowsePages<T>(pages: readonly Page<T>[] | undefined): T[] {
  return pages?.flatMap((page) => page.items) ?? [];
}
