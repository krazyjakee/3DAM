import { useCallback, useEffect, useState } from "react";
import { Keyboard, PanelRightOpen, X } from "lucide-react";
import { useLiveUpdates } from "@/api/ws";
import { useResizableWidth, type Resizable } from "@/lib/use-resizable";
import { splitterWidthForKey } from "@/lib/splitter-keyboard";
import { useViewState } from "@/lib/view-state";
import { useFocusTrap } from "@/lib/use-focus-trap";
import { SelectionProvider, useSelection } from "@/lib/selection";
import {
  dispatchShortcut,
  emitShortcut,
  shortcutLabel,
  SHORTCUTS,
  type ShortcutId,
} from "@/lib/shortcuts";
import { Navigation } from "./Navigation";
import { Browser } from "./Browser";
import { Inspector } from "./Inspector";
import { StatusBar } from "./StatusBar";
import { Drawer } from "./Drawer";

/** The three-region workspace (DESIGN_GUIDELINES §3.1, tech-spec 09 §B.1):
 *  Navigation (left) · Browser (centre) · Inspector (right), over a live status bar.
 *
 *  Responsive: at `lg` and up the three regions sit side by side, with draggable splitters between
 *  them (issue #19 — widths persist in localStorage). Below `lg` the layout collapses to a single
 *  scrollable column — the Browser fills the screen while Navigation and the Inspector become
 *  dismissible overlay drawers (Navigation via the toolbar menu button; the Inspector via the
 *  selection bar's "Inspect" affordance — issue #33), and the splitters are hidden since there is
 *  nothing to resize. */
export function Workspace() {
  return (
    <SelectionProvider>
      <WorkspaceContent />
    </SelectionProvider>
  );
}

