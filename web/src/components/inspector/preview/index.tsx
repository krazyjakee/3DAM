// The Inspector's preview cluster (issue #167, parent #96) — extracted from `Inspector.tsx` as one
// module with a single entry point: {@link Preview}.
//
// It stays one module on purpose. The media lifecycle — pick the content vs preview-mesh URL, decide
// blob-vs-ticket, surface loading/error, and hand `renew` to the elements that can hit an expired
// stream ticket — is shared by all four previews below. Splitting per media type would duplicate
// that lifecycle four times and is exactly how ticket renewal would rot; the per-type components are
// deliberately dumb leaves that receive a ready `src`.
//
// Lazy WASM is preserved: the 3D island still reaches `@/wasm/dam_viewer.js` through the dynamic
// import in `@/islands/index.ts`, so the browse grid pulls zero WASM and only opening a 3D asset
// instantiates wgpu (`web/scripts/check-bundle-budgets.mjs` guards that boundary).

import { useRef, useState } from "react";
import { Box, Grid3x3 } from "lucide-react";
import { api } from "@/api/client";
import { useVersion } from "@/api/queries";
import type { Asset } from "@/api/types";
import { useMediaBlob, useMediaTicket } from "@/lib/media-blob";
import { hasInteractive3D } from "@/lib/model-formats";
import { ModelViewerIsland } from "@/islands/ModelViewerIsland";
import { AudioPlayer } from "../../AudioPlayer";
import { ImageViewer } from "../../ImageViewer";
import { Thumbnail } from "../../Thumbnail";
import { TilePreview } from "../../TilePreview";

/** The preview slot: an interactive WASM island for 3D models and audio (fed raw bytes from the
 *  file-03 content endpoint), or the server-rendered thumbnail for images and as the fallback. */
export function Preview({ asset }: { asset: Asset }) {
  const { summary } = asset;
  const [tiling, setTiling] = useState(false);
  const mediaPath =
    summary.media === "model" && hasInteractive3D(summary.format)
      ? api.assetPreviewMeshUrl(summary.id, asset.source_id)
      : api.assetContentUrl(summary.id, asset.source_id);
  const streaming = summary.media === "audio" || summary.media === "video";
  const blob = useMediaBlob(mediaPath, !streaming);
  const ticket = useMediaTicket(mediaPath, streaming);
  const media = streaming ? ticket : blob;
  if (media.status === "loading" || media.status === "idle") {
    return (
      <div className="flex aspect-square items-center justify-center border-b border-border text-xs text-fg-dim">
        Loading preview…
      </div>
    );
  }
  if (media.status === "error" || !media.url) {
    return (
      <div className="flex aspect-square items-center justify-center border-b border-border text-xs text-fg-dim">
        Preview unavailable.
      </div>
    );
  }
  const src = media.url;
  if (summary.media === "model") {
    // Interactive island for every Assimp-decodable mesh format (issue #18) — the server decodes the
    // preview blob so the DOM never resolves external buffers.
    if (hasInteractive3D(summary.format)) {
      return (
        <div className="aspect-square border-b border-border">
          <ModelViewerIsland src={src} />
        </div>
      );
    }
    // `.blend` has no interactive 3D (Assimp can't decode a modern .blend), so it falls through to
    // the server thumbnail below — which surfaces Blender's own embedded preview image when present.
    // Anything else (the USD family) has no decode path yet: show a clear "preview not available"
    // state rather than a doomed fetch or a silent blank (issue #18 acceptance).
    if (summary.format !== "blend") {
      return <NoModelPreview format={summary.format} />;
    }
  }
  if (summary.media === "audio") {
    // Playable inline: waveform + transport, with the playhead driven by real progress (issues
    // #16, #14). Keyed by id so switching assets resets playback + the decoded waveform.
    // Peaks come from the server analysis pass (issue #73), so the waveform draws with no client-side
    // audio decode; null (not yet analysed) falls back to DOM decode inside the island.
    const peaks =
      asset.attributes?.media === "audio" ? (asset.attributes.peaks ?? null) : null;
    return (
      <div className="border-b border-border">
        <AudioPlayer
          key={summary.id}
          src={src}
          assetId={summary.id}
          peaks={peaks}
          onCredentialExpired={media.renew}
        />
      </div>
    );
  }
  if (summary.media === "image") {
    // Full-resolution content image in a zoom/pan viewer — 1:1 is pixel-accurate for inspecting
    // texture detail / tileability (issue #17). Keyed by id so switching assets resets the view.
    // A "Check tiling" affordance opens the interactive tile preview (cube / flat repeat, issue #58).
    return (
      <div className="relative aspect-square border-b border-border">
        <ImageViewer key={summary.id} src={src} alt={summary.name} />
        <button
          onClick={() => setTiling(true)}
          className="absolute top-1.5 right-1.5 flex items-center gap-1 rounded border border-border bg-surface/85 px-1.5 py-1 text-[10px] text-fg-muted backdrop-blur hover:text-fg coarse:min-h-11"
          title="Check tiling — wrap this texture on a cube / flat repeat"
        >
          <Grid3x3 size={12} /> Check tiling
        </button>
        {tiling && (
          <TilePreview src={src} name={summary.name} onClose={() => setTiling(false)} />
        )}
      </div>
    );
  }
  if (summary.media === "video") {
    // No WASM island and no transcode: the browser plays the original bytes natively. `preload`
    // stays on metadata so opening the inspector doesn't pull a 200 MB cutscene down before the
    // user has asked to watch it — the server's range support (see `ranged_content_response`) is
    // what makes both that and seeking work. Playback works regardless of what the *server* can
    // decode; only the metadata and poster frame depend on its ffmpeg (ADR 0015).
    return <VideoPreview key={summary.id} src={src} onCredentialExpired={media.renew} />;
  }
  if (summary.media === "document") {
    const excerpt =
      asset.attributes?.media === "document" ? (asset.attributes.excerpt ?? null) : null;
    return <DocumentPreview excerpt={excerpt} format={summary.format} src={src} />;
  }
  return (
    <div className="aspect-square border-b border-border">
      <Thumbnail asset={summary} size={64} />
    </div>
  );
}

