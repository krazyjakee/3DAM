// A per-asset "regeneration epoch" for derived thumbnails. When a user forces a thumbnail rebuild
// (Inspector / context-menu "Regenerate thumbnail"), the server drops the cached PNG but its cache
// key — the asset's content hash — is unchanged, so the browser and the `Cache-Control: max-age`
// response would otherwise keep serving the stale image. Bumping this epoch and threading it into the
// thumbnail URL as `&v=<epoch>` busts both caches so the fresh render loads immediately.
//
// Module-level (a single shared store) and exposed through a `useSyncExternalStore` hook, so every
// grid cell and the Inspector preview react to a bump without prop-drilling.

import { useSyncExternalStore } from "react";

const versions = new Map<string, number>();
const listeners = new Set<() => void>();

/** Bump the regeneration epoch for each asset, forcing its `<img>` thumbnails to re-fetch. */
export function bustThumbnails(ids: string[]): void {
  for (const id of ids) versions.set(id, (versions.get(id) ?? 0) + 1);
  for (const notify of listeners) notify();
}

function subscribe(notify: () => void): () => void {
  listeners.add(notify);
  return () => {
    listeners.delete(notify);
  };
}

/** The current regeneration epoch for an asset (0 until first busted). Re-renders the caller on bump. */
export function useThumbnailVersion(id: string): number {
  return useSyncExternalStore(
    subscribe,
    () => versions.get(id) ?? 0,
    () => 0,
  );
}
