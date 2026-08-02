// Draggable sidebar widths (issue #19). A tiny pointer-drag hook that tracks a panel width in px,
// clamps it to a [min,max], and persists it to localStorage so the layout survives a reload. Only
// the persistent `lg` rails use this; below `lg` the panels are overlay drawers (Workspace) and the
// handles are hidden, so there is nothing to resize on touch.

import { useCallback, useEffect, useState } from "react";

function clamp(n: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, n));
}

export interface Resizable {
  width: number;
  min: number;
  max: number;
  /** Set a keyboard-selected width; the hook clamps and persists it like pointer dragging. */
  setWidth: (width: number) => void;
  /** Attach to a handle's `onPointerDown`. `grow` says which drag direction widens the panel:
   *  `"right"` for a left rail (handle on its right edge), `"left"` for a right rail. */
  startDrag: (e: React.PointerEvent, grow: "left" | "right") => void;
}

export function useResizableWidth(
  key: string,
  initial: number,
  min: number,
  max: number,
): Resizable {
  const [width, setWidth] = useState<number>(() => {
    if (typeof window === "undefined") return initial;
    const saved = Number(window.localStorage.getItem(key));
    return saved && !Number.isNaN(saved) ? clamp(saved, min, max) : initial;
  });

  useEffect(() => {
    window.localStorage.setItem(key, String(width));
  }, [key, width]);

  const startDrag = useCallback(
    (e: React.PointerEvent, grow: "left" | "right") => {
      e.preventDefault();
      const startX = e.clientX;
      const startW = width;
      const onMove = (ev: PointerEvent) => {
        const delta = grow === "right" ? ev.clientX - startX : startX - ev.clientX;
        setWidth(clamp(startW + delta, min, max));
      };
      const onUp = () => {
        window.removeEventListener("pointermove", onMove);
        window.removeEventListener("pointerup", onUp);
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
      };
      // While dragging, keep the resize cursor and suppress text selection everywhere.
      document.body.style.cursor = "col-resize";
      document.body.style.userSelect = "none";
      window.addEventListener("pointermove", onMove);
      window.addEventListener("pointerup", onUp);
    },
    [width, min, max],
  );

  const setClampedWidth = useCallback(
    (next: number) => setWidth(clamp(next, min, max)),
    [min, max],
  );

  return { width, min, max, setWidth: setClampedWidth, startDrag };
}
