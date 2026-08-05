import type React from "react";
import { useState } from "react";
import { Link } from "react-router";
import {
  AudioLines,
  Ban,
  Box,
  Clock,
  Copy,
  FileText,
  Film,
  FolderPlus,
  Image as ImageIcon,
  Layers,
  Library,
  Monitor,
  Moon,
  RefreshCw,
  Settings as SettingsIcon,
  Sparkles,
  Star,
  Sun,
  Upload as UploadIcon,
  UserRound,
  WifiOff,
} from "lucide-react";
import {
  useAnalyze,
  useRemoveSource,
  useScan,
  useSources,
  useStats,
  useVersion,
  useWhoami,
} from "@/api/queries";
import { useCan } from "@/api/queries";
import type { ShareResource } from "@/api/admin";
import { useConnection } from "@/api/connection";
import { useWriteGate } from "@/lib/write-gate";
import type { FacetField, Filter, LicenseStatus, MediaType } from "@/api/types";
import { licenseColorVar, licenseLabel } from "@/lib/format";
import { useViewState } from "@/lib/view-state";
import { useDialogs } from "@/lib/dialogs";
import { useTheme, type ThemePref } from "@/lib/theme";
import { ACCOUNT_ROUTES, accountNavigation } from "@/lib/account-surfaces";
import { AddSourceDialog } from "./AddSourceDialog";
import { Collections } from "./navigation/Collections";
import { SourceRow } from "./navigation/SourceRow";
import { ShareDialog } from "./ShareDialog";

const MEDIA: { key: MediaType; label: string; Icon: typeof AudioLines }[] = [
  { key: "audio", label: "Audio", Icon: AudioLines },
  { key: "image", label: "Images", Icon: ImageIcon },
  { key: "model", label: "3D Models", Icon: Box },
  { key: "video", label: "Video", Icon: Film },
  { key: "document", label: "Documents", Icon: FileText },
];

const LICENSES: LicenseStatus[] = ["permissive", "attribution", "restricted", "unknown"];

function SectionLabel({ children }: { children: React.ReactNode }) {
  return (
    <div className="px-3 pt-4 pb-1 text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
      {children}
    </div>
  );
}

/** The one high-value structured facet worth a sidebar chip row, contextual to the active media
 *  type: the analysis-pass *class* (audio type, image type, model complexity). The full set of typed
 *  attribute filters lives in Advanced Search; this is the quick pick. Reads/writes the same `adv`
 *  Filter[] so the two surfaces stay in sync. Hidden unless a single media type is selected. */
const CLASS_FACET: Record<MediaType, { field: FacetField; label: string; options: [string, string][] }> = {
  audio: {
    field: "audio_class",
    label: "Type",
    options: [
      ["one_shot", "One-shot"],
      ["loop", "Loop"],
      ["music", "Music"],
      ["sfx", "SFX"],
    ],
  },
  image: {
    field: "image_class",
    label: "Type",
    options: [
      ["texture", "Texture"],
      ["sprite", "Sprite"],
      ["photo", "Photo"],
    ],
  },
  model: {
    field: "model_class",
    label: "Complexity",
    options: [
      ["prop_lowpoly", "Low-poly"],
      ["prop", "Prop"],
      ["prop_highpoly", "High-poly"],
    ],
  },
  video: {
    field: "video_class",
    label: "Length",
    options: [
      ["sting", "Sting"],
      ["clip", "Clip"],
      ["cutscene", "Cutscene"],
    ],
  },
  document: {
    field: "document_class",
    label: "Kind",
    options: [
      ["license", "Licence"],
      ["readme", "Readme"],
      ["changelog", "Changelog"],
      ["receipt", "Receipt"],
      ["document", "Other"],
    ],
  },
};

