// Focus management for modal overlays (issue #26). While active it (1) moves focus into the overlay
// on open — unless focus is already inside, e.g. an `autoFocus` field — (2) cycles Tab / Shift+Tab
// within the overlay so focus can't escape behind the scrim, and (3) restores focus to whatever was
// focused before (the trigger) when it deactivates. Attach the returned ref to the overlay element.

import { useEffect, useRef } from "react";

const FOCUSABLE = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  '[tabindex]:not([tabindex="-1"])',
].join(",");

export function useFocusTrap<T extends HTMLElement>(active: boolean) {
  const ref = useRef<T>(null);

  useEffect(() => {
    if (!active) return;
    const node = ref.current;
    if (!node) return;

    const previouslyFocused = document.activeElement as HTMLElement | null;

    // Visible, tabbable descendants in DOM order (`offsetParent` is null for `display:none`).
    const focusables = () =>
      Array.from(node.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
        (el) => el.offsetParent !== null || el === document.activeElement,
      );

    // Pull focus inside on open, but don't fight an autoFocus field that's already inside.
    if (!node.contains(document.activeElement)) {
      focusables()[0]?.focus();
    }

    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Tab") return;
      const items = focusables();
      if (items.length === 0) {
        e.preventDefault();
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      const el = document.activeElement as HTMLElement | null;
      if (e.shiftKey) {
        if (el === first || !node.contains(el)) {
          e.preventDefault();
          last.focus();
        }
      } else if (el === last || !node.contains(el)) {
        e.preventDefault();
        first.focus();
      }
    };

    node.addEventListener("keydown", onKey);
    return () => {
      node.removeEventListener("keydown", onKey);
      // Return focus to the trigger so keyboard users aren't dumped at the top of the page.
      previouslyFocused?.focus?.();
    };
  }, [active]);

  return ref;
}
