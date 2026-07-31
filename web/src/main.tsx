import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";

// Swallow file drops that miss the Upload drop zone (issue #80).
//
// A browser's default action for a file dropped on a page is to *navigate to it*. Once an Upload
// view exists, users aim files at the window — and a drop landing just short of the zone, or on the
// asset grid where the instinct points, would replace the whole SPA with a raw image. That is
// merely annoying in a tab; in the desktop shell there is no address bar and no Back item in the
// menu (ADR 0013), so the only recovery is quitting and relaunching, losing every queued file.
//
// Registered here rather than in a component: it must hold on every route, from before React
// mounts, and it never needs tearing down. The drop zone's own handlers call `preventDefault` and
// return, so genuine drops are unaffected.
for (const type of ["dragover", "drop"] as const) {
  window.addEventListener(type, (e) => e.preventDefault());
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
