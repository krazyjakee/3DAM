import type React from "react";
import { useState } from "react";
import { Link } from "react-router-dom";
import {
  AudioLines,
  Box,
  Copy,
  Folder,
  FolderPlus,
  Image as ImageIcon,
  Layers,
  Library,
  Pencil,
  RefreshCw,
  Settings as SettingsIcon,
  Sparkles,
  Trash2,
  WifiOff,
} from "lucide-react";
import {
  useAnalyze,
  useCollections,
  useCreateCollection,
  useDeleteCollection,
  useRemoveSource,
  useRenameCollection,
  useScan,
  useSources,
  useStats,
} from "@/api/queries";
import { useConnection } from "@/api/connection";
import type { Collection, LicenseStatus, MediaType, SourceInfo } from "@/api/types";
import { licenseColorVar, licenseLabel, sourceStateLabel } from "@/lib/format";
import { useViewState } from "@/lib/view-state";
import { AddSourceDialog } from "./AddSourceDialog";

const MEDIA: { key: MediaType; label: string; Icon: typeof AudioLines }[] = [
  { key: "audio", label: "Audio", Icon: AudioLines },
  { key: "image", label: "Images", Icon: ImageIcon },
  { key: "model", label: "3D Models", Icon: Box },
];

const LICENSES: LicenseStatus[] = ["permissive", "attribution", "restricted", "unknown"];

function SectionLabel({ children }: { children: React.ReactNode }) {
  return (
    <div className="px-3 pt-4 pb-1 text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
      {children}
    </div>
  );
}

function Row({
  active,
  onClick,
  children,
  right,
}: {
  active?: boolean;
  onClick?: () => void;
  children: React.ReactNode;
  right?: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      className="group flex w-full items-center gap-2 px-3 py-1 text-left text-xs transition-colors coarse:min-h-11"
      style={{
        background: active ? "var(--color-accent-muted)" : "transparent",
        color: active ? "var(--color-accent)" : "var(--color-fg-muted)",
      }}
    >
      <span className="flex min-w-0 flex-1 items-center gap-2">{children}</span>
      {right}
    </button>
  );
}

/** @param onNavigate called after a filter/source tap so the caller can dismiss the drawer on
 *  narrow screens (responsive + touch pass). Undefined for the persistent `lg` rail, which never closes. */
