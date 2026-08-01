/** Shared keyboard-shortcut contract for the browser and Tauri-hosted web UI (issue #119).
 *
 * Keep matching here, rather than scattering `keydown` checks across components, so the help view,
 * tooltips, tests, and dispatch logic all describe the same bindings. Plain-letter shortcuts are
 * deliberately disabled while editing text; `/` follows the same rule, while native text-input
 * conventions (including Escape and the platform's copy/paste keys) remain untouched.
 */

export type ShortcutId =
  | "focus-search"
  | "focus-navigation"
  | "focus-browser"
  | "focus-inspector"
  | "toggle-view"
  | "play-pause"
  | "find-similar"
  | "toggle-favourite"
  | "action-menu"
  | "accept-suggestion"
  | "reject-suggestion"
  | "show-shortcuts";

export interface ShortcutDefinition {
  id: ShortcutId;
  label: string;
  key: string;
  /** Platform primary modifier: Command on macOS, Control elsewhere. */
  mod?: boolean;
  shift?: boolean;
  group: "Navigate" | "Assets" | "Review";
}

export const SHORTCUTS: readonly ShortcutDefinition[] = [
  { id: "focus-search", label: "Focus search", key: "/", group: "Navigate" },
  { id: "focus-navigation", label: "Focus navigation", key: "1", mod: true, group: "Navigate" },
  { id: "focus-browser", label: "Focus asset browser", key: "2", mod: true, group: "Navigate" },
  { id: "focus-inspector", label: "Focus / dismiss inspector", key: "3", mod: true, group: "Navigate" },
  { id: "toggle-view", label: "Toggle grid / table", key: "\\", mod: true, group: "Navigate" },
  { id: "play-pause", label: "Play / pause selected audio", key: " ", group: "Assets" },
  { id: "find-similar", label: "Find similar to selected asset", key: "s", group: "Assets" },
  { id: "toggle-favourite", label: "Toggle favourite", key: "f", group: "Assets" },
  {
    id: "action-menu",
    label: "Open selected asset actions",
    key: "F10",
    shift: true,
    group: "Assets",
  },
  { id: "accept-suggestion", label: "Restore focused suggestion", key: "y", group: "Review" },
  { id: "reject-suggestion", label: "Reject focused suggestion", key: "n", group: "Review" },
  { id: "show-shortcuts", label: "Keyboard shortcuts", key: "?", shift: true, group: "Navigate" },
] as const;

export interface ShortcutKeyEvent {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  repeat?: boolean;
  target?: EventTarget | null;
}

interface EditableTarget {
  tagName?: string;
  isContentEditable?: boolean;
  closest?: (selector: string) => unknown;
}

/** Structural on purpose: it stays unit-testable without a browser DOM and also handles descendants
 * of contenteditable widgets rather than checking only `document.activeElement`. */
export function isEditableTarget(target: EventTarget | null | undefined): boolean {
  const node = target as EditableTarget | null | undefined;
  const tag = node?.tagName?.toUpperCase();
  return (
    tag === "INPUT" ||
    tag === "TEXTAREA" ||
    tag === "SELECT" ||
    node?.isContentEditable === true ||
    node?.closest?.("[contenteditable='true']") != null
  );
}

function isInteractiveTarget(target: EventTarget | null | undefined): boolean {
  const node = target as EditableTarget | null | undefined;
  const tag = node?.tagName?.toUpperCase();
  return (
    tag === "BUTTON" ||
    tag === "A" ||
    tag === "SUMMARY" ||
    node?.closest?.("button, a[href], summary, [role='button'], [role='menuitem']") != null
  );
}

function isAssetCell(target: EventTarget | null | undefined): boolean {
  return (
    (target as EditableTarget | null | undefined)?.closest?.("[data-asset-id]") != null
  );
}

export function shortcutForEvent(event: ShortcutKeyEvent): ShortcutId | null {
  if (event.repeat || isEditableTarget(event.target)) return null;

  // The dedicated Menu key is the non-chord equivalent of Shift+F10 on Windows/Linux keyboards.
  if (
    event.key === "ContextMenu" &&
    !event.ctrlKey &&
    !event.metaKey &&
    !event.altKey &&
    !event.shiftKey
  )
    return isInteractiveTarget(event.target) && !isAssetCell(event.target)
      ? null
      : "action-menu";

  const key = event.key.toLowerCase();
  for (const shortcut of SHORTCUTS) {
    const wantsMod = shortcut.mod === true;
    const hasMod = event.metaKey || event.ctrlKey;
    if (wantsMod !== hasMod || event.altKey) continue;
    if ((shortcut.shift === true) !== event.shiftKey) continue;
    if (key !== shortcut.key.toLowerCase()) continue;
    // Letter/Space/menu shortcuts must not replace the native activation of an ordinary control.
    // Asset cells are intentional command targets; `/`, `?`, and modified region commands do not
    // collide with button activation and remain available wherever focus happens to be.
    if (
      isInteractiveTarget(event.target) &&
      !isAssetCell(event.target) &&
      !shortcut.mod &&
      shortcut.id !== "focus-search" &&
      shortcut.id !== "show-shortcuts"
    )
      return null;
    return shortcut.id;
  }
  return null;
}

export type ShortcutHandlers = Partial<Record<ShortcutId, () => void>>;

/** Match and invoke in one step. Returning whether a handler ran lets the caller prevent browser
 * defaults only for shortcuts that are meaningful in the current screen/state. */
export function dispatchShortcut(event: ShortcutKeyEvent, handlers: ShortcutHandlers): boolean {
  const id = shortcutForEvent(event);
  const handler = id ? handlers[id] : undefined;
  if (!handler) return false;
  handler();
  return true;
}

export function isMacPlatform(): boolean {
  if (typeof navigator === "undefined") return false;
  return /Mac|iPhone|iPad|iPod/i.test(navigator.platform);
}

export function shortcutLabel(id: ShortcutId, mac = isMacPlatform()): string {
  const shortcut = SHORTCUTS.find((entry) => entry.id === id);
  if (!shortcut) return "";
  const parts: string[] = [];
  if (shortcut.mod) parts.push(mac ? "⌘" : "Ctrl");
  if (shortcut.shift) parts.push(mac ? "⇧" : "Shift");
  const key =
    shortcut.key === " "
      ? "Space"
      : shortcut.key.length === 1
        ? shortcut.key.toUpperCase()
        : shortcut.key;
  parts.push(key);
  return mac ? parts.join("") : parts.join("+");
}

export const SHORTCUT_EVENT = "dam:shortcut";

export function emitShortcut(id: ShortcutId): void {
  window.dispatchEvent(new CustomEvent<ShortcutId>(SHORTCUT_EVENT, { detail: id }));
}
