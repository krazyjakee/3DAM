// Duplicate / dedupe review surface (issue #8; tech-spec 05 §4). A read-only review of the groups
// the analysis pass linked — exact (byte-identical content hash) or near (pHash / embedding). 3DAM
// only *groups*; it never auto-deletes. Each group suggests a "keep"; disposing of the rest
// (remove + block from re-scan, issue #21) is a separate, deliberate step — right-click / long-press
// any member for the same per-item context menu as the Browser tiles/rows (analyze, convert, add to
// collection, copy path, remove / remove + block). Blocked hashes are managed on the /blocklist surface.

import { useCallback, useState } from "react";
import { Link } from "react-router";
import { Copy } from "lucide-react";
import { useDuplicates } from "@/api/queries";
import type { AssetSummary, DupGroup, DupKind, MediaType } from "@/api/types";
import { bytes, mediaLabel } from "@/lib/format";
import { shortcutForEvent } from "@/lib/shortcuts";
import { CenteredCard } from "@/lib/ui";
import { Thumbnail } from "./Thumbnail";
import { LicenseBadge } from "./LicenseBadge";
import { ContextMenu, useLongPress, type MenuState } from "./ContextMenu";
import { ConvertDialog } from "./ConvertDialog";

const KINDS: { key: DupKind; label: string; hint: string }[] = [
  { key: "exact", label: "Exact", hint: "Byte-identical (content hash)" },
  { key: "near", label: "Near", hint: "Perceptually close (pHash / embedding)" },
];
const MEDIA: { key: MediaType | ""; label: string }[] = [
  { key: "", label: "All media" },
  { key: "image", label: "Images" },
  { key: "audio", label: "Audio" },
  { key: "model", label: "3D Models" },
];

export function Duplicates() {
  const [kind, setKind] = useState<DupKind>("exact");
  const [media, setMedia] = useState<MediaType | "">("");
  const groups = useDuplicates({ kind, media: media || undefined });
  const data = groups.data ?? [];

  // Same per-item context menu as the Browser tiles/rows (issue #20). The Duplicates page has no
  // multi-selection, so the target is always the single right-clicked / long-pressed member.
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [convertTargets, setConvertTargets] = useState<AssetSummary[] | null>(null);
  const openMenu = useCallback(
    (asset: AssetSummary, x: number, y: number) => setMenu({ assets: [asset], x, y }),
    [],
  );

  return (
    <div className="mx-auto flex min-h-dvh max-w-4xl flex-col gap-5 p-6 text-sm">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2">
          <Copy size={18} className="text-accent" />
          <h1 className="text-lg font-semibold text-fg">Duplicate review</h1>
        </div>
        <Link to="/" className="text-accent hover:underline">
          ← Back to library
        </Link>
      </header>

      <p className="text-xs text-fg-dim">
        Groups the analysis pass linked. 3DAM only groups — nothing is deleted. Each group marks a
        suggested <span className="text-accent">Keep</span>; open a member to inspect it, or
        right-click for actions.
      </p>

      {/* controls: tier + media filter */}
      <div className="flex flex-wrap items-center gap-2">
        <div className="flex overflow-hidden rounded border border-border">
          {KINDS.map((k) => (
            <button
              key={k.key}
              onClick={() => setKind(k.key)}
              title={k.hint}
              className="px-3 py-1 text-xs coarse:min-h-11"
              style={{
                background: kind === k.key ? "var(--color-accent)" : "var(--color-surface-2)",
                color: kind === k.key ? "var(--color-accent-fg)" : "var(--color-fg-muted)",
              }}
            >
              {k.label}
            </button>
          ))}
        </div>
        <select
          className="field w-auto"
          aria-label="Filter by media type"
          value={media}
          onChange={(e) => setMedia(e.target.value as MediaType | "")}
        >
          {MEDIA.map((m) => (
            <option key={m.key} value={m.key}>
              {m.label}
            </option>
          ))}
        </select>
        <span className="text-xs text-fg-dim tabular-nums">
          {data.length} group{data.length === 1 ? "" : "s"}
        </span>
      </div>

      {groups.isLoading ? (
        <CenteredCard>Scanning for duplicates…</CenteredCard>
      ) : groups.isError ? (
        <CenteredCard tone="danger">Failed to load — is `3dam serve` running?</CenteredCard>
      ) : data.length === 0 ? (
        <CenteredCard>
          No {kind} duplicates{media ? ` among ${mediaLabel[media]}` : ""}. Run the analysis pass to
          populate near-duplicate signals.
        </CenteredCard>
      ) : (
        <div className="flex flex-col gap-4">
          {data.map((g, i) => (
            <GroupCard key={i} group={g} onContext={openMenu} />
          ))}
        </div>
      )}

      <ContextMenu
        menu={menu}
        onClose={() => setMenu(null)}
        onConvert={(assets) => setConvertTargets(assets)}
      />
      {convertTargets && (
        <ConvertDialog assets={convertTargets} onClose={() => setConvertTargets(null)} />
      )}
    </div>
  );
}

function GroupCard({
  group,
  onContext,
}: {
  group: DupGroup;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
}) {
  return (
    <section className="rounded border border-border bg-surface p-3">
      <div className="mb-2 flex items-center justify-between gap-2">
        <span className="text-[11px] text-fg-dim">
          {mediaLabel[group.media]} · {group.members.length} items
        </span>
        {/* the pairwise signal is the explanation (DESIGN_GUIDELINES §1.2) */}
        <span className="rounded bg-surface-2 px-1.5 py-0.5 font-mono text-[10px] text-fg-muted">
          {group.signal}
        </span>
      </div>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        {group.members.map((m) => (
          <MemberTile
            key={m.id}
            asset={m}
            keep={m.id === group.suggested_keep}
            onContext={onContext}
          />
        ))}
      </div>
    </section>
  );
}

function MemberTile({
  asset,
  keep,
  onContext,
}: {
  asset: AssetSummary;
  keep: boolean;
  onContext: (asset: AssetSummary, x: number, y: number) => void;
}) {
  const longPress = useLongPress((x, y) => onContext(asset, x, y));
  return (
    <Link
      to={`/?sel=${asset.id}`}
      data-asset-id={asset.id}
      aria-keyshortcuts="Shift+F10"
      onKeyDown={(event) => {
        if (shortcutForEvent(event.nativeEvent) !== "action-menu") return;
        event.preventDefault();
        const rect = event.currentTarget.getBoundingClientRect();
        onContext(asset, rect.left + Math.min(24, rect.width / 2), rect.top + 24);
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        onContext(asset, e.clientX, e.clientY);
      }}
      {...longPress}
      className="group flex flex-col overflow-hidden rounded border text-left transition-colors"
      style={{
        borderColor: keep ? "var(--color-accent)" : "var(--color-border)",
      }}
      title={`${asset.name} · ${bytes(asset.size)}`}
    >
      <div className="relative aspect-square">
        <Thumbnail asset={asset} size={48} />
        {keep && (
          <span className="absolute top-1 left-1 rounded bg-accent px-1 py-0.5 text-[9px] font-semibold text-accent-fg">
            Keep
          </span>
        )}
      </div>
      <div className="flex items-center justify-between gap-1 border-t border-border px-1.5 py-1">
        <span className="truncate text-[11px] text-fg" title={asset.name}>
          {asset.name}
        </span>
      </div>
      <div className="flex items-center justify-between px-1.5 pb-1">
        <LicenseBadge badge={asset.license} />
        <span className="text-[10px] text-fg-dim tabular-nums">{bytes(asset.size)}</span>
      </div>
    </Link>
  );
}