/** Native `<video>` playback, plus an honest note when the server has no decode backend.
 *
 *  The two are independent and it matters not to conflate them: the browser plays the original
 *  bytes whatever the server can decode, so the video is watchable either way — but with no
 *  discovered `ffmpeg`/`ffprobe` (ADR 0015) the server can't report duration/codec/resolution or
 *  render a poster frame, and every video tile in the grid stays a typed glyph. Left unexplained
 *  that reads as a broken thumbnailer rather than a missing optional dependency, so we say it once,
 *  here, where the user is already looking at a video. */
function VideoPreview({
  src,
  onCredentialExpired,
}: {
  src: string;
  onCredentialExpired: () => void;
}) {
  const version = useVersion();
  const resumeAt = useRef(0);
  const lastRenewal = useRef(0);
  // Only claim a decoder is missing once we've actually heard from the server — mid-fetch,
  // `capabilities` is undefined, and asserting "not installed" then would be a guess.
  const probeMissing =
    version.data != null && !version.data.capabilities.includes("video_probe");
  return (
    <div className="border-b border-border">
      <video
        src={src}
        controls
        preload="metadata"
        playsInline
        className="max-h-[60vh] w-full bg-black"
        onTimeUpdate={(event) => {
          resumeAt.current = event.currentTarget.currentTime;
        }}
        onLoadedMetadata={(event) => {
          if (resumeAt.current > 0) event.currentTarget.currentTime = resumeAt.current;
        }}
        onError={() => {
          // An expired ticket is opaque to HTMLMediaElement. Re-mint at most once per minute: a
          // genuinely unsupported codec fails again immediately and then stays failed, while a
          // long playback recovers after each five-minute ticket without a bearer URL.
          if (Date.now() - lastRenewal.current < 60_000) return;
          lastRenewal.current = Date.now();
          onCredentialExpired();
        }}
      >
        <track kind="captions" />
      </video>
      {probeMissing && (
        <p className="px-3 py-2 text-[11px] text-fg-dim">
          No video decoder on the server — playback works, but duration, codec and poster-frame
          thumbnails need <span className="font-mono">ffmpeg</span> installed where 3DAM runs. 3DAM
          looks for it once per run, so restart the server after installing it.
        </p>
      )}
    </div>
  );
}

/** The document preview: the extracted opening text, set as readable prose rather than rasterised
 *  server-side (PRODUCT_SPEC §9 phase 2b). A document with no text layer — a scanned-image PDF, or
 *  one not yet analysed — says so plainly instead of showing an empty card. "Open original" is the
 *  escape hatch to the real file for anything we can't typeset. */
function DocumentPreview({
  excerpt,
  format,
  src,
}: {
  excerpt: string | null;
  format: string;
  src: string;
}) {
  return (
    <div className="flex max-h-[50vh] flex-col gap-2 overflow-y-auto border-b border-border bg-surface-2 p-4">
      {excerpt ? (
        <p className="text-[12px] leading-relaxed whitespace-pre-wrap text-fg-muted">{excerpt}</p>
      ) : (
        <p className="text-[11px] text-fg-dim">
          No text extracted — this <span className="font-mono uppercase">.{format}</span> may have no
          text layer, or it hasn’t been analysed yet.
        </p>
      )}
      <a
        href={src}
        target="_blank"
        rel="noreferrer"
        className="self-start text-[11px] text-accent hover:underline coarse:min-h-11"
      >
        Open original
      </a>
    </div>
  );
}

/** "Preview not available" state for a 3D format with no decode path yet (the USD family) — issue #18.
 *  Metadata still loads below; this just makes the absent preview explicit rather than a blank tile. */
function NoModelPreview({ format }: { format: string }) {
  return (
    <div className="flex aspect-square flex-col items-center justify-center gap-2 border-b border-border bg-surface-2 px-6 text-center">
      <Box size={28} className="text-fg-dim" />
      <p className="text-xs font-medium text-fg-muted">Preview not available</p>
      <p className="text-[11px] text-fg-dim">
        3DAM can’t render <span className="font-mono uppercase">.{format}</span> yet — its metadata is
        still catalogued below.
      </p>
    </div>
  );
}
