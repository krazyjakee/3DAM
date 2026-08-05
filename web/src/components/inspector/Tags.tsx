// The Inspector's tag panel (issue #166, parent #96) — extracted from `Inspector.tsx`. Manual tag
// editing and automatic-suggestion review share one mutation pair (`useEditTags` +
// `useReviewSuggestion`); `web/tests/components/suggestion-review.test.tsx` is its regression guard.

import { useState } from "react";
import { Check, RotateCcw, X } from "lucide-react";
import { useCan, useEditTags, useReviewSuggestion, useTagVocabulary } from "@/api/queries";
import type { AssetId, Origin, ReviewAction, TagRef } from "@/api/types";
import { AUTH_COPY } from "@/lib/auth";
import { peerReadOnlyTitle } from "@/lib/origin";

/** Automatic tag/class review. Pending automation is visible here but confirmed-only discovery
 *  keeps it out of search and filters until a user accepts it. Decisions are reversible. */
export function TagList({
  assetId,
  tags,
  origin,
}: {
  assetId: AssetId;
  tags: TagRef[];
  origin: Origin;
}) {
  const review = useReviewSuggestion();
  const edit = useEditTags();
  const canWrite = useCan("write");
  const [newTag, setNewTag] = useState("");
  const vocabulary = useTagVocabulary(newTag.trim());
  // Tag review mutates this instance's catalog — a peer-owned asset's tags are reviewed on the peer.
  const peerTitle = peerReadOnlyTitle(origin);
  const manual = tags.filter((tag) => tag.source !== "auto");
  const automatic = tags.filter((tag) => tag.source === "auto");
  const disabled = !canWrite || !!peerTitle || edit.isPending;
  const add = () => {
    const tag = newTag.trim();
    if (!tag) return;
    edit.mutate(
      { assets: [assetId], add: [tag], dry_run: false },
      { onSuccess: () => setNewTag("") },
    );
  };
  return (
    <div className="flex flex-col gap-2">
      <div>
        <p className="mb-1 text-[10px] font-medium tracking-wide text-fg-dim uppercase">Manual</p>
        <div className="flex flex-wrap gap-1">
          {manual.length === 0 && <span className="text-[11px] text-fg-dim italic">No manual tags</span>}
          {manual.map((tag) => (
            <span key={tag.name} className="inline-flex items-center gap-1 rounded bg-surface-2 px-1.5 py-0.5 text-[10px] text-fg-muted">
              {tag.name}
              <button
                aria-label={`Remove manual tag ${tag.name}`}
                title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : "Remove manual tag")}
                disabled={disabled}
                onClick={() => edit.mutate({ assets: [assetId], remove: [tag.name], dry_run: false })}
                className="hover:text-danger disabled:cursor-not-allowed disabled:opacity-40"
              >
                <X size={10} />
              </button>
            </span>
          ))}
        </div>
        <div className="mt-1.5 flex gap-1">
          <input
            className="field min-w-0"
            aria-label="New manual tag"
            placeholder="Add a manual tag…"
            list="known-manual-tags"
            value={newTag}
            disabled={disabled}
            onChange={(event) => setNewTag(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                add();
              }
            }}
            title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : undefined)}
          />
          <button className="btn" disabled={disabled || !newTag.trim()} onClick={add}>Add</button>
          <datalist id="known-manual-tags">
            {(vocabulary.data ?? []).map((tag) => <option key={tag.name} value={tag.name} />)}
          </datalist>
        </div>
      </div>
      <div>
        <p className="text-[10px] font-medium tracking-wide text-fg-dim uppercase">
          Automatic suggestions &amp; classifications
        </p>
        <p className="mb-1.5 text-[10px] text-fg-dim">
          Only accepted suggestions affect search and filters.
        </p>
        {automatic.length === 0 ? (
          <p className="text-[11px] text-fg-dim italic">No automatic suggestions</p>
        ) : (
          <div className="flex flex-col gap-1.5">
            {automatic.map((tag) => (
              <TagChip
                key={tag.name}
                tag={tag}
                busy={review.isPending && review.variables?.tag === tag.name}
                peerTitle={peerTitle}
                onReview={(action) => review.mutate({ asset: assetId, tag: tag.name, action })}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function TagChip({
  tag,
  busy,
  peerTitle,
  onReview,
}: {
  tag: TagRef;
  busy: boolean;
  peerTitle?: string;
  onReview: (action: ReviewAction) => void;
}) {
  const auto = tag.source === "auto";
  const canWrite = useCan("write");
  const confidence = tag.confidence != null
    ? `${Math.round(tag.confidence * 100)}% confidence`
    : "Confidence unavailable";

  // User tags (and any non-auto) are not reviewable — render a plain chip.
  if (!auto) {
    return (
      <span
        className="rounded bg-surface-2 px-1.5 py-0.5 text-[10px] text-fg-muted"
        title="user tag"
      >
        {tag.name}
      </span>
    );
  }

  const pending = tag.state === "pending";
  const confirmed = tag.state === "confirmed";
  const disabled = busy || !canWrite || !!peerTitle;
  return (
    <article
      tabIndex={0}
      className={`rounded border p-2 text-[10px] focus-visible:outline-2 focus-visible:outline-accent ${
        pending
          ? "border-warn/50 bg-warn/5"
          : confirmed
            ? "border-accent/50 bg-accent/5"
            : "border-border bg-surface-2"
      }`}
      aria-label={`${tag.name}, ${tag.state}, ${confidence}`}
      onKeyDown={(event) => {
        if (
          event.target !== event.currentTarget ||
          event.altKey ||
          event.ctrlKey ||
          event.metaKey ||
          event.shiftKey ||
          disabled
        )
          return;
        const key = event.key.toLowerCase();
        if (pending && (key === "y" || key === "n")) {
          event.preventDefault();
          onReview(key === "y" ? "accept" : "reject");
        } else if (!pending && key === "u") {
          event.preventDefault();
          onReview("undo");
        }
      }}
    >
      <div className="flex items-center gap-2">
        <span className={`font-medium ${tag.state === "rejected" ? "text-fg-dim line-through" : "text-fg"}`}>
          {tag.name}
        </span>
        <span className={`rounded px-1 py-0.5 font-medium ${
          pending ? "text-warn" : confirmed ? "text-accent" : "text-danger"
        }`}>
          {pending ? "Pending" : confirmed ? "Accepted" : "Rejected"}
        </span>
        <span className="ml-auto tabular-nums text-fg-muted">{confidence}</span>
      </div>
      <p className="mt-1 text-fg-dim">Why: {tag.why || "The automated analyser proposed this value."}</p>
      {pending ? (
        <div className="mt-1.5 flex gap-1.5">
          <button
            className="btn flex-1 justify-center coarse:min-h-11"
            title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : "Accept suggestion (Y)")}
            aria-label={`Accept suggestion ${tag.name}`}
            aria-keyshortcuts="Y"
            disabled={disabled}
            onClick={() => onReview("accept")}
          >
            <Check size={12} /> Accept <kbd className="text-[9px] text-fg-dim">Y</kbd>
          </button>
          <button
            className="btn flex-1 justify-center text-danger coarse:min-h-11"
            title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : "Reject suggestion (N)")}
            aria-label={`Reject suggestion ${tag.name}`}
            aria-keyshortcuts="N"
            disabled={disabled}
            onClick={() => onReview("reject")}
          >
            <X size={12} /> Reject <kbd className="text-[9px] text-fg-dim">N</kbd>
          </button>
        </div>
      ) : (
        <button
          className="btn mt-1.5 w-full justify-center coarse:min-h-11"
          title={peerTitle ?? (!canWrite ? AUTH_COPY.needsWrite : "Undo decision (U)")}
          aria-label={`Undo ${tag.state} suggestion ${tag.name}`}
          aria-keyshortcuts="U"
          disabled={disabled}
          onClick={() => onReview("undo")}
        >
          <RotateCcw size={12} /> Undo <kbd className="text-[9px] text-fg-dim">U</kbd>
        </button>
      )}
    </article>
  );
}
