import type { LicenseBadge as LicenseBadgeT, LicenseStatus } from "@/api/types";
import { licenseColorVar, licenseLabel } from "@/lib/format";

/** Colour-coded license marker (PRODUCT_SPEC §License). Unknown is never styled as "safe".
 *  `prominent` renders the large inspector badge; the compact form is a grid/table dot. */
export function LicenseBadge({
  badge,
  prominent = false,
}: {
  badge: { id: string | null; status: LicenseStatus } | LicenseBadgeT;
  prominent?: boolean;
}) {
  const color = licenseColorVar[badge.status];
  const text = badge.id ?? licenseLabel[badge.status];

  // An unknown, unidentified license conveys nothing — the common case for local game assets (#54).
  // Don't spend a prime grid/table slot on it: compact contexts drop it entirely, and the inspector
  // keeps only a muted one-liner. License stays prominent when it's actually meaningful.
  if (badge.status === "unknown" && !badge.id) {
    if (!prominent) return null;
    return (
      <span className="text-[11px] text-fg-dim" title="No license information recorded">
        No license info
      </span>
    );
  }

  if (prominent) {
    return (
      <span
        className="inline-flex items-center gap-2 rounded px-2 py-1 text-xs font-medium"
        style={{ color, background: "color-mix(in srgb, currentColor 12%, transparent)" }}
        title={`License: ${text}`}
      >
        <span className="h-2 w-2 rounded-full" style={{ background: color }} />
        {text}
      </span>
    );
  }

  return (
    <span
      className="inline-flex items-center gap-1 text-[11px]"
      style={{ color }}
      title={`License: ${text}`}
    >
      <span className="h-1.5 w-1.5 shrink-0 rounded-full" style={{ background: color }} />
      <span className="truncate">{text}</span>
    </span>
  );
}
