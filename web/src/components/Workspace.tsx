import { useState } from "react";
import { useLiveUpdates } from "@/api/ws";
import { Navigation } from "./Navigation";
import { Browser } from "./Browser";
import { Inspector } from "./Inspector";
import { StatusBar } from "./StatusBar";
import { Drawer } from "./Drawer";

/** The three-region workspace (DESIGN_GUIDELINES §3.1, tech-spec 09 §B.1):
 *  Navigation (left) · Browser (centre) · Inspector (right), over a live status bar.
 *
 *  Responsive: at `lg` and up the three regions sit side by side. Below `lg`
 *  the layout collapses to a single scrollable column — the Browser fills the screen while
 *  Navigation and the Inspector become dismissible overlay drawers (Navigation via the toolbar
 *  menu button; the Inspector opens itself whenever an asset is selected). */
export function Workspace() {
  useLiveUpdates(); // one WebSocket keeps every region live (tech-spec 09 §A.3)
  const [navOpen, setNavOpen] = useState(false);

  return (
    <div className="flex h-dvh flex-col">
      <div className="flex min-h-0 flex-1">
        {/* Navigation — persistent rail on lg, overlay drawer below it. */}
        <div className="hidden w-[220px] shrink-0 lg:block">
          <Navigation />
        </div>
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
        <Inspector />
      </div>
      <StatusBar />
    </div>
  );
}
