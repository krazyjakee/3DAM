import { useEffect } from "react";

/** Refill either edge of the retained page LRU as its loading runway enters the viewport. Both
 *  renderers call this with their own units (the grid converts virtual *rows* back to item slots
 *  first), so the window arithmetic lives here once. */
export function useBrowseWindowLoading(
  firstVisible: number,
  lastVisible: number,
  windowStart: number,
  windowEnd: number,
  hasPrevious: boolean,
  loadingPrevious: boolean,
  loadPrevious: () => void,
  hasMore: boolean,
  loading: boolean,
  loadMore: () => void,
) {
  useEffect(() => {
    if (hasPrevious && !loadingPrevious && firstVisible <= windowStart + 8) loadPrevious();
  }, [firstVisible, windowStart, hasPrevious, loadingPrevious, loadPrevious]);
  useEffect(() => {
    if (hasMore && !loading && lastVisible >= windowEnd - 8) loadMore();
  }, [lastVisible, windowEnd, hasMore, loading, loadMore]);
}
