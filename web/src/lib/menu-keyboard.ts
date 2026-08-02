/** Keyboard commands shared by the DOM menu implementation and its focused regression tests. */
export type MenuKeyAction =
  | "next"
  | "previous"
  | "first"
  | "last"
  | "open-submenu"
  | "close-submenu"
  | "close-menu"
  | "tab-away";

export function menuKeyAction(
  key: string,
  options: { inSubmenu: boolean; hasSubmenu: boolean },
): MenuKeyAction | null {
  switch (key) {
    case "ArrowDown":
      return "next";
    case "ArrowUp":
      return "previous";
    case "Home":
      return "first";
    case "End":
      return "last";
    case "ArrowRight":
      return options.hasSubmenu ? "open-submenu" : null;
    case "ArrowLeft":
      return options.inSubmenu ? "close-submenu" : null;
    case "Escape":
      return options.inSubmenu ? "close-submenu" : "close-menu";
    case "Tab":
      return "tab-away";
    default:
      return null;
  }
}

/** Wrap focus within one menu level. Nested submenu items are supplied separately by the caller. */
export function menuItemIndex(
  current: number,
  count: number,
  action: "next" | "previous" | "first" | "last",
): number | null {
  if (count === 0) return null;
  if (action === "first") return 0;
  if (action === "last") return count - 1;
  if (current < 0) return action === "previous" ? count - 1 : 0;
  return action === "next" ? (current + 1) % count : (current - 1 + count) % count;
}
