import { useState } from "react";
import { MessageSquare, Pencil, Reply, Trash2, X } from "lucide-react";
import {
  useComments,
  useDeleteComment,
  useEditComment,
  usePostComment,
  useVersion,
  useWhoami,
} from "@/api/queries";
import type { Asset, Comment } from "@/api/types";
import { relTime } from "@/lib/format";

/** Per-asset discussion (issue #82) — append-only, authored, time-ordered.
 *
 *  **Why this looks nothing like the note field.** The companion ticket warned that two unlabelled
 *  text areas in one Inspector is a design failure, and it is right. Three things keep them apart:
 *
 *  1. **They rarely coexist.** Discussion is gated on the `user_accounts` flag and renders nothing
 *     without it, so the single-user local library — the common case — still shows exactly one text
 *     box. The confusion is mostly designed out rather than labelled away.
 *  2. **Different position and grammar.** The note sits inside the metadata stack because it *is* a
 *     property of the asset: one box, no author, no timestamps, autosaved. Discussion sits at the
 *     bottom, past the derived sections, as a list of authored messages with an explicit Post
 *     button. Autosave-vs-send is itself the strongest signal of which kind of thing you're using.
 *  3. **The empty state says the difference** in the user's own terms, once, where they'll read it.
 *
 *  Threading is flat with one level of quoting (`reply_to`), not arbitrary nesting — deep trees are
 *  a lot of UI for little value on a three-message exchange. */
export function Discussion({ asset }: { asset: Asset }) {
  const version = useVersion();
  const whoami = useWhoami();
  // Off ⇒ absent (ADR 0004): the routes 404, so don't render a panel that can only fail.
  const accountsOn = version.data?.accounts === true;
  const me = whoami.data?.account ?? null;

  const id = asset.summary.id;
  const comments = useComments(id, accountsOn);
  const post = usePostComment(id);
  const [draft, setDraft] = useState("");
  const [replyTo, setReplyTo] = useState<Comment | null>(null);

  if (!accountsOn) return null;

  const messages = comments.data ?? [];
  const live = messages.filter((c) => !c.deleted_at).length;
  const canPost = !!me;

  const submit = () => {
    const body = draft.trim();
    if (!body || post.isPending) return;
    post.mutate(
      { body, replyTo: replyTo?.id },
      {
        onSuccess: () => {
          setDraft("");
          setReplyTo(null);
        },
      },
    );
  };

  return (
    <div className="mt-6 border-t border-border pt-4">
      <div className="mb-2 flex items-center gap-1.5">
        <MessageSquare size={12} className="text-fg-dim" />
        <span className="text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
          Discussion{live > 0 ? ` (${live})` : ""}
        </span>
      </div>

      {comments.isLoading ? (
        <p className="text-[11px] text-fg-dim">Loading…</p>
      ) : messages.length === 0 ? (
        <p className="text-[11px] leading-relaxed text-fg-dim">
          Nothing here yet. Use the <span className="text-fg-muted">note</span> above for what to
          know about this asset; use discussion for what the team decided about it.
        </p>
      ) : (
        <ol className="space-y-2.5">
          {messages.map((c) => (
            <Message
              key={c.id}
              comment={c}
              parent={messages.find((m) => m.id === c.reply_to) ?? null}
              assetId={id}
              mine={!!me && me.account_id === c.author.id}
              onReply={() => setReplyTo(c)}
            />
          ))}
        </ol>
      )}

      {replyTo && (
        <div className="mt-3 flex items-start gap-1.5 rounded border-l-2 border-accent bg-surface-2 px-2 py-1 text-[10px] text-fg-dim">
          <span className="min-w-0 flex-1 truncate">
            Replying to {authorLabel(replyTo)}: “{replyTo.body}”
          </span>
          <button onClick={() => setReplyTo(null)} aria-label="Cancel reply" className="shrink-0">
            <X size={11} />
          </button>
        </div>
      )}

      <textarea
        className="field mt-2 min-h-16 resize-y leading-relaxed disabled:cursor-not-allowed disabled:opacity-60"
        rows={2}
        value={draft}
        disabled={!canPost}
        placeholder={canPost ? "Write a message…" : "Sign in to join the discussion"}
        aria-label="New discussion message"
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          // Enter sends, Shift+Enter breaks the line — the convention everywhere else you type a
          // message, and the reason this needs no "Send" affordance on touch either.
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            submit();
          }
        }}
      />
      <div className="mt-1 flex items-center justify-end gap-2">
        <span className="text-[10px] text-fg-dim">{post.isPending ? "Posting…" : ""}</span>
        <button
          className="btn"
          disabled={!canPost || !draft.trim() || post.isPending}
          onClick={submit}
        >
          Post
        </button>
      </div>
    </div>
  );
}

