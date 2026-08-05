// Per-section loading/error framing. Every administration section loads independently (a slow or
// failing one must never hold the rest of the screen hostage), so this framing is shared rather
// than owned by the top-level Administration component.

import type { ReactNode } from "react";

export function SectionLoading({ name }: { name: string }) {
  return (
    <div className="rounded border border-border px-3 py-2 text-fg-dim" role="status">
      Loading {name.toLowerCase()}…
    </div>
  );
}

export function SectionError({ name, message }: { name: string; message: string }) {
  return (
    <div className="rounded border border-danger/40 bg-danger/10 px-3 py-2 text-danger" role="alert">
      <span className="font-medium">{name} unavailable.</span> {message}
    </div>
  );
}

/** A section may keep rendering its last successful payload beneath a later refresh error. */
export function AdminSectionState({
  name,
  loading,
  error,
  children,
}: {
  name: string;
  loading: boolean;
  error: string | null;
  children: ReactNode;
}) {
  return (
    <>
      {loading && <SectionLoading name={name} />}
      {error && <SectionError name={name} message={error} />}
      {children}
    </>
  );
}

export function DependencyNote({ children }: { children: ReactNode }) {
  return (
    <p className="rounded border border-border px-3 py-2 text-xs text-fg-dim">
      {children}
    </p>
  );
}
