import { useState } from "react";
import { PanelRightOpen, X } from "lucide-react";
import { useLiveUpdates } from "@/api/ws";
import { useResizableWidth, type Resizable } from "@/lib/use-resizable";
import { useViewState } from "@/lib/view-state";
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
  useLiveUpdates(); // one WebSocket keeps every region live (tech-spec 09 §A.3)
  const [navOpen, setNavOpen] = useState(false);
  // Below `lg` the inspector is an opt-in overlay: selecting highlights in place, and this flag —
  // toggled by the selection bar — controls whether the detail drawer is up (issue #33 item 3).
  const [inspectOpen, setInspectOpen] = useState(false);
  // On `lg` the inspector rail can be collapsed to reclaim space for the Browser (issue #65);
  // persisted so the choice survives a reload.
  const [collapsed, setCollapsed] = useState(
    () => typeof window !== "undefined" && localStorage.getItem("dam.inspectorCollapsed") === "1",
  );
  const collapse = (v: boolean) => {
    setCollapsed(v);
    if (typeof window !== "undefined")
      localStorage.setItem("dam.inspectorCollapsed", v ? "1" : "0");
  };
  const nav = useResizableWidth("dam.navWidth", 220, 160, 480);
  // The inspector holds a 3D preview / full-res image + dense metadata, so let it drag meaningfully
  // wide — up to ~55% of the viewport (issue #65), with a generous floor for small screens.
  const inspectorMax =
    typeof window !== "undefined" ? Math.max(560, Math.floor(window.innerWidth * 0.55)) : 560;
  const inspector = useResizableWidth("dam.inspectorWidth", 300, 220, inspectorMax);

  return (
    <div className="flex h-dvh flex-col">
      <div className="flex min-h-0 flex-1">
        {/* Navigation — persistent rail on lg, overlay drawer below it. */}
        <div className="hidden shrink-0 lg:block" style={{ width: nav.width }}>
          <Navigation />
        </div>
        <ResizeHandle resizable={nav} grow="right" label="Resize navigation" />
        <Drawer
          open={navOpen}
          onClose={() => setNavOpen(false)}
          side="left"
          label="Library navigation"
        >
          <Navigation onNavigate={() => setNavOpen(false)} />
        </Drawer>

        <Browser onOpenNav={() => setNavOpen(true)} />

        {/* Inspector — persistent rail on lg (collapsible, issue #65); on narrow an opt-in drawer.
            The splitter is hidden when the rail is collapsed — there's nothing to resize. */}
        {!collapsed && <ResizeHandle resizable={inspector} grow="left" label="Resize inspector" />}
        <Inspector
          width={inspector.width}
          open={inspectOpen}
          onClose={() => setInspectOpen(false)}
          collapsed={collapsed}
          onCollapse={() => collapse(true)}
          onExpand={() => collapse(false)}
        />
      </div>
      {/* Narrow-screen selection bar: a tapped asset stays highlighted in the grid; "Inspect" raises
          the detail drawer on demand (issue #33 item 3). Absent on `lg`, where the rail is always up. */}
      <SelectionBar onInspect={() => setInspectOpen(true)} />
      <StatusBar />
    </div>
  );
}

/** The below-`lg` selection affordance (issue #33 item 3). When an asset is selected, this slim bar
 *  confirms the selection and offers an explicit "Inspect" — so a glance-tap highlights in place
 *  instead of throwing the full-screen inspector over the grid. Hidden entirely on `lg` (persistent
 *  rail) and when nothing is selected. */
function SelectionBar({ onInspect }: { onInspect: () => void }) {
  const { state, patch } = useViewState();
  if (!state.selected) return null;
  return (
    <div className="flex items-center gap-2 border-t border-border bg-surface px-3 py-2 lg:hidden">
      <span className="min-w-0 flex-1 truncate text-xs text-fg-muted">1 asset selected</span>
      <button className="btn btn-accent" onClick={onInspect}>
        <PanelRightOpen size={13} /> Inspect
      </button>
      <button
        className="btn coarse:min-w-11 coarse:justify-center"
        onClick={() => patch({ selected: null })}
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
}: {
  resizable: Resizable;
  grow: "left" | "right";
  label: string;
}) {
  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      onPointerDown={(e) => resizable.startDrag(e, grow)}
      className="hidden w-1 shrink-0 cursor-col-resize bg-border transition-colors hover:bg-accent lg:block"
    />
  );
}