function WorkspaceContent() {
  useLiveUpdates(); // one WebSocket keeps every region live (tech-spec 09 §A.3)
  const { state, patch } = useViewState();
  const selection = useSelection();
  const [navOpen, setNavOpen] = useState(false);
  const [showShortcuts, setShowShortcuts] = useState(false);
  // Below `lg` the inspector is an opt-in overlay: selecting highlights in place, and this flag —
  // toggled by the selection bar — controls whether the detail drawer is up (issue #33 item 3).
  const [inspectOpen, setInspectOpen] = useState(false);
  const [navCollapsed, setNavCollapsed] = useState(
    () => typeof window !== "undefined" && localStorage.getItem("dam.navCollapsed") === "1",
  );
  const collapseNav = useCallback((value: boolean) => {
    setNavCollapsed(value);
    if (typeof window !== "undefined")
      localStorage.setItem("dam.navCollapsed", value ? "1" : "0");
  }, []);
  // On `lg` the inspector rail can be collapsed to reclaim space for the Browser (issue #65);
  // persisted so the choice survives a reload.
  const [inspectorCollapsed, setInspectorCollapsed] = useState(
    () => typeof window !== "undefined" && localStorage.getItem("dam.inspectorCollapsed") === "1",
  );
  const collapseInspector = useCallback((v: boolean) => {
    setInspectorCollapsed(v);
    if (typeof window !== "undefined")
      localStorage.setItem("dam.inspectorCollapsed", v ? "1" : "0");
  }, []);
  const nav = useResizableWidth("dam.navWidth", 220, 160, 480);
  // The inspector holds a 3D preview / full-res image + dense metadata, so let it drag meaningfully
  // wide — up to ~55% of the viewport (issue #65), with a generous floor for small screens.
  const inspectorMax =
    typeof window !== "undefined" ? Math.max(560, Math.floor(window.innerWidth * 0.55)) : 560;
  const inspector = useResizableWidth("dam.inspectorWidth", 300, 220, inspectorMax);

  useEffect(() => {
    const visibleRegion = (selector: string) =>
      [...document.querySelectorAll<HTMLElement>(selector)].find(
        (element) => element.getClientRects().length > 0,
      );
    const focusRegion = (selector: string) =>
      requestAnimationFrame(() => visibleRegion(selector)?.focus());

    const onKey = (event: KeyboardEvent) => {
      if (showShortcuts) return;
      const focusedAssetCell =
        event.target instanceof Element && event.target.closest("[data-asset-id]");
      const handled = dispatchShortcut(event, {
        "focus-search": () => document.getElementById("asset-search")?.focus(),
        "focus-navigation": () => {
          if (navCollapsed && window.matchMedia("(min-width: 64rem)").matches) {
            collapseNav(false);
            focusRegion("[data-shortcut-region='navigation']");
            return;
          }
          const nav = visibleRegion("[data-shortcut-region='navigation']");
          if (navOpen && nav?.closest("[role='dialog']")) setNavOpen(false);
          else if (nav) nav.focus();
          else {
            setNavOpen(true);
            focusRegion("[data-shortcut-region='navigation']");
          }
        },
        "focus-browser": () => focusRegion("[data-shortcut-region='browser']"),
        "focus-inspector": selection.focused
          ? () => {
              const panel = visibleRegion("[data-shortcut-region='inspector']");
              const focusedInside = panel?.contains(document.activeElement) ?? false;
              if (inspectOpen && panel?.closest("[role='dialog']")) setInspectOpen(false);
              else if (!inspectorCollapsed && focusedInside) collapseInspector(true);
              else if (panel) panel.focus();
              else {
                collapseInspector(false);
                if (!window.matchMedia("(min-width: 64rem)").matches) setInspectOpen(true);
                focusRegion("[data-shortcut-region='inspector']");
              }
            }
          : undefined,
        "toggle-view": () => patch({ view: state.view === "grid" ? "table" : "grid" }),
        "show-shortcuts": () => setShowShortcuts(true),
        "find-similar": selection.focused ? () => emitShortcut("find-similar") : undefined,
        "toggle-favourite": selection.focused ? () => emitShortcut("toggle-favourite") : undefined,
        "action-menu":
          selection.focused && !focusedAssetCell ? () => emitShortcut("action-menu") : undefined,
        // Y/N are handled by the focused suggestion chip so only that one review action fires.
      });
      if (handled) event.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [
    collapseInspector,
    collapseNav,
    inspectOpen,
    inspectorCollapsed,
    navCollapsed,
    navOpen,
    patch,
    showShortcuts,
    selection.focused,
    state.view,
  ]);

  return (
    <div className="flex h-dvh flex-col">
      <div className="flex min-h-0 flex-1">
        {/* Navigation — persistent rail on lg, overlay drawer below it. */}
        <div
          id="workspace-navigation-rail"
          className="hidden shrink-0 lg:block"
          style={{ width: navCollapsed ? 0 : nav.width }}
        >
          {!navCollapsed && <Navigation />}
        </div>
        <ResizeHandle
          resizable={nav}
          grow="right"
          label="Resize navigation"
          controls="workspace-navigation-rail"
          collapsed={navCollapsed}
          onToggleCollapse={() => collapseNav(!navCollapsed)}
        />
        <Drawer
          open={navOpen}
          onClose={() => setNavOpen(false)}
          side="left"
          label="Library navigation"
        >
          <Navigation onNavigate={() => setNavOpen(false)} />
        </Drawer>

        <Browser
          onOpenNav={() => setNavOpen(true)}
          onShowShortcuts={() => setShowShortcuts(true)}
        />

        {/* Inspector — persistent rail on lg (collapsible, issue #65); on narrow an opt-in drawer.
            Its splitter remains focusable when collapsed so Enter or an arrow can restore it. */}
        <ResizeHandle
          resizable={inspector}
          grow="left"
          label="Resize inspector"
          controls="workspace-inspector-rail"
          collapsed={inspectorCollapsed}
          onToggleCollapse={() => collapseInspector(!inspectorCollapsed)}
        />
        <Inspector
          width={inspector.width}
          open={inspectOpen}
          onClose={() => setInspectOpen(false)}
          collapsed={inspectorCollapsed}
          onCollapse={() => collapseInspector(true)}
          onExpand={() => collapseInspector(false)}
        />
      </div>
      {/* Narrow-screen selection bar: a tapped asset stays highlighted in the grid; "Inspect" raises
          the detail drawer on demand (issue #33 item 3). Absent on `lg`, where the rail is always up. */}
      <MobileSelectionBar
        onInspect={() => setInspectOpen(true)}
        onClear={() => setInspectOpen(false)}
      />
      <StatusBar />
      {showShortcuts && <ShortcutHelp onClose={() => setShowShortcuts(false)} />}
    </div>
  );
}

function ShortcutHelp({ onClose }: { onClose: () => void }) {
  const panelRef = useFocusTrap<HTMLDivElement>(true);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const groups = ["Navigate", "Assets", "Review"] as const;
  return (
    <div
      className="fixed inset-0 z-[70] flex items-center justify-center bg-black/60 p-4"
      onMouseDown={onClose}
    >
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="shortcut-help-title"
        className="max-h-[85vh] w-full max-w-lg overflow-y-auto rounded-lg border border-border bg-surface shadow-2xl"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="flex items-center border-b border-border px-4 py-3">
          <Keyboard size={16} className="mr-2 text-accent" />
          <h2 id="shortcut-help-title" className="flex-1 text-sm font-semibold text-fg">
            Keyboard shortcuts
          </h2>
          <button
            className="btn px-1.5 py-1"
            onClick={onClose}
            aria-label="Close keyboard shortcuts"
          >
            <X size={14} />
          </button>
        </div>
        <div className="space-y-4 p-4">
          {groups.map((group) => (
            <section key={group} aria-labelledby={`shortcut-group-${group.toLowerCase()}`}>
              <h3
                id={`shortcut-group-${group.toLowerCase()}`}
                className="mb-1.5 text-[10px] font-semibold tracking-wider text-fg-dim uppercase"
              >
                {group}
              </h3>
              <dl className="divide-y divide-border rounded border border-border">
                {SHORTCUTS.filter((shortcut) => shortcut.group === group).map((shortcut) => (
                  <div key={shortcut.id} className="flex items-center gap-4 px-3 py-2">
                    <dt className="min-w-0 flex-1 text-xs text-fg-muted">{shortcut.label}</dt>
                    <dd>
                      <ShortcutKey id={shortcut.id} />
                    </dd>
                  </div>
                ))}
              </dl>
            </section>
          ))}
          <p className="text-[11px] text-fg-dim">
            Letter shortcuts are paused while you type. Arrow keys move through assets; Enter selects
            one; Escape dismisses menus and drawers.
          </p>
        </div>
      </div>
    </div>
  );
}

function ShortcutKey({ id }: { id: ShortcutId }) {
  return (
    <kbd className="rounded border border-border-strong bg-surface-2 px-1.5 py-0.5 font-mono text-[11px] whitespace-nowrap text-fg">
      {shortcutLabel(id)}
    </kbd>
  );
}

/** The below-`lg` selection affordance (issue #33 item 3). When an asset is selected, this slim bar
 *  confirms the selection and offers an explicit "Inspect" — so a glance-tap highlights in place
 *  instead of throwing the full-screen inspector over the grid. Hidden entirely on `lg` (persistent
 *  rail) and when nothing is selected. */
export function MobileSelectionBar({
  onInspect,
  onClear,
}: {
  onInspect: () => void;
  onClear?: () => void;
}) {
  const selection = useSelection();
  if (selection.count === 0) return null;
  const label =
    selection.selected.kind === "results"
      ? `${selection.count.toLocaleString()} ${
          selection.selected.selector.kind === "collection" ? "collection" : "query"
        } results selected`
      : `${selection.count.toLocaleString()} asset${selection.count === 1 ? "" : "s"} selected`;
  return (
    <div className="flex items-center gap-2 border-t border-border bg-surface px-3 py-2 lg:hidden">
      <span className="min-w-0 flex-1 truncate text-xs text-fg-muted">{label}</span>
      {selection.focused && (
        <button
          className="btn btn-accent"
          onClick={onInspect}
          title={`Inspect focused asset (${shortcutLabel("focus-inspector")})`}
          aria-keyshortcuts="Control+3 Meta+3"
        >
          <PanelRightOpen size={13} /> Inspect
        </button>
      )}
      <button
        className="btn coarse:min-w-11 coarse:justify-center"
        onClick={() => {
          selection.clear();
          onClear?.();
        }}
        aria-label="Clear selection"
        title="Clear selection"
      >
        <X size={13} />
      </button>
    </div>
  );
}

/** A draggable splitter between two regions. Persistent-rail only: hidden below `lg`, where the
 *  panels are overlay drawers. `grow` says which drag direction widens the adjacent rail. */
function ResizeHandle({
  resizable,
  grow,
  label,
  controls,
  collapsed,
  onToggleCollapse,
}: {
  resizable: Resizable;
  grow: "left" | "right";
  label: string;
  controls: string;
  collapsed: boolean;
  onToggleCollapse: () => void;
}) {
  return (
    <div
      role="separator"
      tabIndex={0}
      aria-orientation="vertical"
      aria-label={label}
      aria-controls={controls}
      aria-valuemin={resizable.min}
      aria-valuemax={resizable.max}
      aria-valuenow={resizable.width}
      aria-valuetext={
        collapsed ? `Collapsed; saved width ${resizable.width} pixels` : `${resizable.width} pixels`
      }
      title={`${label}: Arrow keys resize, Home/End set limits, Enter collapses or expands`}
      onPointerDown={(event) => {
        if (collapsed) onToggleCollapse();
        resizable.startDrag(event, grow);
      }}
      onDoubleClick={onToggleCollapse}
      onKeyDown={(event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          onToggleCollapse();
          return;
        }
        const width = splitterWidthForKey({
          key: event.key,
          width: resizable.width,
          min: resizable.min,
          max: resizable.max,
          grow,
        });
        if (width == null) return;
        event.preventDefault();
        if (collapsed) onToggleCollapse();
        resizable.setWidth(width);
      }}
      className="hidden w-1 shrink-0 cursor-col-resize bg-border transition-colors hover:bg-accent focus-visible:bg-accent lg:block"
    />
  );
}
