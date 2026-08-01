// A tiny global toast store (issue #23). It lives outside React on purpose so non-component code —
// the TanStack Query `MutationCache` wired in App.tsx — can raise toasts for *every* mutation
// failure without each call site remembering to. Components subscribe via `useToasts()`.

import { useSyncExternalStore } from "react";

export type ToastKind = "success" | "warning" | "error" | "info";

export interface Toast {
  id: number;
  kind: ToastKind;
  message: string;
  /** A full value that remains selectable when an automatic copy action fails. */
  manualCopy?: {
    label: string;
    value: string;
  };
  /** Optional in-app destination/action associated with this outcome. */
  action?: {
    label: string;
    onClick: () => void;
  };
}

export interface ToastOptions {
  manualCopy?: Toast["manualCopy"];
  action?: Toast["action"];
}

let toasts: Toast[] = [];
const listeners = new Set<() => void>();
let nextId = 1;

function emit() {
  for (const l of listeners) l();
}

/** Errors linger (the user may need to read/act); successes auto-dismiss quickly. */
function push(kind: ToastKind, message: string, options?: ToastOptions): number {
  const id = nextId++;
  toasts = [...toasts, { id, kind, message, ...options }];
  emit();
  // A manual-copy fallback stays until explicitly dismissed: the user may need time to select the
  // complete value and switch to the destination. Ordinary errors still clear after seven seconds.
  if (!options?.manualCopy) {
    const ttl = kind === "error" || kind === "warning" ? 7000 : 3500;
    setTimeout(() => dismiss(id), ttl);
  }
  return id;
}

export function dismiss(id: number) {
  toasts = toasts.filter((t) => t.id !== id);
  emit();
}

export const toast = {
  success: (m: string, options?: ToastOptions) => push("success", m, options),
  warning: (m: string, options?: ToastOptions) => push("warning", m, options),
  error: (m: string, options?: ToastOptions) => push("error", m, options),
  info: (m: string, options?: ToastOptions) => push("info", m, options),
};

/** Normalise anything thrown (ApiError, Error, string) into a human message for a toast. */
export function errorMessage(e: unknown): string {
  if (e instanceof Error) return e.message;
  if (typeof e === "string") return e;
  return "Something went wrong";
}

function subscribe(cb: () => void) {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

function snapshot() {
  return toasts;
}

export function useToasts(): Toast[] {
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}
