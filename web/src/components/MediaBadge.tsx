import type { MediaType } from "@/api/types";

// A small, colour-coded media-type tag (DESIGN_GUIDELINES — "Media badges"). One hue + short label
// per media type so a mixed grid/table reads at a glance: SFX (audio, teal), IMG (image, orange),
// 3D (model, indigo). Uses the per-media design tokens, not the accent.

const MEDIA: Record<MediaType, { label: string; color: string }> = {
  audio: { label: "SFX", color: "var(--color-media-audio)" },
  image: { label: "IMG", color: "var(--color-media-image)" },
  model: { label: "3D", color: "var(--color-media-model)" },
};

/** The media-type badge. `size="sm"` for dense table rows, default for grid overlays. */
export function MediaBadge({ media, className = "" }: { media: MediaType; className?: string }) {
  const m = MEDIA[media];
  return (
    <span
      className={`inline-block rounded font-bold tracking-wide uppercase ${className}`}
      style={{
        color: m.color,
        background: "color-mix(in srgb, currentColor 16%, transparent)",
        fontSize: "10px",
        padding: "1px 5px",
      }}
      title={media}
    >
      {m.label}
    </span>
  );
}
