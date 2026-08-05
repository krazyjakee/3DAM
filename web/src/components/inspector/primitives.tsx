// The two shapes every Inspector panel is built from (issue #166, parent #96) — extracted from
// `Inspector.tsx` so a panel module can be read, tested, and rendered on its own.
//
// They live here rather than in `@/components` at large on purpose: this is the Inspector's own
// label/value grammar (right-aligned, truncating, 11px), tuned for a narrow rail. A wider surface
// wanting a key/value list should not inherit those constraints by accident.

import type React from "react";
import { ClipboardCopy } from "lucide-react";
import { copyText } from "@/lib/clipboard";

/** A titled block of fields — the Inspector's one sectioning device. */
export function Group({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="mt-4">
      <div className="mb-1.5 text-[10px] font-semibold tracking-wider text-fg-dim uppercase">
        {title}
      </div>
      <div className="space-y-1">{children}</div>
    </div>
  );
}

/** One label/value row. Long values truncate with the full text on hover; `copyable` adds the
 *  clipboard affordance (paths and URLs are meant to leave the app). */
export function Field({
  label,
  value,
  mono,
  copyable,
}: {
  label: string;
  value: string;
  mono?: boolean;
  copyable?: boolean;
}) {
  return (
    <div className="flex items-baseline justify-between gap-2 text-[11px]">
      <span className="shrink-0 text-fg-dim">{label}</span>
      <span className="flex min-w-0 items-center gap-1">
        <span
          className={`min-w-0 truncate text-right text-fg-muted ${mono ? "font-mono text-[10px]" : ""}`}
          title={value}
        >
          {value}
        </span>
        {copyable && (
          <button
            type="button"
            className="flex shrink-0 items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
            title={`Copy ${label.toLowerCase()}`}
            aria-label={`Copy ${label.toLowerCase()}`}
            onClick={() => void copyText(value, label)}
          >
            <ClipboardCopy size={12} />
          </button>
        )}
      </span>
    </div>
  );
}
