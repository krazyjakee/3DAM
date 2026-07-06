// Small display helpers. Content-truthful, information-dense (DESIGN_GUIDELINES §4).

import type { LicenseStatus, MediaType, Origin, SourceState } from "@/api/types";

export function bytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const u = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${u[i]}`;
}

export function relTime(epochSecs: number | null | undefined): string {
  if (!epochSecs) return "—";
  const diff = Date.now() / 1000 - epochSecs;
  if (diff < 60) return "just now";
  const m = Math.floor(diff / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  if (d < 30) return `${d}d ago`;
  return new Date(epochSecs * 1000).toLocaleDateString();
}

export function duration(ms: number | null | undefined): string {
  if (ms == null) return "—";
  const s = ms / 1000;
  const m = Math.floor(s / 60);
  const rem = Math.round(s % 60);
  return m > 0 ? `${m}:${rem.toString().padStart(2, "0")}` : `${s.toFixed(1)}s`;
}

export const mediaLabel: Record<MediaType, string> = {
  audio: "Audio",
  image: "Image",
  model: "3D Model",
};

export function originLabel(o: Origin): string {
  return o === "local" ? "Local" : `Peer: ${o.peer}`;
}

export function sourceStateLabel(s: SourceState): string {
  if (typeof s === "string") return s;
  return `error: ${s.error}`;
}

export const licenseColorVar: Record<LicenseStatus, string> = {
  permissive: "var(--color-lic-permissive)",
  attribution: "var(--color-lic-attribution)",
  restricted: "var(--color-lic-restricted)",
  unknown: "var(--color-lic-unknown)",
};

export const licenseLabel: Record<LicenseStatus, string> = {
  permissive: "Permissive",
  attribution: "Attribution",
  restricted: "Restricted",
  unknown: "Unknown",
};
