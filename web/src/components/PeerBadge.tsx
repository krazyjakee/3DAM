import type { Origin } from "@/api/types";

// Origin attribution for federated results (issue #39): a tiny, neutral chip carrying the peer
// name, shown only when an asset came from another 3DAM instance. Local assets get nothing — the
// common case stays chrome-free (DESIGN_GUIDELINES — density). Neutral tones, not warn/danger:
// a remote origin is information, not exposure risk.

/** Renders nothing for local assets; a small "peer-name" chip otherwise. */
export function PeerBadge({ origin, className = "" }: { origin: Origin; className?: string }) {
  if (origin === "local") return null;
  return (
    <span
      className={`inline-block max-w-[8rem] shrink-0 truncate rounded align-middle text-fg-muted ${className}`}
      style={{
        background: "color-mix(in srgb, currentColor 14%, transparent)",
        fontSize: "10px",
        padding: "1px 5px",
      }}
      title={`From federated peer “${origin.peer}”`}
    >
      {origin.peer}
    </span>
  );
}
