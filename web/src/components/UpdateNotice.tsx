import { Link, useLocation } from "react-router";
import { useDesktopUpdates } from "../api/updates";

/** A shared observer announces updates across the library and account routes. */
export function UpdateNotice() {
  const { data } = useDesktopUpdates();
  const location = useLocation();
  if (!data?.supported || location.pathname === "/updates" || !["available", "ready"].includes(data.phase)) return null;
  return (
    <aside className="fixed right-4 bottom-12 z-40 flex max-w-xs flex-col gap-2 rounded-lg border border-accent/30 bg-surface p-4 text-xs shadow-lg" aria-label="Desktop update">
      <span>{data.phase === "ready" ? "3DAM update installed. Restart when ready." : `3DAM ${data.release?.version} is available.`}</span>
      <Link to="/updates" className="font-medium text-accent hover:underline">{data.phase === "ready" ? "Finish update" : "Review update"}</Link>
    </aside>
  );
}
