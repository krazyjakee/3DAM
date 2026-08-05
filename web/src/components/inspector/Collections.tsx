// The Inspector's collection-membership panel (issue #166, parent #96) — extracted from
// `Inspector.tsx`. One query pair (`useCollections` + `useCollectionMembers`) and one `asset` prop.

import { X } from "lucide-react";
import { useCan, useCollectionMembers, useCollections } from "@/api/queries";
import type { Asset, CollectionId } from "@/api/types";
import { AUTH_COPY } from "@/lib/auth";
import { peerReadOnlyTitle } from "@/lib/origin";
import { Group } from "./primitives";

/** This asset's collection memberships (issue #3). Manual memberships are editable per-asset here —
 *  remove with the chip's ×, add via the picker — which needs no multi-select (batch add/remove to a
 *  selection lands with the multi-select enabler). Smart folders are query-driven, so they show as a
 *  read-only chip with no remove control. */
export function CollectionsGroup({ asset }: { asset: Asset }) {
  const collections = useCollections();
  const members = useCollectionMembers();
  const canWrite = useCan("write");
  // Membership rows live in this instance's catalog — a peer-owned reference can't join them.
  const peerTitle = peerReadOnlyTitle(asset.summary.origin);
  const all = collections.data ?? [];
  const inIds = new Set(asset.collections);
  const inCollections = all.filter((c) => inIds.has(c.id));
  const addable = all.filter((c) => c.kind === "manual" && !inIds.has(c.id));

  const edit = (id: CollectionId, op: "add" | "remove") =>
    members.mutate({ id, members: { [op]: [asset.summary.id] } });

  return (
    <Group title="Collections">
      {inCollections.length === 0 ? (
        <p className="text-[11px] text-fg-dim italic">Not in any collection.</p>
      ) : (
        <div className="flex flex-wrap gap-1">
          {inCollections.map((c) => (
            <span
              key={c.id}
              className="inline-flex items-center gap-1 rounded bg-surface-2 px-1.5 py-0.5 text-[10px] text-fg-muted"
              title={c.kind === "smart" ? "Smart folder (query-driven membership)" : c.name}
            >
              {c.name}
              {c.kind === "manual" && (
                <button
                  className="flex items-center justify-center hover:text-danger disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
                  title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : `Remove from ${c.name}`)}
                  aria-label={`Remove from ${c.name}`}
                  disabled={members.isPending || !canWrite || !!peerTitle}
                  onClick={() => edit(c.id, "remove")}
                >
                  <X size={11} />
                </button>
              )}
            </span>
          ))}
        </div>
      )}
      {addable.length > 0 && (
        <select
          className="field mt-2 disabled:cursor-not-allowed disabled:opacity-40"
          aria-label="Add to collection"
          title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : undefined)}
          value=""
          disabled={members.isPending || !canWrite || !!peerTitle}
          onChange={(e) => {
            if (e.target.value) edit(e.target.value, "add");
          }}
        >
          <option value="" disabled>
            Add to collection…
          </option>
          {addable.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
        </select>
      )}
    </Group>
  );
}
