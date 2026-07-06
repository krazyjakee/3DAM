import { AudioLines, Box, Image as ImageIcon } from "lucide-react";
import type { MediaType } from "@/api/types";

const ICONS = {
  audio: AudioLines,
  image: ImageIcon,
  model: Box,
} as const;

/** Consistent per-media iconography (DESIGN_GUIDELINES §4). */
export function MediaIcon({ media, size = 14 }: { media: MediaType; size?: number }) {
  const Icon = ICONS[media];
  return <Icon size={size} strokeWidth={1.75} aria-hidden />;
}
