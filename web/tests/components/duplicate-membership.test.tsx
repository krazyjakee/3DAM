import { describe, expect, it } from "vitest";
import type { AssetSummary } from "@/api/types";
import { collapseExactDuplicates } from "@/lib/duplicate-membership";
import { assetSelectionKey } from "@/lib/selection";

function asset(id: string, origin: AssetSummary["origin"], sourceId: string | null): AssetSummary {
  return {
    id,
    name: id,
    media: "image",
    format: "png",
    size: 1,
    license: { id: null, status: "unknown" },
    top_tags: [],
    origin,
    key_attrs: {},
    favorite: false,
    source_id: sourceId,
  };
}

describe("page-local duplicate membership", () => {
  it("never applies local membership or badges to a peer row with the same UUID", () => {
    const peer = asset("shared-id", { peer: "remote" }, "peer-source");
    const local = asset("shared-id", "local", "local-source");
    const localTwin = asset("local-twin", "local", "local-source");

    const result = collapseExactDuplicates([peer, local, localTwin], [
      { asset: local.id, group: "hash", count: 2 },
      { asset: localTwin.id, group: "hash", count: 2 },
    ]);

    expect(result.visible).toEqual([peer, local]);
    expect(result.dupCounts.get(assetSelectionKey(peer))).toBeUndefined();
    expect(result.dupCounts.get(assetSelectionKey(local))).toBe(1);
  });
});
