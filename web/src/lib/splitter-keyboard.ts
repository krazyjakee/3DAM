export type SplitterKey = "ArrowLeft" | "ArrowRight" | "Home" | "End";

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

/** Resolve the next rail width. Physical arrow direction follows the splitter: Right grows a left
 * rail, while Left grows a right rail. Home/End announce and select the supported extrema. */
export function splitterWidthForKey({
  key,
  width,
  min,
  max,
  grow,
  step = 10,
}: {
  key: string;
  width: number;
  min: number;
  max: number;
  grow: "left" | "right";
  step?: number;
}): number | null {
  if (key === "Home") return min;
  if (key === "End") return max;
  if (key !== "ArrowLeft" && key !== "ArrowRight") return null;
  const physicalDelta = key === "ArrowRight" ? step : -step;
  const panelDelta = grow === "right" ? physicalDelta : -physicalDelta;
  return clamp(width + panelDelta, min, max);
}
