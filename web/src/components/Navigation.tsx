import type React from "react";
import { useState } from "react";
import {
  AudioLines,
  Box,
  FolderPlus,
  Image as ImageIcon,
  Layers,
  Library,
  RefreshCw,
  Tag as TagIcon,
  Trash2,
} from "lucide-react";
import { useScan, useSources, useStats, useRemoveSource } from "@/api/queries";
import type { LicenseStatus, MediaType, SourceInfo } from "@/api/types";
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
  const removeSource = useRemoveSource();
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
          title="Rescan all sources"
          onClick={() => scan.mutate({ mode: "delta" })}
          disabled={scan.isPending}
        >
          <RefreshCw size={13} className={scan.isPending ? "animate-spin" : ""} />
        </button>
      </div>

      {/* library / media filters */}
      <SectionLabel>Library</SectionLabel>
      <Row active={!state.media} onClick={() => go({ media: null })} right={<Count n={total} />}>
        <Layers size={14} /> All assets
      </Row>
      {MEDIA.map(({ key, label, Icon }) => (
        <Row
          key={key}
          active={state.media === key}
          onClick={() => go({ media: state.media === key ? null : key })}
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
          active={state.license === lic}
          onClick={() => go({ license: state.license === lic ? null : lic })}
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
      {sources.data?.length === 0 && (
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
          active={state.source === s.id}
          onSelect={() => go({ source: state.source === s.id ? null : s.id })}
          onRescan={() => scan.mutate({ sources: [s.id], mode: "full" })}
          onRemove={() => {
            if (confirm(`Remove source "${s.name}"? Its catalogued rows are dropped.`))
              removeSource.mutate(s.id);
          }}
        />
      ))}

      {/* not-yet surfaces — honest about the roadmap, not fake chrome */}
      <SectionLabel>Collections</SectionLabel>
      <div className="flex items-center gap-2 px-3 py-1 text-xs text-fg-dim">
        <TagIcon size={13} /> <span className="italic">Tags & collections — soon</span>
      </div>

      <div className="mt-auto" />
      {showAdd && <AddSourceDialog onClose={() => setShowAdd(false)} />}
    </nav>
  );
}

function Count({ n }: { n: number }) {
  return <span className="text-[10px] text-fg-dim tabular-nums">{n.toLocaleString()}</span>;
}

function SourceRow({
  source,
  active,
  onSelect,
  onRescan,
  onRemove,
}: {
  source: SourceInfo;
  active: boolean;
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
        <span
          className="h-1.5 w-1.5 shrink-0 rounded-full"
          style={{
            background: errored
              ? "var(--color-danger)"
              : scanning
                ? "var(--color-warn)"
                : "var(--color-lic-permissive)",
          }}
          title={sourceStateLabel(source.state)}
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
          className="flex items-center justify-center text-fg-dim hover:text-danger coarse:min-h-11 coarse:min-w-11"
          title="Remove"
          onClick={onRemove}
        >
          <Trash2 size={12} />
        </button>
      </div>
    </div>
  );
}
