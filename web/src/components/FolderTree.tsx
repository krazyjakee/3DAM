import { useState } from "react";
import { ChevronRight, Folder, FolderOpen } from "lucide-react";
import { useFolders } from "@/api/queries";
import type { SourceId } from "@/api/types";
import { useViewState } from "@/lib/view-state";

/** Lazy folder tree for one source (issue #66). Rendered beneath an expanded source row: each node
 *  fetches only its immediate children, and only once it is opened — so a deep hierarchy costs
 *  nothing until the user actually browses into it. Selecting a folder scopes the Browser to that
 *  subtree (source + path-prefix filter, deep-linked via the `path` URL param).
 *
 *  Rows nest via left padding keyed off `depth`; the chevron toggles expansion, the label navigates. */
export function FolderTree({
  source,
  prefix,
  depth,
  onNavigate,
}: {
  source: SourceId;
  prefix: string;
  depth: number;
  onNavigate?: () => void;
}) {
  const folders = useFolders(source, prefix, true);

  if (folders.isLoading) return <Hint depth={depth}>Loading…</Hint>;
  if (folders.isError) return <Hint depth={depth}>Couldn’t list folders.</Hint>;

  const entries = folders.data ?? [];
  // Only the root of a source explains its own emptiness; a leaf just renders nothing.
  if (entries.length === 0) return depth === 0 ? <Hint depth={depth}>No subfolders.</Hint> : null;

  return (
    <>
      {entries.map((e) => (
        <FolderNode
          key={e.name}
          source={source}
          prefix={`${prefix}${e.name}/`}
          name={e.name}
          count={e.asset_count}
          depth={depth}
          onNavigate={onNavigate}
        />
      ))}
    </>
  );
}

function FolderNode({
  source,
  prefix,
  name,
  count,
  depth,
  onNavigate,
}: {
  source: SourceId;
  prefix: string;
  name: string;
  count: number;
  depth: number;
  onNavigate?: () => void;
}) {
  const { state, patch } = useViewState();
  const [open, setOpen] = useState(false);
  const active = state.source === source && state.path === prefix && !state.collection;
  // Folder scope is a facet: it pairs with the source, and (like the other facets) exits collection
  // mode. Media/license/tag filters compose on top, so "images under Environment/Rock/" works.
  const scope = () => {
    patch({ source, path: prefix, collection: null });
    onNavigate?.();
  };
  const indent = 12 + depth * 12;

  return (
    <>
      <div
        className="group flex items-center text-xs transition-colors"
        style={{
          background: active ? "var(--color-accent-muted)" : "transparent",
          color: active ? "var(--color-accent)" : "var(--color-fg-muted)",
        }}
      >
        <button
          onClick={() => setOpen((o) => !o)}
          aria-expanded={open}
          aria-label={open ? `Collapse ${name}` : `Expand ${name}`}
          className="flex items-center justify-center py-1 text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
          style={{ paddingLeft: indent }}
        >
          <ChevronRight
            size={12}
            className="transition-transform"
            style={{ transform: open ? "rotate(90deg)" : "none" }}
          />
        </button>
        <button
          onClick={scope}
          className="flex min-w-0 flex-1 items-center gap-1.5 py-1 pr-3 text-left coarse:min-h-11"
          title={prefix}
        >
          {open ? (
            <FolderOpen size={13} className="shrink-0" />
          ) : (
            <Folder size={13} className="shrink-0" />
          )}
          <span className="truncate">{name}</span>
          <span className="ml-auto shrink-0 text-[10px] text-fg-dim tabular-nums">{count}</span>
        </button>
      </div>
      {open && (
        <FolderTree source={source} prefix={prefix} depth={depth + 1} onNavigate={onNavigate} />
      )}
    </>
  );
}

function Hint({ depth, children }: { depth: number; children: React.ReactNode }) {
  return (
    <div
      className="py-1 text-[10px] text-fg-dim italic"
      style={{ paddingLeft: 12 + depth * 12 + 16 }}
    >
      {children}
    </div>
  );
}
