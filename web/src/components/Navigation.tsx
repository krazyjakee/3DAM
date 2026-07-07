import type React from "react";
import { useState } from "react";
import { Link } from "react-router-dom";
import {
  AudioLines,
  Ban,
  Box,
  Clock,
  Copy,
  Folder,
  FolderPlus,
  Image as ImageIcon,
  Layers,
  Library,
  Monitor,
  Moon,
  Pencil,
  RefreshCw,
  Settings as SettingsIcon,
  Sparkles,
  Star,
  Sun,
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
import { useDialogs } from "@/lib/dialogs";
import { useTheme, type ThemePref } from "@/lib/theme";
import { AddSourceDialog } from "./AddSourceDialog";

const MEDIA: { key: MediaType; label: string; Icon: typeof AudioLines }[] = [
  { key: "audio", label: "Audio", Icon: AudioLines },
  { key: "image", label: "Images", Icon: ImageIcon },
  { key: "model", label: "3D Models", Icon: Box },
];

const LICENSES: LicenseStatus[] = ["permissive", "attribution", "restricted", "unknown"];

/** Tags filter facet (design: the sidebar "Tags" chip row). Renders the most-used confirmed tags
 *  from stats as clickable chips; the active tag is highlighted. Hidden entirely until the library
 *  has confirmed tags, so a fresh/unanalysed catalog shows no empty header. */
function TagFacet({
  tags,
  active,
  onSelect,
}: {
  tags: Record<string, number>;
  active: string | null;
  onSelect: (tag: string) => void;
}) {
  const names = Object.keys(tags).sort();
  if (names.length === 0) return null;
  return (
    <>
      <SectionLabel>Tags</SectionLabel>
      <div className="flex flex-wrap gap-1.5 px-3 py-1">
        {names.map((name) => {
          const on = active === name;
          return (
            <button
              key={name}
              onClick={() => onSelect(name)}
              title={`${tags[name].toLocaleString()} asset${tags[name] === 1 ? "" : "s"}`}
              aria-pressed={on}
              className="rounded-md border px-2 py-0.5 text-xs transition-colors coarse:min-h-11"
              style={{
                background: on ? "var(--color-accent-muted)" : "var(--color-surface-2)",
                color: on ? "var(--color-accent)" : "var(--color-fg-muted)",
                borderColor: on ? "var(--color-accent)" : "var(--color-border)",
              }}
            >
              {name}
            </button>
          );
        })}
      </div>
    </>
  );
}

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
  const { confirm } = useDialogs();
  const [showAdd, setShowAdd] = useState(false);

  const total = stats.data?.total ?? 0;
  const byMedia = stats.data?.by_media ?? {};
  // The "Recently added" shortcut is a sort overlay (newest-scanned first), not a filter — so it reads
  // as active whenever that ordering is in effect, regardless of the media/source facet layered on it.
  const recentlyAdded = state.sort === "scanned" && state.dir === "desc";

  return (
    <nav className="flex h-full flex-col overflow-y-auto border-r border-border bg-surface">
      {/* library header */}
      <div className="flex items-center gap-2 border-b border-border px-3 py-2.5">
        <Library size={16} className="text-accent" />
        <div className="min-w-0">
          <div className="text-sm font-semibold text-fg">3DAM</div>
          <div className="text-[10px] text-fg-dim">
            {stats.isLoading ? "…" : total.toLocaleString()} assets
          </div>
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
          title="Quick rescan — changed files only (all sources)"
          aria-label="Quick rescan all sources (changed files only)"
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
        right={<Count n={total} loading={stats.isLoading} />}
      >
        <Layers size={14} /> All assets
      </Row>
      {/* Favorites (issue #63): a boolean facet — star an asset from the Inspector — that composes
          with the media/source facets. Toggling it on clears any active collection view. */}
      <Row active={state.fav} onClick={() => go({ fav: !state.fav, collection: null })}>
        <Star size={14} className={state.fav ? "fill-current" : ""} /> Favorites
      </Row>
      {/* Recently added (issue #63): a saved-sort shortcut — newest-scanned first — that composes with
          the media/source facets (so "recently added images" works). No schema change; `Scanned` sort
          already exists. Clicking it again reverts to the default name/ascending order. */}
      <Row
        active={recentlyAdded}
        onClick={() =>
          go(
            recentlyAdded
              ? { sort: "name", dir: "asc" }
              : { sort: "scanned", dir: "desc", collection: null },
          )
        }
      >
        <Clock size={14} /> Recently added
      </Row>
      {MEDIA.map(({ key, label, Icon }) => (
        <Row
          key={key}
          active={state.media === key && !state.collection}
          onClick={() => go({ media: state.media === key ? null : key, collection: null })}
          right={<Count n={byMedia[key] ?? 0} loading={stats.isLoading} />}
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

      {/* tags facet — the most-used confirmed tags, from stats. Composes with the media/license/
          source facets; clicking one toggles it (and exits any collection view). */}
      <TagFacet
        tags={stats.data?.tags ?? {}}
        active={state.tag}
        onSelect={(t) => go({ tag: state.tag === t ? null : t, collection: null })}
      />

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
          + Add a source to scan
        </button>
      )}
      {sources.data?.map((s) => (
        <SourceRow
          key={s.id}
          source={s}
          active={state.source === s.id && !state.collection}
          removing={removeSource.isPending && removeSource.variables === s.id}
          onSelect={() => go({ source: state.source === s.id ? null : s.id, collection: null })}
          onRescan={() => scan.mutate({ sources: [s.id], mode: "delta" })}
          onRemove={async () => {
            const n = s.stats.asset_count;
            if (
              await confirm({
                title: `Remove source “${s.name}”?`,
                message: `Its ${n.toLocaleString()} catalogued row${n === 1 ? "" : "s"} will be dropped from the library. The files on disk are not touched.`,
                danger: true,
                confirmLabel: "Remove source",
              })
            ) {
              removeSource.mutate(s.id, {
                onSuccess: () => {
                  // Don't leave the browser filtered on a source that no longer exists (issue #29).
                  if (state.source === s.id) patch({ source: null });
                },
              });
            }
          }}
        />
      ))}

      {/* collections & smart folders (phase 4) */}
      <Collections
        activeId={state.collection}
        onSelect={(id) =>
          go({ collection: id, media: null, source: null, license: null, tag: null, q: "" })
        }
      />

      <div className="mt-auto" />
      {/* Light/dark theme (issue #62) — cycles system → light → dark, persisted. */}
      <ThemeToggle />
      {/* Duplicate / dedupe review (issue #8). */}
      <Link
        to="/duplicates"
        onClick={onNavigate}
        className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-dim hover:text-accent coarse:min-h-11"
      >
        <Copy size={14} /> Duplicate review
      </Link>
      {/* Rescan blocklist management (issue #21). */}
      <Link
        to="/blocklist"
        onClick={onNavigate}
        className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-dim hover:text-accent coarse:min-h-11"
      >
        <Ban size={14} /> Rescan blocklist
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

/** Theme switch (issue #62): a single footer row that cycles the preference System → Light → Dark,
 *  persisted and applied by the ThemeProvider. The icon/label reflect the current preference; the
 *  title hints what a click does next. */
const THEME_ORDER: ThemePref[] = ["system", "light", "dark"];
const THEME_META: Record<ThemePref, { Icon: typeof Monitor; label: string }> = {
  system: { Icon: Monitor, label: "System theme" },
  light: { Icon: Sun, label: "Light theme" },
  dark: { Icon: Moon, label: "Dark theme" },
};

function ThemeToggle() {
  const { pref, setPref } = useTheme();
  const { Icon, label } = THEME_META[pref];
  const next = THEME_ORDER[(THEME_ORDER.indexOf(pref) + 1) % THEME_ORDER.length];
  return (
    <button
      onClick={() => setPref(next)}
      className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-dim hover:text-accent coarse:min-h-11"
      title={`${label} — switch to ${THEME_META[next].label.toLowerCase()}`}
    >
      <Icon size={14} /> {label}
    </button>
  );
}

function Count({ n, loading = false }: { n: number; loading?: boolean }) {
  // While stats load, show a muted placeholder rather than a misleading "0" (issue #30).
  return (
    <span className="text-[10px] text-fg-dim tabular-nums">
      {loading ? "·" : n.toLocaleString()}
    </span>
  );
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
  const { confirm, prompt } = useDialogs();
  const items = collections.data ?? [];

  const onCreate = async () => {
    const name = (await prompt({ title: "New collection", placeholder: "Name", confirmLabel: "Create" }))?.trim();
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
          onRename={async () => {
            const name = (
              await prompt({ title: "Rename collection", initial: c.name, confirmLabel: "Rename" })
            )?.trim();
            if (name && name !== c.name) rename.mutate({ id: c.id, name });
          }}
          onDelete={async () => {
            if (
              await confirm({
                title: `Delete collection “${c.name}”?`,
                message: "The assets themselves are untouched.",
                danger: true,
                confirmLabel: "Delete",
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
          title="Quick rescan — changed files only (full re-scan lives in Settings)"
          aria-label="Quick rescan this source (changed files only)"
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
