import type React from "react";
import { useCallback, useEffect, useRef, useState } from "react";

/** Keyboard roving-focus for the virtualised grid/table (issue #27). Tabbing through a 100k-item
 *  list is impractical, so exactly one cell is a tab stop (roving `tabindex`); arrow keys move it,
 *  Home/End jump to the ends, and native `<button>` semantics turn Enter/Space into selection.
 *  `cols` is the row stride — 1 for the table (horizontal arrows are ignored), the live column
 *  count for the grid. `scrollToItem` pulls the target into the virtual window before we hand it
 *  DOM focus. Returns the focused index, a setter (so a mouse click/focus can re-seat the tab stop),
 *  and the container key handler. */
export function useRovingFocus(
  itemCount: number,
  cols: number,
  parentRef: React.RefObject<HTMLDivElement | null>,
  scrollToItem: (index: number) => void,
) {
  const [focusIndex, setFocusIndex] = useState(0);
  const moveFocus = useRef(false);

  // Keep the roving index in range as the list grows (infinite scroll) or shrinks (new query).
  useEffect(() => {
    if (itemCount > 0) setFocusIndex((i) => Math.min(i, itemCount - 1));
  }, [itemCount]);

  // Once a key moves the index, pull the target into the virtual window and give it real DOM focus.
  // A large jump can render a frame late, so retry once on the next frame.
  useEffect(() => {
    if (!moveFocus.current) return;
    moveFocus.current = false;
    const focus = () =>
      parentRef.current?.querySelector<HTMLElement>(`[data-index="${focusIndex}"]`)?.focus();
    focus();
    const raf = requestAnimationFrame(focus);
    return () => cancelAnimationFrame(raf);
  }, [focusIndex, parentRef]);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      let next: number | null = null;
      switch (e.key) {
        case "ArrowRight":
          if (cols === 1) return;
          next = focusIndex + 1;
          break;
        case "ArrowLeft":
          if (cols === 1) return;
          next = focusIndex - 1;
          break;
        case "ArrowDown":
          next = focusIndex + cols;
          break;
        case "ArrowUp":
          next = focusIndex - cols;
          break;
        case "Home":
          next = 0;
          break;
        case "End":
          next = itemCount - 1;
          break;
        default:
          return;
      }
      // Out of range → swallow the key so the scroll container doesn't also pan, but don't move.
      if (next < 0 || next >= itemCount) {
        e.preventDefault();
        return;
      }
      e.preventDefault();
      moveFocus.current = true;
      setFocusIndex(next);
      scrollToItem(next);
    },
    [focusIndex, cols, itemCount, scrollToItem],
  );

  return { focusIndex, setFocusIndex, onKeyDown };
}
