import type React from "react";
import { useState } from "react";
import {
  FileDown,
  Keyboard,
  LayoutGrid,
  Loader2,
  Menu,
  Rows3,
  Search,
  Sparkles,
  X,
} from "lucide-react";
import type { SearchMode, SortField } from "@/api/types";
import { useViewState } from "@/lib/view-state";
import { useWriteGate } from "@/lib/write-gate";
import { shortcutLabel } from "@/lib/shortcuts";
import { AdvancedSearch } from "../AdvancedSearch";
import { ExportDialog } from "../ExportDialog";
import { SmartFolderDialog } from "../SmartFolderDialog";

/** The browse toolbar: search box + mode, advanced search, sort, the result count, manifest export,
 *  save-as-smart-folder, the grid/table toggle, and the shortcut-help affordance. Pure presentation
 *  over `useViewState` — the parent only supplies the counts and the two drawer callbacks. */
export function Toolbar({
  count,
  total,
  onOpenNav,
  onShowShortcuts,
  searching,
}: {
  count: number;
  total: number | null;
  onOpenNav?: () => void;
  onShowShortcuts?: () => void;
  searching?: boolean;
}) {
  const { state, patch, request } = useViewState();
  const { gate } = useWriteGate();
  const [showExport, setShowExport] = useState(false);
  const [showSmartFolder, setShowSmartFolder] = useState(false);
  return (
    <>
    <div className="browser-toolbar border-b border-border px-3 py-2">
      <div className="browser-toolbar-primary flex min-w-0 items-center gap-2">
      {/* Menu — opens the Navigation drawer once the layout collapses (responsive + touch pass). */}
      <button
        className="btn -ml-1 shrink-0 px-1.5 py-1 lg:hidden coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
        title={`Menu (${shortcutLabel("focus-navigation")})`}
        aria-label="Open navigation"
        aria-keyshortcuts="Control+1 Meta+1"
        onClick={onOpenNav}
      >
        <Menu size={16} />
      </button>
      <div className="browser-toolbar-search relative min-w-0 flex-1" role="search" aria-label="Search assets">
        <label htmlFor="asset-search" className="sr-only">
          Search assets
        </label>
        <Search size={13} className="absolute top-1/2 left-2 -translate-y-1/2 text-fg-dim" />
        <input
          id="asset-search"
          className="field pr-6 pl-7"
          placeholder="Search assets…"
          aria-label="Search assets"
          aria-keyshortcuts="/"
          title={`Search assets (${shortcutLabel("focus-search")})`}
          value={state.q}
          // Searching is a faceted query — it can't compose with a collection view, so typing
          // exits collection mode (mirrors the sidebar's mutual-exclusion).
          onChange={(e) =>
            patch({ q: e.target.value, collection: null }, { replace: true })
          }
        />
        {searching ? (
          <Loader2
            size={13}
            className="absolute top-1/2 right-2 -translate-y-1/2 animate-spin text-fg-dim"
            aria-label="Searching…"
          />
        ) : (
          state.q && (
            <button
              className="absolute top-1/2 right-2 -translate-y-1/2 text-fg-dim hover:text-fg"
              onClick={() => patch({ q: "" })}
              aria-label="Clear search"
            >
              <X size={13} />
            </button>
          )
        )}
      </div>
      </div>

      <div className="browser-toolbar-secondary flex min-w-0 flex-wrap items-center gap-2">

      {/* Search-mode selector (semantic-search M5): only meaningful with a text query, so it appears
          alongside the box when one is active. Hybrid/Semantic widen results with embedding
          neighbours of the matches. */}
      {state.q && (
        <select
          className="field max-w-full w-auto"
          value={state.mode}
          title="How the search text is matched"
          aria-label="Search mode"
          onChange={(e) => patch({ mode: e.target.value as SearchMode, collection: null })}
        >
          <option value="lexical">Keywords</option>
          <option value="hybrid">Keywords + similar</option>
          <option value="semantic">Most similar</option>
        </select>
      )}

      {/* Advanced Search: typed structured-attribute filters (dropdowns / ranges / toggles over the
          per-media attr columns) + tag filters. Contextual to the active media type. */}
      <AdvancedSearch />

      <select
        className="field max-w-full w-auto"
        aria-label="Sort order"
        title="Sort order"
        value={`${state.sort}:${state.dir}`}
        onChange={(e) => {
          const [sort, dir] = e.target.value.split(":") as [SortField, "asc" | "desc"];
          // Sort applies to the faceted grid; a collection view has its own order, so re-sorting
          // exits collection mode.
          patch({ sort, dir, collection: null });
        }}
      >
        {/* Relevance only ranks a text search — offer it when a query is active (or already picked,
            so the control never falls to a blank value after the query is cleared). */}
        {(state.q || state.sort === "relevance") && (
          <option value="relevance:desc">Best match</option>
        )}
        <option value="name:asc">Name ↑</option>
        <option value="name:desc">Name ↓</option>
        <option value="size:desc">Largest</option>
        <option value="size:asc">Smallest</option>
        <option value="scanned:desc">Newest</option>
        <option value="scanned:asc">Oldest</option>
      </select>

      <span className="browser-toolbar-count hidden text-[11px] whitespace-nowrap text-fg-dim tabular-nums sm:inline">
        {count.toLocaleString()}
        {total != null && total > count ? ` / ${total.toLocaleString()}` : ""}
      </span>

      {/* Export the current view — a collection when one is active, else the faceted query. */}
      <button
        className="btn shrink-0 px-1.5 py-1 disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
        aria-label="Save search as smart folder"
        onClick={() => setShowSmartFolder(true)}
        {...gate({
          disabled: state.collection !== null,
          title: state.collection
            ? "Open a search view before saving a smart folder"
            : "Save this search as a live smart folder",
        })}
      >
        <Sparkles size={14} />
      </button>

      <button
        className="btn shrink-0 px-1.5 py-1 disabled:cursor-not-allowed disabled:opacity-40 coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
        aria-label="Export manifest"
        onClick={() => setShowExport(true)}
        {...gate({ title: "Export manifest for the current view" })}
      >
        <FileDown size={14} />
      </button>

      <div className="flex overflow-hidden rounded border border-border">
        <ViewBtn
          active={state.view === "grid"}
          onClick={() => patch({ view: "grid" })}
          label="Grid view"
        >
          <LayoutGrid size={14} />
        </ViewBtn>
        <ViewBtn
          active={state.view === "table"}
          onClick={() => patch({ view: "table" })}
          label="Table view"
        >
          <Rows3 size={14} />
        </ViewBtn>
      </div>

      <button
        className="btn shrink-0 px-1.5 py-1 coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
        onClick={onShowShortcuts}
        title={`Keyboard shortcuts (${shortcutLabel("show-shortcuts")})`}
        aria-label="Keyboard shortcuts"
        aria-keyshortcuts="Shift+/"
      >
        <Keyboard size={14} />
      </button>
      </div>
    </div>

      {showExport && (
        <ExportDialog
          scope={state.collection ? { collection: state.collection } : { query: request }}
          onClose={() => setShowExport(false)}
        />
      )}
      {showSmartFolder && (
        <SmartFolderDialog query={request} onClose={() => setShowSmartFolder(false)} />
      )}
    </>
  );
}

function ViewBtn({
  active,
  onClick,
  label,
  children,
}: {
  active: boolean;
  onClick: () => void;
  /** Accessible name for the icon-only toggle (a11y — axe button-name, issue #44). */
  label: string;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      title={`${label} (${shortcutLabel("toggle-view")})`}
      aria-label={label}
      aria-pressed={active}
      aria-keyshortcuts="Control+\\ Meta+\\"
      className="flex items-center justify-center px-2 py-1 coarse:min-h-11 coarse:min-w-11"
      style={{
        background: active ? "var(--color-accent)" : "var(--color-surface-2)",
        color: active ? "var(--color-accent-fg)" : "var(--color-fg-muted)",
      }}
    >
      {children}
    </button>
  );
}