/** An author with no `display` is one whose account was deleted. The message stays — removing it
 *  would silently rewrite the project's history — so it is labelled honestly instead. */
function authorLabel(c: Comment): string {
  return c.author.display ?? "deleted user";
}

function Message({
  comment,
  parent,
  assetId,
  mine,
  onReply,
}: {
  comment: Comment;
  parent: Comment | null;
  assetId: string;
  mine: boolean;
  onReply: () => void;
}) {
  const edit = useEditComment(assetId);
  const del = useDeleteComment(assetId);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(comment.body);

  if (comment.deleted_at) {
    return (
      <li className="text-[11px] text-fg-dim italic">Message deleted.</li>
    );
  }

  return (
    <li className="text-[11px]">
      {parent && (
        <div className="mb-0.5 truncate border-l-2 border-border pl-1.5 text-[10px] text-fg-dim">
          {authorLabel(parent)}: {parent.deleted_at ? "(deleted)" : parent.body}
        </div>
      )}
      <div className="flex items-baseline gap-1.5">
        <span
          className={`font-medium ${comment.author.display ? "text-fg-muted" : "text-fg-dim italic"}`}
        >
          {authorLabel(comment)}
        </span>
        <span className="text-[10px] text-fg-dim">{relTime(comment.created_at)}</span>
        {comment.edited_at && <span className="text-[10px] text-fg-dim">· edited</span>}
        {mine && !editing && (
          <span className="ml-auto flex shrink-0 items-center gap-1">
            <button
              onClick={() => {
                setDraft(comment.body);
                setEditing(true);
              }}
              title="Edit"
              aria-label="Edit message"
              className="text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
            >
              <Pencil size={11} />
            </button>
            <button
              onClick={() => del.mutate(comment.id)}
              title="Delete"
              aria-label="Delete message"
              className="text-fg-dim hover:text-danger coarse:min-h-11 coarse:min-w-11"
            >
              <Trash2 size={11} />
            </button>
          </span>
        )}
        {!mine && (
          <button
            onClick={onReply}
            title="Reply"
            aria-label="Reply to message"
            className="ml-auto shrink-0 text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
          >
            <Reply size={11} />
          </button>
        )}
      </div>
      {editing ? (
        <div className="mt-1">
          <textarea
            className="field min-h-12 resize-y"
            rows={2}
            value={draft}
            aria-label="Edit message"
            onChange={(e) => setDraft(e.target.value)}
          />
          <div className="mt-1 flex justify-end gap-1.5">
            <button className="btn" onClick={() => setEditing(false)}>
              Cancel
            </button>
            <button
              className="btn btn-accent"
              disabled={!draft.trim() || edit.isPending}
              onClick={() =>
                edit.mutate(
                  { commentId: comment.id, body: draft.trim() },
                  { onSuccess: () => setEditing(false) },
                )
              }
            >
              Save
            </button>
          </div>
        </div>
      ) : (
        <p className="mt-0.5 leading-relaxed whitespace-pre-wrap text-fg-muted">{comment.body}</p>
      )}
    </li>
  );
}
