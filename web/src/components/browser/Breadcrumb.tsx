import { Fragment } from "react";
import { ChevronRight, Layers } from "lucide-react";
import { useSources } from "@/api/queries";
import { useViewState } from "@/lib/view-state";

/** Folder breadcrumb (issue #66): when a folder filter is active, show the source name
 *  followed by each path segment, `/`-separated. Clicking a crumb re-scopes to that level (the
 *  source name clears the folder path entirely); the trailing crumb is the current folder and is
 *  inert. Hidden in collection views and without a folder filter. Reads/writes the same `source` +
 *  `path` view state as the search filters. */
export function Breadcrumb() {
  const { state, patch } = useViewState();
  const sources = useSources();
  if (!state.source || !state.path || state.collection) return null;

  const sourceName = sources.data?.find((s) => s.id === state.source)?.name ?? "Source";
  const segs = (state.path ?? "").split("/").filter(Boolean);
  const crumbs: { label: string; path: string | null }[] = [{ label: sourceName, path: null }];
  let acc = "";
  for (const seg of segs) {
    acc += `${seg}/`;
    crumbs.push({ label: seg, path: acc });
  }

  return (
    <nav
      aria-label="Folder path"
      className="flex items-center gap-1 overflow-x-auto border-b border-border bg-surface px-3 py-1 text-[11px] text-fg-dim"
    >
      <Layers size={12} className="mr-0.5 shrink-0 text-fg-dim" />
      {crumbs.map((c, i) => {
        const last = i === crumbs.length - 1;
        return (
          <Fragment key={i}>
            {i > 0 && <ChevronRight size={11} className="shrink-0 opacity-60" />}
            <button
              className={`max-w-[10rem] shrink-0 truncate coarse:min-h-11 ${
                last ? "font-medium text-fg-muted" : "hover:text-accent"
              }`}
              disabled={last}
              onClick={() => patch({ path: c.path })}
              title={c.label}
            >
              {c.label}
            </button>
          </Fragment>
        );
      })}
      {/* Subtree vs this-folder-only (issue #66). Only meaningful once a folder is actually
          selected — at the source root "include subfolders" off would mean the loose files at the
          top level, which is a real scope but not one worth a control nobody asked for. */}
      {state.path && (
        <label
          className="ml-auto flex shrink-0 items-center gap-1.5 pl-3 select-none coarse:min-h-11"
          title="Off: show only the files filed directly in this folder, not those in its subfolders."
        >
          <input
            type="checkbox"
            className="accent-accent"
            checked={state.subfolders}
            onChange={(e) => patch({ subfolders: e.target.checked })}
          />
          <span>Subfolders</span>
        </label>
      )}
    </nav>
  );
}
