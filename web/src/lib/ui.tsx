// Small shared presentational primitives reused across the workspace views.

import type React from "react";

/**
 * Centred empty / loading / error state. `tone="danger"` reddens the text; `className` overrides the
 * default full-height layout (e.g. a bordered card — see {@link CenteredCard}).
 */
export function Centered({
  children,
  tone,
  className = "h-full",
}: {
  children: React.ReactNode;
  tone?: "danger";
  className?: string;
}) {
  return (
    <div
      className={`flex items-center justify-center px-6 text-center text-xs ${className}`}
      style={{ color: tone === "danger" ? "var(--color-danger)" : "var(--color-fg-dim)" }}
    >
      {children}
    </div>
  );
}

/** Bordered, flex-filling variant for full-page list views (blocklist, duplicates). */
export function CenteredCard({ children, tone }: { children: React.ReactNode; tone?: "danger" }) {
  return (
    <Centered tone={tone} className="flex-1 rounded border border-border py-12">
      {children}
    </Centered>
  );
}
