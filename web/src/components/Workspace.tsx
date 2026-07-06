import { useState } from "react";
import { useLiveUpdates } from "@/api/ws";
import { useResizableWidth, type Resizable } from "@/lib/use-resizable";
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
 *  dismissible overlay drawers (Navigation via the toolbar menu button; the Inspector opens itself
 *  whenever an asset is selected), and the splitters are hidden since there is nothing to resize. */
export function Workspace() {
  useLiveUpdates(); // one WebSocket keeps every region live (tech-spec 09 §A.3)
  const [navOpen, setNavOpen] = useState(false);
  const nav = useResizableWidth("dam.navWidth", 220, 160, 480);
  const inspector = useResizableWidth("dam.inspectorWidth", 300, 220, 560);

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

        {/* Inspector — persistent rail on lg; on narrow it opens itself as a drawer when selected. */}
        <ResizeHandle resizable={inspector} grow="left" label="Resize inspector" />
        <Inspector width={inspector.width} />
      </div>
      <StatusBar />
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
