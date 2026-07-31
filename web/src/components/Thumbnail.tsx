import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import type { AssetSummary } from "@/api/types";
import { api } from "@/api/client";
import { useThumbnailVersion } from "@/lib/thumbnail-cache";
import { MediaIcon } from "./MediaIcon";

// Images get a real server-rendered PNG thumbnail (tech-spec 04 §6.4); 3D models get a server-side
// wgpu turntable render (ADR 0002); video gets a poster frame grabbed at ~10% duration by a
// discovered ffmpeg (ADR 0015) — all lazily loaded via <img>. Audio previews are interactive
// WASM islands with no server thumbnail, so audio keeps the honest typed tile here
// (DESIGN_GUIDELINES §4 — "no decorative placeholders masquerading as content").
//
// Documents keep the typed tile as well. Their excerpt is real content and it *is* shown — in the
// Inspector, where there is room to read it. Shrinking 280 characters into a ~120px tile would
// render unreadable grey texture: decoration wearing the costume of content, which is precisely
// what that guideline rules out. The tile shows the glyph, the format, and the page/word counts
// that already ride along in `key_attrs`.
//
// While a thumbnail is still fetching/generating we show a neutral placeholder (the typed tile
// glyph) with a loading spinner overlay rather than raw alt text (issue #53); on load we swap to the
// image, and on a decode/unsupported/no-GPU error we fall back to the same typed tile — so a model
// on a GPU-less host, a format the renderer can't decode yet, or a video on a server with no ffmpeg
// installed all degrade the same graceful way.

/** Media whose preview the server can render to a PNG the grid shows via <img>. */
function hasServerThumbnail(media: AssetSummary["media"]): boolean {
  return media === "image" || media === "model" || media === "video";
}

const TINT: Record<AssetSummary["media"], string> = {
  audio: "var(--color-lic-attribution)",
  image: "var(--color-lic-permissive)",
  model: "var(--color-warn)",
  video: "var(--color-media-video)",
  document: "var(--color-media-document)",
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
  const [status, setStatus] = useState<"loading" | "loaded" | "failed">("loading");
  // Regeneration epoch: bumped when the user forces a rebuild, so the <img> re-fetches past the cache.
  const version = useThumbnailVersion(asset.id);

  // A virtualised cell may be reused for a different asset without remounting — reset when the id
  // (or regeneration epoch) changes so the placeholder/spinner tracks the new image.
  useEffect(() => setStatus("loading"), [asset.id, version]);

  // Media with no server thumbnail (audio), or a preview that failed to load, shows the typed tile.
  if (!hasServerThumbnail(asset.media) || status === "failed") {
    return <TypedTile asset={asset} size={size} />;
  }

  // Request roughly 2× the render box (capped) so the thumbnail stays crisp on HiDPI grids.
  const edge = Math.min(512, Math.max(64, size * 4));
  return (
    <div className="relative h-full w-full" style={{ background: "var(--color-bg)" }}>
      {status === "loading" && (
        <div className="absolute inset-0 flex items-center justify-center" aria-hidden>
          <span className="opacity-20" style={{ color: TINT[asset.media] }}>
            <MediaIcon media={asset.media} size={size} />
          </span>
          <Loader2 className="absolute animate-spin text-fg-dim" size={Math.max(14, size / 2)} />
        </div>
      )}
      <img
        src={api.assetThumbnailUrl(asset.id, edge, version)}
        alt={asset.name}
        loading="lazy"
        decoding="async"
        className="h-full w-full object-contain transition-opacity"
        style={{ opacity: status === "loaded" ? 1 : 0 }}
        onLoad={() => setStatus("loaded")}
        onError={() => setStatus("failed")}
      />
    </div>
  );
}