export function Navigation({ onNavigate }: { onNavigate?: () => void }) {
  const { state, patch } = useViewState();
  const go = (next: Parameters<typeof patch>[0]) => {
    patch(next);
    onNavigate?.();
  };
  const stats = useStats();
  const sources = useSources();
  const scan = useScan();
  const analyze = useAnalyze();
  const removeSource = useRemoveSource();
  const conn = useConnection();
  const [showAdd, setShowAdd] = useState(false);

  const total = stats.data?.total ?? 0;
  const byMedia = stats.data?.by_media ?? {};

  return (
    <nav className="flex h-full flex-col overflow-y-auto border-r border-border bg-surface">
      {/* library header */}
      <div className="flex items-center gap-2 border-b border-border px-3 py-2.5">
        <Library size={16} className="text-accent" />
        <div className="min-w-0">
          <div className="text-sm font-semibold text-fg">3DAM</div>
          <div className="text-[10px] text-fg-dim">{total.toLocaleString()} assets</div>
        </div>
        <button
          className="btn ml-auto px-1.5 py-1 coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
          title="Analyze library (embeddings, similarity, auto-tags)"
          aria-label="Analyze library"
          onClick={() => analyze.mutate({})}
          disabled={analyze.isPending}
        >
          <Sparkles size={13} className={analyze.isPending ? "animate-pulse" : ""} />
        </button>
        <button
          className="btn px-1.5 py-1 coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
          title="Rescan all sources"
          aria-label="Rescan all sources"
          onClick={() => scan.mutate({ mode: "delta" })}
          disabled={scan.isPending}
        >
          <RefreshCw size={13} className={scan.isPending ? "animate-spin" : ""} />
        </button>
      </div>

      {/* backend-unreachable banner — otherwise a dead server reads as an empty library (issue #24) */}
      {conn.backendDown && (
        <div className="flex items-center gap-2 border-b border-border bg-danger/10 px-3 py-2 text-[11px] text-danger">
          <WifiOff size={13} className="shrink-0" />
          <span>
            Can’t reach the server. Is <code className="font-mono">3dam serve</code> running?
          </span>
        </div>
      )}

      {/* library / media filters */}
      <SectionLabel>Library</SectionLabel>
      <Row
        active={!state.media && !state.collection}
        onClick={() => go({ media: null, collection: null })}
        right={<Count n={total} />}
      >
        <Layers size={14} /> All assets
      </Row>
      {MEDIA.map(({ key, label, Icon }) => (
        <Row
          key={key}
          active={state.media === key && !state.collection}
          onClick={() => go({ media: state.media === key ? null : key, collection: null })}
          right={<Count n={byMedia[key] ?? 0} />}
        >
          <Icon size={14} /> {label}
        </Row>
      ))}

      {/* license facet */}
      <SectionLabel>License</SectionLabel>
      {LICENSES.map((lic) => (
        <Row
          key={lic}
          active={state.license === lic && !state.collection}
          onClick={() => go({ license: state.license === lic ? null : lic, collection: null })}
        >
          <span
            className="h-2 w-2 shrink-0 rounded-full"
            style={{ background: licenseColorVar[lic] }}
          />
          {licenseLabel[lic]}
        </Row>
      ))}

      {/* sources */}
      <div className="flex items-center justify-between px-3 pt-4 pb-1">
        <span className="text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
          Sources
        </span>
        <button
          className="flex items-center justify-center text-fg-dim hover:text-accent coarse:min-h-11 coarse:min-w-11"
          title="Add source"
          onClick={() => setShowAdd(true)}
        >
          <FolderPlus size={14} />
        </button>
      </div>
      {/* Only when the fetch actually succeeded with zero sources — `data?.length === 0` was falsy
          while `data` is undefined (loading/error), leaving a blank, unexplained section (issue #24). */}
      {sources.isSuccess && sources.data.length === 0 && (
        <button
          className="mx-3 my-1 rounded border border-dashed border-border px-2 py-2 text-center text-[11px] text-fg-dim hover:border-accent hover:text-accent"
          onClick={() => setShowAdd(true)}
        >
          + Add a folder to scan
        </button>
      )}
      {sources.data?.map((s) => (
        <SourceRow
          key={s.id}
          source={s}
          active={state.source === s.id && !state.collection}
          removing={removeSource.isPending && removeSource.variables === s.id}
          onSelect={() => go({ source: state.source === s.id ? null : s.id, collection: null })}
          onRescan={() => scan.mutate({ sources: [s.id], mode: "full" })}
          onRemove={() => {
            if (confirm(`Remove source "${s.name}"? Its catalogued rows are dropped.`))
              removeSource.mutate(s.id);
          }}
        />
      ))}

      {/* collections & smart folders (phase 4) */}
      <Collections
        activeId={state.collection}
        onSelect={(id) =>
          go({ collection: id, media: null, source: null, license: null, q: "" })
        }
      />

      <div className="mt-auto" />
      {/* Duplicate / dedupe review (issue #8). */}
      <Link
        to="/duplicates"
        onClick={onNavigate}
        className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-dim hover:text-accent coarse:min-h-11"
      >
        <Copy size={14} /> Duplicate review
      </Link>
      {/* Admin / Settings surface (tech-spec 09 §B.4). */}
      <Link
        to="/settings"
        onClick={onNavigate}
        className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-dim hover:text-accent coarse:min-h-11"
      >
        <SettingsIcon size={14} /> Settings &amp; Administration
      </Link>
      {showAdd && <AddSourceDialog onClose={() => setShowAdd(false)} />}
    </nav>
  );
}

function Count({ n }: { n: number }) {
  return <span className="text-[10px] text-fg-dim tabular-nums">{n.toLocaleString()}</span>;
}

/** Collections & smart folders (issue #3). Lists them, filters the grid on click, and offers
 *  create / rename / delete. The web UI creates *manual* collections here; a smart folder carries a
 *  saved query (set via the CLI in v1) and is shown read-only with a Sparkles marker. Adding assets
 *  to a manual collection happens per-asset in the Inspector (batch add lands with multi-select). */
