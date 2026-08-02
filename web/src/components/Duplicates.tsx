// Duplicate review (issues #8/#111): visible, keyboard/touch-operable decisions over durable
// review queues. Catalog removal is explicit and confirmed; source files are never deleted.

import { useEffect, useState } from "react";
import { Link } from "react-router";
import { Ban, Check, CheckCircle2, Copy, RotateCcw, Trash2, XCircle } from "lucide-react";
import { api } from "@/api/client";
import { useCan, useDuplicates, useReviewDuplicate } from "@/api/queries";
import type {
  DupGroup,
  DupKind,
  DupMember,
  DupReviewFilter,
  DupReviewRequest,
  MediaType,
} from "@/api/types";
import { bytes, mediaLabel } from "@/lib/format";
import { useDialogs } from "@/lib/dialogs";
import { CenteredCard } from "@/lib/ui";
import { Thumbnail } from "./Thumbnail";
import { LicenseBadge } from "./LicenseBadge";

const KINDS: { key: DupKind; label: string; hint: string }[] = [
  { key: "exact", label: "Exact", hint: "Byte-identical (content hash)" },
  { key: "near", label: "Near", hint: "Only comparable assets in the same embedding space" },
];
const MEDIA: { key: MediaType | ""; label: string }[] = [
  { key: "", label: "All media" },
  { key: "image", label: "Images" },
  { key: "audio", label: "Audio" },
  { key: "model", label: "3D Models" },
  { key: "video", label: "Videos" },
  { key: "document", label: "Documents" },
];
const REVIEW: { key: DupReviewFilter; label: string }[] = [
  { key: "pending", label: "Pending review" },
  { key: "resolved", label: "Resolved" },
  { key: "dismissed", label: "Dismissed" },
  { key: "all", label: "All states" },
];

