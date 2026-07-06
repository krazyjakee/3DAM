import { useState } from "react";
import type { AssetSummary } from "@/api/types";
import { api } from "@/api/client";
import { MediaIcon } from "./MediaIcon";

// Capability phase 2 (Media depth): images get a real server-rendered PNG thumbnail (tech-spec
// 04 §6.4), lazily loaded via <img>. Audio/3D previews are interactive WASM islands, not server
// thumbnails, so they keep the honest typed tile here (DESIGN_GUIDELINES §4 — "no decorative
// placeholders masquerading as content"). If a thumbnail fails to load (unsupported/decode error),
// we fall back to that same tile rather than showing a broken image.

const TINT: Record<AssetSummary["media"], string> = {
  audio: "var(--color-lic-attribution)",
  image: "var(--color-lic-permissive)",
  model: "var(--color-warn)",
};

/** The honest typed tile: a media glyph + format label — never a fake preview. */
function TypedTile({ asset, size }: { asset: AssetSummary; size: number }) {
  return (
    <div
      className="flex h-full w-full flex-col items-center justify-center gap-1"
      style={{ color: TINT[asset.media], background: "var(--color-bg)" }}
    >
      <MediaIcon media={asset.media} size={size} />
      <span className="text-[10px] font-medium tracking-wide text-fg-dim uppercase">
        {asset.format}
      </span>
    </div>
  );
}

export function Thumbnail({ asset, size = 28 }: { asset: AssetSummary; size?: number }) {
  const [failed, setFailed] = useState(false);

  if (asset.media === "image" && !failed) {
    // Request roughly 2× the render box (capped) so the thumbnail stays crisp on HiDPI grids.
    const edge = Math.min(512, Math.max(64, size * 4));
    return (
      <img
        src={api.assetThumbnailUrl(asset.id, edge)}
        alt={asset.name}
        loading="lazy"
        decoding="async"
        className="h-full w-full bg-bg object-contain"
        onError={() => setFailed(true)}
      />
    );
  }

  return <TypedTile asset={asset} size={size} />;
}
