// A tiny cross-region signal for "double-click an audio asset to play it" (issue #52). The Browser
// and the Inspector are siblings (no shared props), so a double-click in the Browser records a
// one-shot autoplay request here; the Inspector's AudioPlayer for that asset observes it, starts
// playback, and clears it so it never re-fires on a later (single-click) mount of the same asset.

import { useSyncExternalStore } from "react";

let pendingId: string | null = null;
let version = 0;
const listeners = new Set<() => void>();

function emit() {
  listeners.forEach((l) => l());
}

/** Ask the Inspector's player for `id` to auto-start. Bumps a version so a repeat request on an
 *  already-mounted player still fires. */
export function requestAutoplay(id: string) {
  pendingId = id;
  version += 1;
  emit();
}

/** Clear the pending request once a player has acted on it, so it doesn't re-fire on later mounts. */
export function clearAutoplay() {
  if (pendingId === null) return;
  pendingId = null;
  version += 1;
  emit();
}

/** A signal that is `> 0` while an autoplay is pending for `id` (and ticks per request), else `0`. */
export function useAutoplaySignal(id: string | undefined): number {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => (id && pendingId === id ? version : 0),
    () => 0,
  );
}
