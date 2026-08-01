/** Pure camera interaction math for the 3D island. DOM pointer plumbing stays in the wrapper; these
 * functions are deterministic and covered without requiring WASM, a browser GPU, or real hardware. */

export const DEFAULT_VIEWER_POSE: ViewerPose = {
  // Framing convention v1; mirrored and parity-tested in dam-render/dam-viewer.
  yaw: 0.7328151,
  pitch: 0.45071298,
  zoom: 1,
  panX: 0,
  panY: 0,
};

export interface ViewerPose {
  yaw: number;
  pitch: number;
  zoom: number;
  panX: number;
  panY: number;
}

export interface Point {
  x: number;
  y: number;
}

export const MIN_PITCH = -Math.PI / 2 + 0.05;
export const MAX_PITCH = Math.PI / 2 - 0.05;
export const MIN_ZOOM = 0.35;
export const MAX_ZOOM = 10;

const clamp = (value: number, min: number, max: number) =>
  Math.min(max, Math.max(min, value));

export function orbit(pose: ViewerPose, dx: number, dy: number): ViewerPose {
  return {
    ...pose,
    yaw: pose.yaw + dx * 0.01,
    pitch: clamp(pose.pitch + dy * 0.01, MIN_PITCH, MAX_PITCH),
  };
}

export function pan(
  pose: ViewerPose,
  dx: number,
  dy: number,
  width: number,
  height: number,
): ViewerPose {
  return {
    ...pose,
    panX: clamp(pose.panX - dx / Math.max(1, width), -2, 2),
    panY: clamp(pose.panY + dy / Math.max(1, height), -2, 2),
  };
}

export function zoom(pose: ViewerPose, delta: number): ViewerPose {
  return {
    ...pose,
    zoom: clamp(pose.zoom * Math.exp(delta * 0.0015), MIN_ZOOM, MAX_ZOOM),
  };
}

export function midpoint(a: Point, b: Point): Point {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

export function distance(a: Point, b: Point): number {
  return Math.hypot(a.x - b.x, a.y - b.y);
}

/** Two-pointer gesture: midpoint movement pans; increasing finger separation zooms in. */
export function pinchPan(
  pose: ViewerPose,
  previous: [Point, Point],
  current: [Point, Point],
  width: number,
  height: number,
): ViewerPose {
  const beforeMidpoint = midpoint(previous[0], previous[1]);
  const afterMidpoint = midpoint(current[0], current[1]);
  const beforeDistance = Math.max(1, distance(previous[0], previous[1]));
  const afterDistance = Math.max(1, distance(current[0], current[1]));
  const panned = pan(
    pose,
    afterMidpoint.x - beforeMidpoint.x,
    afterMidpoint.y - beforeMidpoint.y,
    width,
    height,
  );
  return {
    ...panned,
    zoom: clamp(panned.zoom * (beforeDistance / afterDistance), MIN_ZOOM, MAX_ZOOM),
  };
}

export type ViewerKeyAction =
  | "orbit-left"
  | "orbit-right"
  | "orbit-up"
  | "orbit-down"
  | "pan-left"
  | "pan-right"
  | "pan-up"
  | "pan-down"
  | "zoom-in"
  | "zoom-out"
  | "reset";

export function keyAction(key: string, shift: boolean): ViewerKeyAction | null {
  if (key === "Home" || key === "0") return "reset";
  if (key === "+" || key === "=" || key === "PageUp") return "zoom-in";
  if (key === "-" || key === "_" || key === "PageDown") return "zoom-out";
  const direction = key.startsWith("Arrow") ? key.slice(5).toLowerCase() : "";
  if (!["left", "right", "up", "down"].includes(direction)) return null;
  return `${shift ? "pan" : "orbit"}-${direction}` as ViewerKeyAction;
}

export function keyboard(pose: ViewerPose, action: ViewerKeyAction): ViewerPose {
  switch (action) {
    case "orbit-left":
      return orbit(pose, -12, 0);
    case "orbit-right":
      return orbit(pose, 12, 0);
    case "orbit-up":
      return orbit(pose, 0, -12);
    case "orbit-down":
      return orbit(pose, 0, 12);
    case "pan-left":
      return { ...pose, panX: clamp(pose.panX + 0.05, -2, 2) };
    case "pan-right":
      return { ...pose, panX: clamp(pose.panX - 0.05, -2, 2) };
    case "pan-up":
      return { ...pose, panY: clamp(pose.panY - 0.05, -2, 2) };
    case "pan-down":
      return { ...pose, panY: clamp(pose.panY + 0.05, -2, 2) };
    case "zoom-in":
      return zoom(pose, -120);
    case "zoom-out":
      return zoom(pose, 120);
    case "reset":
      return { ...DEFAULT_VIEWER_POSE };
  }
}
