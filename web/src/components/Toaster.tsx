// The toast viewport (issue #23) — the single place transient success/failure feedback surfaces.
// Bottom-anchored so it never covers the toolbar; a polite live-region announces each toast (errors
// as `alert`) so the feedback reaches assistive tech, not just sighted users. Uses the design tokens
// (DESIGN_GUIDELINES §4), no hardcoded palette.

import { CheckCircle2, Info, X, XCircle } from "lucide-react";
import { dismiss, useToasts, type ToastKind } from "@/lib/toast";

const ICON = { success: CheckCircle2, error: XCircle, info: Info } as const;

const ACCENT: Record<ToastKind, string> = {
  success: "var(--color-lic-permissive)",
  error: "var(--color-danger)",
  info: "var(--color-accent)",
};

export function Toaster() {
  const toasts = useToasts();
  return (
    <div
      aria-live="polite"
      aria-atomic="false"
      className="pointer-events-none fixed inset-x-0 bottom-0 z-50 flex flex-col items-center gap-2 p-4 sm:items-end"
    >
      {toasts.map((t) => {
        const Icon = ICON[t.kind];
        return (
          <div
            key={t.id}
            role={t.kind === "error" ? "alert" : "status"}
            className="pointer-events-auto flex w-full max-w-sm items-start gap-2 rounded-md border px-3 py-2 text-xs shadow-lg"
            style={{
              background: "var(--color-surface-2)",
              borderColor: "var(--color-border-strong)",
            }}
          >
            <Icon size={15} className="mt-px shrink-0" style={{ color: ACCENT[t.kind] }} />
            <div className="min-w-0 flex-1 text-fg">
              <p className="break-words">{t.message}</p>
              {t.manualCopy && (
                <textarea
                  readOnly
                  rows={2}
                  value={t.manualCopy.value}
                  aria-label={t.manualCopy.label}
                  spellCheck={false}
                  onFocus={(e) => e.currentTarget.select()}
                  className="field mt-2 resize-none font-mono text-[10px]"
                />
              )}
            </div>
            <button
              type="button"
              aria-label="Dismiss notification"
              onClick={() => dismiss(t.id)}
              className="shrink-0 text-fg-dim transition-colors hover:text-fg"
            >
              <X size={14} />
            </button>
          </div>
        );
      })}
    </div>
  );
}