function QuickFacet({
  media,
  adv,
  onChange,
}: {
  media: MediaType;
  adv: Filter[];
  onChange: (next: Filter[]) => void;
}) {
  const { field, label, options } = CLASS_FACET[media];
  const cur = adv.find((f) => f.field === field);
  const active = cur && "str" in cur.value ? cur.value.str : null;
  const select = (v: string) => {
    const rest = adv.filter((f) => f.field !== field);
    onChange(active === v ? rest : [...rest, { field, op: "eq", value: { str: v } }]);
  };
  return (
    <>
      <SectionLabel>{label}</SectionLabel>
      <div className="flex flex-wrap gap-1.5 px-3 py-1">
        {options.map(([v, l]) => {
          const on = active === v;
          return (
            <button
              key={v}
              onClick={() => select(v)}
              aria-pressed={on}
              className="rounded-md border px-2 py-0.5 text-xs transition-colors coarse:min-h-11"
              style={{
                background: on ? "var(--color-accent-muted)" : "var(--color-surface-2)",
                color: on ? "var(--color-accent)" : "var(--color-fg-muted)",
                borderColor: on ? "var(--color-accent)" : "var(--color-border)",
              }}
            >
              {l}
            </button>
          );
        })}
      </div>
    </>
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
  const sources = useSources();
  // Sidebar counts follow the browsed source (phase 6): a scoped local source counts its own
  // assets; a federated source reports the peer's live numbers (polled — no WS across peers).
  const isPeer =
    sources.data?.some((s) => s.id === state.source && s.kind === "federated") ?? false;
  const stats = useStats(state.source, { isPeer });
  const scan = useScan();
  const analyze = useAnalyze();
  const removeSource = useRemoveSource();
  const conn = useConnection();
  const { confirm } = useDialogs();
  const { gate } = useWriteGate();
  const canWrite = useCan("write");
  const version = useVersion();
  const whoami = useWhoami();
  // Unlike write controls, route discovery should not be optimistic: a viewer must never see an
  // Administration destination while /whoami is still settling. Auth-off/legacy servers are the
  // exception because their version posture itself grants local-owner trust.
  const canAdmin = whoami.data
    ? whoami.data.scopes.includes("admin")
    : version.isSuccess && (version.data.auth ?? "off") === "off";
  const [showAdd, setShowAdd] = useState(false);
  // Sharing (user accounts, issue #42): the Share… affordance is admin-only and needs the
  // `user_accounts` flag on — /api/version already reports it, so no extra query.
  const accountsOn = version.data?.accounts === true;
  const canShare = canAdmin && accountsOn;
  const accountNav = accountNavigation(whoami.data?.account != null, canAdmin);
  // Uploads (issue #80) need the `upload` flag *and* write scope: the flag says this deployment
  // accepts writes into a source at all, the scope says this caller may make them. Same source of
  // truth as `accounts` — /api/version already reports it, so no extra query.
  const uploadOn = version.data?.upload === true;
  const [share, setShare] = useState<{
    resource: ShareResource;
    id: string;
    name: string;
    readOnly?: boolean;
  } | null>(null);

  const total = stats.data?.total ?? 0;
  const byMedia = stats.data?.by_media ?? {};
  // The "Recently added" shortcut is a sort overlay (newest-scanned first), not a filter — so it reads
  // as active whenever that ordering is in effect, regardless of the media/source facet layered on it.
  const recentlyAdded = state.sort === "scanned" && state.dir === "desc";

  return (
    <nav
      className="flex h-full flex-col overflow-y-auto border-r border-border bg-surface"
      aria-label="Library navigation"
      data-shortcut-region="navigation"
      tabIndex={-1}
    >
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
          className="btn ml-auto px-1.5 py-1 disabled:opacity-40 coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
          aria-label="Analyze library"
          onClick={() => analyze.mutate({})}
          {...gate({
            disabled: analyze.isPending,
            title: "Analyze library (embeddings, similarity, auto-tags)",
          })}
        >
          <Sparkles size={13} className={analyze.isPending ? "animate-pulse" : ""} />
        </button>
        <button
          className="btn px-1.5 py-1 disabled:opacity-40 coarse:min-h-11 coarse:min-w-11 coarse:justify-center"
          aria-label="Quick rescan all sources (changed files only)"
          onClick={() => scan.mutate({ mode: "delta" })}
          {...gate({
            disabled: scan.isPending,
            title: "Quick rescan — changed files only (all sources)",
          })}
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
        <Layers size={14} /> All media types
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

      {/* Tags no longer live in the sidebar: auto-tags are open-vocabulary and audio-heavy, so they
          cluttered the rail without earning their place. They now power search/filtering instead —
          a tag control lands in Advanced Search. Structured, bounded attributes (media class, BPM,
          key, dimensions…) get their own typed facets there rather than masquerading as tags.
          The single highest-value one — the analysis class — gets a quick chip row here when a media
          type is active; the full set lives in the Browser's Advanced Search panel. */}
      {state.media && !state.collection && (
        <QuickFacet
          media={state.media}
          adv={state.adv}
          onChange={(next) => go({ adv: next, collection: null })}
        />
      )}

      {/* sources */}
      <div className="flex items-center justify-between px-3 pt-4 pb-1">
        <span className="text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
          Sources
        </span>
        <button
          className="flex items-center justify-center text-fg-dim hover:text-accent disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:text-fg-dim coarse:min-h-11 coarse:min-w-11"
          aria-label="Add source"
          onClick={() => setShowAdd(true)}
          {...gate({ title: "Add source" })}
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
          gate={gate}
          active={state.source === s.id && !state.path && !state.collection}
          removing={removeSource.isPending && removeSource.variables === s.id}
          onNavigate={onNavigate}
          onSelect={() =>
            // Selecting the source row means the whole source — drop any folder scope (issue #66).
            go({ source: state.source === s.id ? null : s.id, path: null, collection: null })
          }
          onShare={
            canShare
              ? () =>
                  setShare({
                    resource: "source",
                    id: s.id,
                    name: s.name,
                    readOnly: s.kind === "federated",
                  })
              : undefined
          }
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
                  // Don't leave the browser filtered on a source that no longer exists (issue #29),
                  // and drop any folder scope under it (issue #66).
                  // The source no longer exists, so repair the current URL instead of leaving an
                  // invalid history entry that Back could revisit and clean up repeatedly.
                  if (state.source === s.id)
                    patch({ source: null, path: null }, { replace: true });
                },
              });
            }
          }}
        />
      ))}

      {/* collections & smart folders (phase 4) */}
      <Collections
        gate={gate}
        onShare={
          canShare
            ? (c) => setShare({ resource: "collection", id: c.id, name: c.name })
            : undefined
        }
        activeId={state.collection}
        onSelect={(id) =>
          go({
            collection: id,
            media: null,
            source: null,
            path: null,
            license: null,
            tag: null,
            adv: [],
            q: "",
          })
        }
      />

      <div className="mt-auto" />
      {/* Light/dark theme (issue #62) — cycles system → light → dark, persisted. */}
      <ThemeToggle />
      {/* Upload (issue #80). Absent, not disabled, without the `upload` flag or `write` scope: the
          whole surface is a write action, so a greyed-out entry would only advertise something
          they can never do — unlike a per-asset control, where the row still has a read purpose.
          With the flag off the route is genuinely absent server-side, so there is nothing to
          advertise at all. */}
      {uploadOn && canWrite && (
        <Link
          to="/upload"
          onClick={onNavigate}
          className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-dim hover:text-accent coarse:min-h-11"
        >
          <UploadIcon size={14} /> Upload assets
        </Link>
      )}
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
      {/* Personal account self-service is present only for an actual signed-in account. An API
          token has no profile or session list of its own. */}
      {accountNav.profile && (
        <Link
          to={ACCOUNT_ROUTES.profile}
          onClick={onNavigate}
          className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-dim hover:text-accent coarse:min-h-11"
        >
          <UserRound size={14} /> Profile
        </Link>
      )}
      {/* Administration exists in navigation only when the current capability set includes admin.
          Non-admins are not shown an ordinary-looking destination that can only reject them. */}
      {accountNav.administration && (
        <Link
          to={ACCOUNT_ROUTES.administration}
          onClick={onNavigate}
          className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-dim hover:text-accent coarse:min-h-11"
        >
          <SettingsIcon size={14} /> Administration
        </Link>
      )}
      {showAdd && <AddSourceDialog onClose={() => setShowAdd(false)} />}
      {share && (
        <ShareDialog
          resource={share.resource}
          resourceId={share.id}
          resourceName={share.name}
          readOnly={share.readOnly}
          onClose={() => setShare(null)}
        />
      )}
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
