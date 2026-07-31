// A shared write-gate for user-facing mutating controls (front-door auth, scope-aware UI). Rather
// than each button re-deriving "am I allowed to write?" and inventing its own disabled/tooltip
// wording, they call `useWriteGate()` and spread `gate(...)` onto the control. Consistent behaviour:
// controls disable (not hide) without the write scope, and carry one standard explanatory title.

import { useCan } from "@/api/queries";
import { AUTH_COPY } from "@/lib/auth";

export interface WriteGate {
  /** Does the caller hold the write scope? */
  canWrite: boolean;
  /**
   * Props for a mutating control. Merges the caller's own `disabled`/`title` with the write gate:
   * disabled when the caller lacks write (or already disabled), and the standard "requires write"
   * title takes over exactly when the gate is what's blocking it.
   */
  gate: (opts?: { disabled?: boolean; title?: string }) => {
    disabled: boolean;
    title: string | undefined;
  };
}

export function useWriteGate(): WriteGate {
  const canWrite = useCan("write");
  return {
    canWrite,
    gate: (opts) => ({
      disabled: !canWrite || !!opts?.disabled,
      title: !canWrite ? AUTH_COPY.needsWrite : opts?.title,
    }),
  };
}
