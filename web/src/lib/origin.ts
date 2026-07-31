// The origin gate for mutating controls (federation, issue #39). A federated asset is a read-only
// reference owned by its peer — no local write path mutates it (tech-spec 07 §7.4) — so acting on
// one against this server can only end in "not found". Mirrors the write-gate pattern
// (lib/write-gate.ts): controls disable (not hide) and explain why via `title`.

import type { AssetSummary, Origin } from "@/api/types";

/** `true` when this instance's catalog owns the asset — the precondition for every mutation. */
export function isLocal(origin: Origin): boolean {
  return origin === "local";
}

/** The subset of a target set this instance can mutate. */
export function localOnly(assets: AssetSummary[]): AssetSummary[] {
  return assets.filter((a) => isLocal(a.origin));
}

/** The standard read-only tooltip for a peer-owned target; `undefined` when local. */
export function peerReadOnlyTitle(origin: Origin): string | undefined {
  return origin === "local"
    ? undefined
    : `Read-only — this asset lives on federated peer “${origin.peer}”. Manage it there.`;
}

/** The plural variant, for a multi-target set that is entirely peer-owned. */
export const PEER_READONLY_SET =
  "Read-only — these assets live on a federated peer. Manage them there.";
