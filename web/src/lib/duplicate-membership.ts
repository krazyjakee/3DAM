import type { AssetSummary, DupMembership } from "@/api/types";
import { isLocal } from "./origin";
import { assetSelectionKey } from "./selection";

/** Collapse exact duplicate rows using only membership/count metadata for the retained page window. */
export function collapseExactDuplicates(
  items: AssetSummary[],
  memberships: DupMembership[] | undefined,
): { visible: AssetSummary[]; dupCounts: Map<string, number> } {
  const memberToGroup = new Map((memberships ?? []).map((item) => [item.asset, item]));
  if (memberToGroup.size === 0) return { visible: items, dupCounts: new Map() };

  const seen = new Set<string>();
  const visible: AssetSummary[] = [];
  const dupCounts = new Map<string, number>();
  for (const asset of items) {
    // A peer UUID is scoped to that peer. It must never match local membership just because its
    // bytes happen to equal a local UUID included elsewhere in the retained window.
    const membership = isLocal(asset.origin) ? memberToGroup.get(asset.id) : undefined;
    if (!membership) {
      visible.push(asset);
      continue;
    }
    if (seen.has(membership.group)) continue;
    seen.add(membership.group);
    visible.push(asset);
    dupCounts.set(assetSelectionKey(asset), membership.count - 1);
  }
  return { visible, dupCounts };
}
