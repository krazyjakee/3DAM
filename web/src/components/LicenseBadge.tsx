import type { LicenseBadge as LicenseBadgeT, LicenseStatus } from "@/api/types";
import { licenseColorVar, licenseLabel } from "@/lib/format";

/** Colour-coded license marker (PRODUCT_SPEC §License). Unknown is never styled as "safe".
 *  `prominent` renders the large inspector badge; the compact form is a grid/table dot.
 *
 *  **Unknown is always rendered** (issue #106). It used to be dropped entirely in compact contexts,
 *  which made "nobody has established what you may do with this" visually identical to "fine" — the
 *  exact silent-permissive failure DESIGN_GUIDELINES §3.1 and PRODUCT_SPEC §5 forbid. It is shown in
 *  the muted `--color-lic-unknown` token rather than a warn colour, so a freshly-scanned library
 *  reads as *unverified* instead of as a wall of alarms, and its dot is a hollow ring rather than a
 *  filled one — a settled status is a solid mark, an unestablished one is an outline. */
export function LicenseBadge({
  badge,
  prominent = false,
}: {
  badge: { id: string | null; status: LicenseStatus } | LicenseBadgeT;
  prominent?: boolean;
}) {
  const color = licenseColorVar[badge.status];
  // An id with an unknown status means the licence is *named* but its terms were never established
  // (any of the four rights still NULL) — say so rather than showing the bare id as if it settled it.
  const unverified = badge.status === "unknown";
  const text =
    badge.id ?? (unverified && prominent ? "No licence recorded" : licenseLabel[badge.status]);
  const title = !unverified
    ? `License: ${text}`
    : badge.id
      ? `License: ${badge.id} — rights not established, so this is not verified`
      : "License unknown — nothing recorded. Don’t assume it is safe to ship.";

  const dot = (size: string) => (
    <span
      className={`${size} shrink-0 rounded-full`}
      style={
        unverified
          ? { border: `1px solid ${color}`, background: "transparent" }
          : { background: color }
      }
      aria-hidden
    />
  );

  if (prominent) {
    return (
      <span
        className="inline-flex items-center gap-2 rounded px-2 py-1 text-xs font-medium"
        style={{ color, background: "color-mix(in srgb, currentColor 12%, transparent)" }}
        title={title}
      >
        {dot("h-2 w-2")}
        {text}
      </span>
    );
  }

  return (
    <span className="inline-flex items-center gap-1 text-[11px]" style={{ color }} title={title}>
      {dot("h-1.5 w-1.5")}
      <span className="truncate">{text}</span>
    </span>
  );
}
