// The sidebar's collections section (issue #168, parent #96) — extracted from `Navigation.tsx`.
//
// The split falls here because this is the one part of the rail that *owns* state rather than
// reflecting it: three mutations (create/rename/delete) and the dialogs that drive them. The
// residual `Navigation` is declarative facet tables rendered through `QuickFacet`/`Row`, which
// have nothing to gain from a file of their own.
//
// The section keeps its own queries and mutations, so it mounts standalone — `Navigation` passes
// only the write gate, the active id, and what selecting/sharing one means to the view state.

import { Folder, FolderPlus, Pencil, Share2, Sparkles, Trash2 } from "lucide-react";
import {
  useCollections,
  useCreateCollection,
  useDeleteCollection,
  useUpdateCollection,
} from "@/api/queries";
import type { Collection } from "@/api/types";
import { useDialogs } from "@/lib/dialogs";
import type { WriteGate } from "@/lib/write-gate";

/** Collections & smart folders (issue #3/#108). Manual collections are created here; the Browser
 * toolbar saves or replaces smart folders from a faceted search. */
export function Collections({
  gate,
  activeId,
  onSelect,
  onShare,
}: {
  gate: WriteGate["gate"];
  activeId: string | null;
  onSelect: (id: string | null) => void;
  /** Open the sharing dialog for a collection (admin + accounts flag on); undefined hides it. */
  onShare?: (c: Collection) => void;
}) {
  const collections = useCollections();
  const create = useCreateCollection();
  const update = useUpdateCollection();
  const del = useDeleteCollection();
  const { confirm, prompt } = useDialogs();
  const items = collections.data ?? [];

  const onCreate = async () => {
    const name = (await prompt({
      title: "New manual collection",
      message: "Manual collections keep assets you add yourself. To save a live search, use the sparkles button in the browser toolbar.",
      placeholder: "Name",
      confirmLabel: "Create",
    }))?.trim();
    if (name) create.mutate({ name, kind: "manual" });
  };

  return (
    <>
      <div className="flex items-center justify-between px-3 pt-4 pb-1">
        <span className="text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
          Collections
        </span>
        <button
          className="flex items-center justify-center text-fg-dim hover:text-accent disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:text-fg-dim coarse:min-h-11 coarse:min-w-11"
          aria-label="New manual collection"
          onClick={onCreate}
          {...gate({ disabled: create.isPending, title: "New manual collection" })}
        >
          <FolderPlus size={14} />
        </button>
      </div>
      {collections.isSuccess && items.length === 0 && (
        <button
          className="mx-3 my-1 rounded border border-dashed border-border px-2 py-2 text-center text-[11px] text-fg-dim hover:border-accent hover:text-accent"
          onClick={onCreate}
        >
          + Create a manual collection
        </button>
      )}
      {items.map((c) => (
        <CollectionRow
          key={c.id}
          collection={c}
          gate={gate}
          active={activeId === c.id}
          onShare={onShare ? () => onShare(c) : undefined}
          onSelect={() => onSelect(activeId === c.id ? null : c.id)}
          onRename={async () => {
            const name = (
              await prompt({ title: "Rename collection", initial: c.name, confirmLabel: "Rename" })
            )?.trim();
            if (name && name !== c.name)
              update.mutate({ id: c.id, patch: { name } });
          }}
          onDelete={async () => {
            if (
              await confirm({
                title: `Delete collection “${c.name}”?`,
                message: "The assets themselves are untouched.",
                danger: true,
                confirmLabel: "Delete collection",
              })
            ) {
              if (activeId === c.id) onSelect(null);
              del.mutate(c.id);
            }
          }}
        />
      ))}
    </>
  );
}

export function CollectionRow({
  collection,
  gate,
  active,
  onSelect,
  onRename,
  onDelete,
  onShare,
}: {
  collection: Collection;
  gate: WriteGate["gate"];
  active: boolean;
  onSelect: () => void;
  onRename: () => void;
  onDelete: () => void;
  /** Sharing (issue #42) — present only for admins with the accounts flag on. */
  onShare?: () => void;
}) {
  const smart = collection.kind === "smart";
  return (
    <div
      className="group flex items-center gap-2 px-3 py-1 text-xs"
      style={{
        background: active ? "var(--color-accent-muted)" : "transparent",
        color: active ? "var(--color-accent)" : "var(--color-fg-muted)",
      }}
    >
      <button
        className="flex min-w-0 flex-1 items-center gap-2 text-left coarse:min-h-11"
        onClick={onSelect}
      >
        {/* Smart folders resolve a saved query live — flag them so their read-only membership reads
            as intentional, not a missing edit affordance. */}
        {smart ? (
          <Sparkles size={13} className="shrink-0" />
        ) : (
          <Folder size={13} className="shrink-0" />
        )}
        <span className="truncate" title={smart ? "Smart folder (saved query)" : collection.name}>
          {collection.name}
        </span>
        {collection.count != null && (
          <span className="text-[10px] text-fg-dim tabular-nums">{collection.count}</span>
        )}
      </button>
      <div className="hidden items-center gap-1 group-hover:flex group-focus-within:flex coarse:flex">
        {/* Sharing (issue #42): admin-only, so it bypasses the write gate — an admin always may. */}
        {onShare && (
          <button
            className="flex items-center justify-center text-fg-dim hover:text-accent coarse:min-h-11 coarse:min-w-11"
            aria-label={`Share collection ${collection.name}`}
            title="Share…"
            onClick={onShare}
          >
            <Share2 size={12} />
          </button>
        )}
        {/* A smart folder can be renamed here; replace its saved query from the Browser toolbar. */}
        <button
          className="flex items-center justify-center text-fg-dim hover:text-accent disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:text-fg-dim coarse:min-h-11 coarse:min-w-11"
          aria-label={`Rename collection ${collection.name}`}
          onClick={onRename}
          {...gate({ title: "Rename" })}
        >
          <Pencil size={12} />
        </button>
        <button
          className="flex items-center justify-center text-fg-dim hover:text-danger disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:text-fg-dim coarse:min-h-11 coarse:min-w-11"
          aria-label={`Delete collection ${collection.name}`}
          onClick={onDelete}
          {...gate({ title: "Delete" })}
        >
          <Trash2 size={12} />
        </button>
      </div>
    </div>
  );
}
