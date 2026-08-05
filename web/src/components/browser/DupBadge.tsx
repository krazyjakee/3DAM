/** A red count badge for a collapsed duplicate group — the top-right circle showing how many
 *  byte-identical copies are folded behind this card/row (the set is listed in the Inspector). Danger
 *  tone flags the redundant storage; capped at 99+ so a large group can't blow out the layout. */
export function DupBadge({ count, className = "" }: { count: number; className?: string }) {
  return (
    <span
      className={`flex h-4 min-w-4 items-center justify-center rounded-full px-1 text-[10px] font-semibold tabular-nums ${className}`}
      style={{ background: "var(--color-danger)", color: "var(--color-bg)" }}
      title={`${count} duplicate${count === 1 ? "" : "s"} — listed in the Inspector`}
      aria-label={`${count} duplicate${count === 1 ? "" : "s"}`}
    >
      {count > 99 ? "99+" : count}
    </span>
  );
}