function Collections({
  activeId,
  onSelect,
}: {
  activeId: string | null;
  onSelect: (id: string | null) => void;
}) {
  const collections = useCollections();
  const create = useCreateCollection();
  const rename = useRenameCollection();
  const del = useDeleteCollection();
  const items = collections.data ?? [];

  const onCreate = () => {
    const name = prompt("New collection name")?.trim();
    if (name) create.mutate({ name, kind: "manual" });
  };

  return (
    <>
      <div className="flex items-center justify-between px-3 pt-4 pb-1">
        <span className="text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
          Collections
        </span>
        <button
          className="flex items-center justify-center text-fg-dim hover:text-accent coarse:min-h-11 coarse:min-w-11"
          title="New collection"
          aria-label="New collection"
          onClick={onCreate}
          disabled={create.isPending}
        >
          <FolderPlus size={14} />
        </button>
      </div>
      {collections.isSuccess && items.length === 0 && (
        <button
          className="mx-3 my-1 rounded border border-dashed border-border px-2 py-2 text-center text-[11px] text-fg-dim hover:border-accent hover:text-accent"
          onClick={onCreate}
        >
          + Group assets into a collection
        </button>
      )}
      {items.map((c) => (
        <CollectionRow
          key={c.id}
          collection={c}
          active={activeId === c.id}
          onSelect={() => onSelect(activeId === c.id ? null : c.id)}
          onRename={() => {
            const name = prompt("Rename collection", c.name)?.trim();
            if (name && name !== c.name) rename.mutate({ id: c.id, name });
          }}
          onDelete={() => {
            if (confirm(`Delete collection "${c.name}"? The assets themselves are untouched.`)) {
              if (activeId === c.id) onSelect(null);
              del.mutate(c.id);
            }
          }}
        />
      ))}
    </>
  );
}

function CollectionRow({
  collection,
  active,
  onSelect,
  onRename,
  onDelete,
}: {
  collection: Collection;
  active: boolean;
  onSelect: () => void;
  onRename: () => void;
  onDelete: () => void;
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
      <div className="hidden items-center gap-1 group-hover:flex coarse:flex">
        {/* Renaming a smart folder is fine; its query is edited via the CLI in v1. */}
        <button
          className="flex items-center justify-center text-fg-dim hover:text-accent coarse:min-h-11 coarse:min-w-11"
          title="Rename"
          aria-label={`Rename collection ${collection.name}`}
          onClick={onRename}
        >
          <Pencil size={12} />
        </button>
        <button
          className="flex items-center justify-center text-fg-dim hover:text-danger coarse:min-h-11 coarse:min-w-11"
          title="Delete"
          aria-label={`Delete collection ${collection.name}`}
          onClick={onDelete}
        >
          <Trash2 size={12} />
        </button>
      </div>
    </div>
  );
}

function SourceRow({
  source,
  active,
  removing,
  onSelect,
  onRescan,
  onRemove,
}: {
  source: SourceInfo;
  active: boolean;
  removing: boolean;
  onSelect: () => void;
  onRescan: () => void;
  onRemove: () => void;
}) {
  const scanning = source.state === "scanning";
  const errored = typeof source.state === "object";
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
        {/* State is also carried by shape (filled circle / hollow ring / square), not colour alone,
            and exposed to assistive tech — issue #28. */}
        <span
          role="img"
          aria-label={`Source status: ${sourceStateLabel(source.state)}`}
          title={sourceStateLabel(source.state)}
          className={`h-2 w-2 shrink-0 ${errored ? "rounded-[1px]" : "rounded-full"} ${
            scanning ? "animate-pulse border" : ""
          }`}
          style={{
            background: errored
              ? "var(--color-danger)"
              : scanning
                ? "transparent"
                : "var(--color-lic-permissive)",
            borderColor: scanning ? "var(--color-warn)" : undefined,
          }}
        />
        <span className="truncate" title={source.uri}>
          {source.name}
        </span>
        <span className="text-[10px] text-fg-dim tabular-nums">{source.stats.asset_count}</span>
      </button>
      {/* Hover-reveal under a mouse; always visible on touch, where there is no hover. */}
      <div className="hidden items-center gap-1 group-hover:flex coarse:flex">
        <button
          className="flex items-center justify-center text-fg-dim hover:text-accent coarse:min-h-11 coarse:min-w-11"
          title="Rescan"
          onClick={onRescan}
        >
          <RefreshCw size={12} className={scanning ? "animate-spin" : ""} />
        </button>
        <button
          className="flex items-center justify-center text-fg-dim hover:text-danger disabled:opacity-40 coarse:min-h-11 coarse:min-w-11"
          title="Remove"
          onClick={onRemove}
          disabled={removing}
        >
          <Trash2 size={12} className={removing ? "animate-pulse" : ""} />
        </button>
      </div>
    </div>
  );
}
