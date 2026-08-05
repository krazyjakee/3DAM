// The Inspector's exact-duplicate panel (issue #166, parent #96) — extracted from `Inspector.tsx`.
// One query (`useDuplicateGroup`) and one `asset` prop; renders nothing when the asset has no twin.

import { useDuplicateGroup } from "@/api/queries";
import type { Asset, AssetSummary } from "@/api/types";
import { bytes } from "@/lib/format";
import { isLocal } from "@/lib/origin";
import { useViewState } from "@/lib/view-state";
import { Thumbnail } from "../Thumbnail";
import { Group } from "./primitives";

/** The byte-identical copies of this asset. In the grid/table those copies collapse into one badged
 *  card; this is where the full set is enumerated (the request in the golden rules: "duplicates listed
 *  in the inspector"). Exact only — perceptual near-matches are the separate "Similar" surface. Absent
 *  entirely when the asset has no identical twin, so the panel stays lean for the common case. */
export function DuplicatesSection({ asset }: { asset: Asset }) {
  const { patch } = useViewState();
  const id = asset.summary.id;
  // A peer-local UUID may collide with a local UUID. Never resolve it against this server's local
  // duplicate index; peer duplicate review belongs to the owning peer.
  const dups = useDuplicateGroup(isLocal(asset.summary.origin) ? id : null);
  const group = dups.data;
  if (!group) return null;

  const others = group.total_members - 1;
  return (
    <Group title={`Duplicates (${others})`}>
      <p className="mb-2 text-[11px] text-fg-dim">
        {others} byte-identical {others === 1 ? "copy" : "copies"} (same content hash). 3DAM only
        groups — dispose of a copy from its context menu.
      </p>
      <div className="grid grid-cols-3 gap-1.5">
        {group.members.map((m) => (
          <DuplicateTile
            key={m.asset.id}
            member={m.asset}
            keep={(group.chosen_keep ?? group.suggested_keep) === m.asset.id}
            current={m.asset.id === id}
            onOpen={() => patch({
              selected: m.asset.id,
              owner: typeof m.asset.origin === "object" ? m.asset.source_id : null,
            })}
          />
        ))}
      </div>
      {group.members.length < group.total_members && (
        <p className="mt-2 text-[11px] text-fg-dim">
          Showing {group.members.length} of {group.total_members} copies. Open Duplicate review to
          page through the set.
        </p>
      )}
    </Group>
  );
}

function DuplicateTile({
  member,
  keep,
  current,
  onOpen,
}: {
  member: AssetSummary;
  keep: boolean;
  current: boolean;
  onOpen: () => void;
}) {
  return (
    <button
      className="flex flex-col overflow-hidden rounded border text-left transition-colors hover:border-border-strong coarse:min-h-11"
      style={{ borderColor: current ? "var(--color-accent)" : "var(--color-border)" }}
      title={`${member.name} · ${bytes(member.size)}${keep ? " · suggested keep" : ""}${current ? " · this asset" : ""}`}
      onClick={onOpen}
    >
      <span className="relative aspect-square w-full">
        <Thumbnail asset={member} size={32} />
        {keep && (
          <span className="absolute top-1 left-1 rounded bg-accent px-1 py-0.5 text-[9px] font-semibold text-accent-fg">
            Keep
          </span>
        )}
      </span>
      <span className="flex items-center justify-between gap-1 px-1 py-0.5">
        <span className="min-w-0 truncate text-[10px] text-fg-muted">{member.name}</span>
        <span className="shrink-0 text-[10px] text-fg-dim tabular-nums">{bytes(member.size)}</span>
      </span>
    </button>
  );
}
