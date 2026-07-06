import type { AssetSummary } from "@/api/types";
import { MediaIcon } from "./MediaIcon";

// There is no thumbnail endpoint yet (server-rendered previews and WASM waveform islands come
// later — capability phase 2, Media depth). Per DESIGN_GUIDELINES §4 "no decorative placeholders
// masquerading as content", we
// render an honest typed tile — a media glyph + format label — NOT a fake preview image.

const TINT: Record<AssetSummary["media"], string> = {
  audio: "var(--color-lic-attribution)",
  image: "var(--color-lic-permissive)",
  model: "var(--color-warn)",
};

export function Thumbnail({ asset, size = 28 }: { asset: AssetSummary; size?: number }) {
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