export function Duplicates() {
  const [kind, setKind] = useState<DupKind>("exact");
  const [media, setMedia] = useState<MediaType | "">("");
  const [reviewFilter, setReviewFilter] = useState<DupReviewFilter>("pending");
  const groups = useDuplicates({ kind, media: media || undefined, review: reviewFilter });
  const review = useReviewDuplicate();
  const canWrite = useCan("write");
  const data = groups.data?.pages.flatMap((page) => page.items) ?? [];
  const partialWarnings = [
    ...new Set(
      groups.data?.pages.flatMap((page) =>
        page.partial.complete ? [] : (page.partial.warnings ?? []).map((warning) => warning.message),
      ) ?? [],
    ),
  ];

  return (
    <div className="mx-auto flex min-h-dvh max-w-6xl flex-col gap-5 p-4 text-sm sm:p-6">
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
        Compare metadata, choose the copy to keep, then resolve or dismiss the group. Remove actions
        change only 3DAM’s catalog—source files stay on disk. Decisions persist across navigation and
        refresh.
      </p>

      <div className="flex flex-wrap items-center gap-2">
        <div className="flex overflow-hidden rounded border border-border">
          {KINDS.map((item) => (
            <button
              key={item.key}
              onClick={() => setKind(item.key)}
              title={item.hint}
              className="px-3 py-1 text-xs coarse:min-h-11"
              style={{
                background: kind === item.key ? "var(--color-accent)" : "var(--color-surface-2)",
                color: kind === item.key ? "var(--color-accent-fg)" : "var(--color-fg-muted)",
              }}
            >
              {item.label}
            </button>
          ))}
        </div>
        <select
          className="field w-auto"
          aria-label="Filter by media type"
          value={media}
          onChange={(event) => setMedia(event.target.value as MediaType | "")}
        >
          {MEDIA.map((item) => (
            <option key={item.key} value={item.key}>
              {item.label}
            </option>
          ))}
        </select>
        <select
          className="field w-auto"
          aria-label="Filter by review state"
          value={reviewFilter}
          onChange={(event) => setReviewFilter(event.target.value as DupReviewFilter)}
        >
          {REVIEW.map((item) => (
            <option key={item.key} value={item.key}>
              {item.label}
            </option>
          ))}
        </select>
        <span className="text-xs text-fg-dim tabular-nums">
          {data.length} group{data.length === 1 ? "" : "s"}
        </span>
      </div>

      {kind === "near" && (
        <p className="rounded border border-border bg-surface-2 px-3 py-2 text-[11px] text-fg-dim">
          Near review compares only assets with a valid signal in the same media embedding space.
          Media without an analysis signal are omitted rather than cross-ranked.
        </p>
      )}
      {!canWrite && (
        <CenteredCard>Review decisions require write access. Comparison remains read-only.</CenteredCard>
      )}
      {partialWarnings.map((warning) => (
        <CenteredCard key={warning}>{warning}</CenteredCard>
      ))}

      {groups.isLoading ? (
        <CenteredCard>Scanning for duplicates…</CenteredCard>
      ) : groups.isError ? (
        <CenteredCard tone="danger">Failed to load — is `3dam serve` running?</CenteredCard>
      ) : data.length === 0 ? (
        <CenteredCard>
          No {kind} duplicates{media ? ` among ${mediaLabel[media]}` : ""} in this review state.
        </CenteredCard>
      ) : (
        <div className="flex flex-col gap-4">
          {data.map((group) => (
            <GroupCard
              key={group.review}
              group={group}
              canWrite={canWrite}
              busy={review.isPending}
              decide={(request) => review.mutateAsync(request)}
            />
          ))}
          {groups.hasNextPage && (
            <button
              className="btn self-center"
              disabled={groups.isFetchingNextPage}
              onClick={() => void groups.fetchNextPage()}
            >
              {groups.isFetchingNextPage ? "Loading…" : "Load more groups"}
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function GroupCard({
  group,
  canWrite,
  busy,
  decide,
}: {
  group: DupGroup;
  canWrite: boolean;
  busy: boolean;
  decide: (request: DupReviewRequest) => Promise<void>;
}) {
  const { confirm } = useDialogs();
  const [members, setMembers] = useState(group.members);
  const [memberCursor, setMemberCursor] = useState(group.members_cursor);
  const [loadingMembers, setLoadingMembers] = useState(false);
  const [memberError, setMemberError] = useState(false);
  const chosenKeep = group.chosen_keep ?? group.suggested_keep;

  useEffect(() => {
    setMembers(group.members);
    setMemberCursor(group.members_cursor);
    setLoadingMembers(false);
    setMemberError(false);
  }, [group.review, group.members, group.members_cursor]);

  const loadMembers = async () => {
    if (!group.group || !memberCursor || loadingMembers) return;
    setLoadingMembers(true);
    setMemberError(false);
    try {
      const page = await api.duplicateGroupMembers({ group: group.group, after: memberCursor });
      setMembers((current) => [...current, ...page.items]);
      setMemberCursor(page.cursor);
    } catch {
      setMemberError(true);
    } finally {
      setLoadingMembers(false);
    }
  };

  const chooseKeep = (member: DupMember) =>
    decide({ review: group.review, state: "pending", keep: member.asset.id });

  const remove = async (member: DupMember, block: boolean) => {
    const exactBlock = block && group.kind === "exact";
    const accepted = await confirm({
      title: block ? "Remove and block these bytes?" : "Remove this catalog record?",
      message: exactBlock
        ? `This blocks the shared content hash and removes every byte-identical catalog row in this group, including the chosen Keep. Source files are not deleted. Future scans will skip these bytes.`
        : block
          ? `This removes ${member.asset.name} and every byte-identical catalog row, then blocks those bytes from future scans. Source files are not deleted.`
          : `This removes only ${member.asset.name} from 3DAM’s catalog. Its source file remains on disk and a later scan may import it again.`,
      confirmLabel: block ? "Remove + block" : "Remove from catalog",
      danger: true,
    });
    if (!accepted) return;
    await decide({
      review: group.review,
      state: "pending",
      keep: chosenKeep,
      removals: [{ asset: member.asset.id, block }],
    });
  };

  const changeState = (state: "pending" | "resolved" | "dismissed") =>
    decide({ review: group.review, state, keep: chosenKeep });

  return (
    <section
      className="rounded border border-border bg-surface p-3 focus-visible:outline-2 focus-visible:outline-accent"
      tabIndex={0}
      aria-label={`${group.media} duplicate group, ${group.review_state}`}
      onKeyDown={(event) => {
        if (event.target !== event.currentTarget || !canWrite || busy) return;
        if (event.key === "Enter" && group.review_state === "pending") {
          event.preventDefault();
          void changeState("resolved");
        } else if (event.key.toLowerCase() === "d" && group.review_state === "pending") {
          event.preventDefault();
          void changeState("dismissed");
        }
      }}
    >
      <div className="mb-3 flex flex-wrap items-start justify-between gap-2">
        <div>
          <p className="text-[11px] text-fg-dim">
            {mediaLabel[group.media]} · {group.total_members} items · {group.review_state}
          </p>
          <p className="mt-1 text-xs text-fg-muted">Grouped by: {group.signal}</p>
          <p className="mt-0.5 text-[11px] text-fg-dim">
            Suggested keep: {group.suggested_keep_reason}
            {group.chosen_keep && group.chosen_keep !== group.suggested_keep
              ? " You overrode this suggestion."
              : ""}
          </p>
        </div>
        <div className="flex flex-wrap gap-1.5">
          {group.review_state === "pending" ? (
            <>
              <button
                className="btn coarse:min-h-11"
                aria-keyshortcuts="Enter"
                disabled={!canWrite || busy}
                onClick={() => void changeState("resolved")}
              >
                <CheckCircle2 size={13} /> Resolve <Shortcut>Enter</Shortcut>
              </button>
              <button
                className="btn coarse:min-h-11"
                aria-keyshortcuts="D"
                disabled={!canWrite || busy}
                onClick={() => void changeState("dismissed")}
              >
                <XCircle size={13} /> Dismiss <Shortcut>D</Shortcut>
              </button>
            </>
          ) : (
            <button
              className="btn coarse:min-h-11"
              disabled={!canWrite || busy}
              onClick={() => void changeState("pending")}
            >
              <RotateCcw size={13} /> Reopen
            </button>
          )}
        </div>
      </div>

      <p className="mb-2 text-[10px] text-fg-dim">
        Focus a member card and press K to keep, R to remove, or B to remove + block.
      </p>
      <div className="grid grid-cols-1 gap-2 sm:grid-cols-2 xl:grid-cols-3">
        {members.map((member) => (
          <MemberCard
            key={member.asset.id}
            member={member}
            selectedKeep={member.asset.id === chosenKeep}
            suggested={member.asset.id === group.suggested_keep}
            canWrite={canWrite && group.review_state === "pending"}
            busy={busy}
            onKeep={() => chooseKeep(member)}
            onRemove={() => remove(member, false)}
            onBlock={() => remove(member, true)}
          />
        ))}
      </div>
      {memberCursor && group.group && (
        <button className="btn mt-2" disabled={loadingMembers} onClick={() => void loadMembers()}>
          {loadingMembers
            ? "Loading…"
            : `Load more members (${members.length}/${group.total_members})`}
        </button>
      )}
      {memberError && <p className="mt-2 text-[11px] text-danger">Couldn’t load more members.</p>}
      {!memberCursor && members.length < group.total_members && (
        <p className="mt-2 text-[11px] text-fg-dim">
          Showing {members.length} of {group.total_members} computed near-duplicate members.
        </p>
      )}
    </section>
  );
}

function MemberCard({
  member,
  selectedKeep,
  suggested,
  canWrite,
  busy,
  onKeep,
  onRemove,
  onBlock,
}: {
  member: DupMember;
  selectedKeep: boolean;
  suggested: boolean;
  canWrite: boolean;
  busy: boolean;
  onKeep: () => void;
  onRemove: () => Promise<void>;
  onBlock: () => Promise<void>;
}) {
  const asset = member.asset;
  const origin = typeof asset.origin === "object" ? `Peer ${asset.origin.peer}` : "Local";
  const attributes = Object.values(asset.key_attrs).join(" · ") || "No media attributes";
  return (
    <article
      tabIndex={0}
      className="overflow-hidden rounded border border-border bg-surface-2 focus-visible:outline-2 focus-visible:outline-accent"
      aria-label={`${asset.name}${selectedKeep ? ", chosen keep" : ""}`}
      onKeyDown={(event) => {
        if (event.target !== event.currentTarget || !canWrite || busy) return;
        const key = event.key.toLowerCase();
        if (key === "k") {
          event.preventDefault();
          onKeep();
        } else if (key === "r") {
          event.preventDefault();
          void onRemove();
        } else if (key === "b") {
          event.preventDefault();
          void onBlock();
        }
      }}
    >
      <div className="flex gap-3 p-2">
        <Link to={`/?sel=${asset.id}`} className="relative h-24 w-24 shrink-0 overflow-hidden rounded">
          <Thumbnail asset={asset} size={48} />
          {selectedKeep && (
            <span className="absolute top-1 left-1 rounded bg-accent px-1 py-0.5 text-[9px] font-semibold text-accent-fg">
              {suggested ? "Suggested keep" : "Chosen keep"}
            </span>
          )}
        </Link>
        <dl className="min-w-0 flex-1 space-y-1 text-[10px]">
          <Comparison label="Name" value={asset.name} />
          <Comparison label="Path" value={member.path} mono />
          <Comparison label="Source" value={`${member.source} · ${origin}`} />
          <Comparison label="Size" value={bytes(asset.size)} />
          <Comparison label="Media" value={attributes} />
          <Comparison label="Modified" value={formatDate(member.modified_at)} />
          <div className="flex items-center justify-between gap-2">
            <dt className="text-fg-dim">License</dt>
            <dd><LicenseBadge badge={asset.license} /></dd>
          </div>
          <Comparison
            label="Analysis"
            value={member.analyzed_at ? `Analyzed ${formatDate(member.analyzed_at)}` : "Not analyzed"}
          />
        </dl>
      </div>
      <div className="grid grid-cols-3 gap-1 border-t border-border p-1.5">
        <button
          className="btn min-w-0 justify-center coarse:min-h-11"
          aria-keyshortcuts="K"
          disabled={!canWrite || busy || selectedKeep}
          onClick={onKeep}
        >
          <Check size={12} /> Keep <Shortcut>K</Shortcut>
        </button>
        <button
          className="btn min-w-0 justify-center text-danger coarse:min-h-11"
          aria-keyshortcuts="R"
          disabled={!canWrite || busy || selectedKeep}
          onClick={() => void onRemove()}
        >
          <Trash2 size={12} /> Remove <Shortcut>R</Shortcut>
        </button>
        <button
          className="btn min-w-0 justify-center text-danger coarse:min-h-11"
          aria-keyshortcuts="B"
          disabled={!canWrite || busy}
          onClick={() => void onBlock()}
        >
          <Ban size={12} /> Remove + block <Shortcut>B</Shortcut>
        </button>
      </div>
    </article>
  );
}

function Comparison({ label, value, mono = false }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-start justify-between gap-2">
      <dt className="shrink-0 text-fg-dim">{label}</dt>
      <dd className={`min-w-0 truncate text-right text-fg-muted${mono ? " font-mono" : ""}`} title={value}>
        {value}
      </dd>
    </div>
  );
}

function Shortcut({ children }: { children: React.ReactNode }) {
  return <kbd className="ml-1 text-[9px] text-fg-dim">{children}</kbd>;
}

function formatDate(value?: number | null): string {
  if (!value) return "Unknown";
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(
    new Date(value),
  );
}
