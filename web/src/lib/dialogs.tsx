// In-app confirm/prompt modals replacing the browser-native `confirm()`/`prompt()` (issue #29):
// those are un-themeable, break the dark UI, can be suppressed by "prevent this page from creating
// dialogs", and `prompt()` masks nothing for a bearer token. This exposes a promise-based API so a
// call site reads almost like the native one — `if (await confirm({…}))` / `await prompt({…})` — via
// a single provider that renders one modal at a time (focus-trapped, Escape-to-cancel).

import { createContext, useCallback, useContext, useEffect, useId, useRef, useState } from "react";
import type React from "react";
import { X } from "lucide-react";
import { useEscape, useFocusTrap } from "./use-focus-trap";

export interface ConfirmOpts {
  title: string;
  message?: React.ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  /** Style the confirm action as destructive (red). */
  danger?: boolean;
}

export interface PromptOpts {
  title: string;
  message?: React.ReactNode;
  initial?: string;
  placeholder?: string;
  /** Mask the input (bearer tokens, secrets). */
  password?: boolean;
  confirmLabel?: string;
  /** Allow submitting an empty value (e.g. "blank to clear"). */
  allowEmpty?: boolean;
}

interface DialogApi {
  /** Resolves `true` on confirm, `false` on cancel/Escape/backdrop. */
  confirm: (opts: ConfirmOpts) => Promise<boolean>;
  /** Resolves the entered string on submit, or `null` on cancel. */
  prompt: (opts: PromptOpts) => Promise<string | null>;
}

type Pending =
  | { kind: "confirm"; opts: ConfirmOpts; resolve: (v: boolean) => void }
  | { kind: "prompt"; opts: PromptOpts; resolve: (v: string | null) => void };

const Ctx = createContext<DialogApi | null>(null);

export function useDialogs(): DialogApi {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useDialogs must be used within <DialogProvider>");
  return ctx;
}

export function DialogProvider({ children }: { children: React.ReactNode }) {
  const [pending, setPending] = useState<Pending | null>(null);

  const confirm = useCallback(
    (opts: ConfirmOpts) =>
      new Promise<boolean>((resolve) => setPending({ kind: "confirm", opts, resolve })),
    [],
  );
  const prompt = useCallback(
    (opts: PromptOpts) =>
      new Promise<string | null>((resolve) => setPending({ kind: "prompt", opts, resolve })),
    [],
  );

  const finish = (result: boolean | string | null) => {
    if (!pending) return;
    (pending.resolve as (v: boolean | string | null) => void)(result);
    setPending(null);
  };

  return (
    <Ctx.Provider value={{ confirm, prompt }}>
      {children}
      {pending?.kind === "confirm" && (
        <ConfirmModal opts={pending.opts} onResult={(b) => finish(b)} />
      )}
      {pending?.kind === "prompt" && <PromptModal opts={pending.opts} onResult={(s) => finish(s)} />}
    </Ctx.Provider>
  );
}

function Overlay({ children, onClose }: { children: React.ReactNode; onClose: () => void }) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      onClick={onClose}
    >
      {children}
    </div>
  );
}

/**
 * A titled modal panel: backdrop + centred surface, focus-trapped, Escape/backdrop to close, with a
 * heading (optional icon) and a close button. Wraps the shared dialog boilerplate so feature dialogs
 * only supply their body.
 */
export function Modal({
  title,
  icon,
  labelledBy,
  onClose,
  wide,
  scroll,
  children,
}: {
  title: React.ReactNode;
  icon?: React.ReactNode;
  labelledBy: string;
  onClose: () => void;
  /** Widen from the default 380px to 420px (denser forms). */
  wide?: boolean;
  /** Cap height and scroll the body (long forms / reports). */
  scroll?: boolean;
  children: React.ReactNode;
}) {
  const ref = useFocusTrap<HTMLDivElement>(true);
  useEscape(onClose);
  return (
    <Overlay onClose={onClose}>
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby={labelledBy}
        className={`w-full ${wide ? "max-w-[420px]" : "max-w-[380px]"} rounded-lg border border-border bg-surface p-4 shadow-xl${
          scroll ? " max-h-[90vh] overflow-y-auto" : ""
        }`}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center justify-between">
          <h2 id={labelledBy} className="flex items-center gap-2 text-sm font-semibold">
            {icon} {title}
          </h2>
          <button
            className="flex items-center justify-center text-fg-dim hover:text-fg coarse:min-h-11 coarse:min-w-11"
            onClick={onClose}
            aria-label="Close"
          >
            <X size={16} />
          </button>
        </div>
        {children}
      </div>
    </Overlay>
  );
}

function ConfirmModal({ opts, onResult }: { opts: ConfirmOpts; onResult: (b: boolean) => void }) {
  const ref = useFocusTrap<HTMLDivElement>(true);
  const titleId = useId();
  const descriptionId = useId();
  useEscape(() => onResult(false));
  return (
    <Overlay onClose={() => onResult(false)}>
      <div
        ref={ref}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={opts.message != null ? descriptionId : undefined}
        className="w-full max-w-[380px] rounded-lg border border-border bg-surface p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id={titleId} className="text-sm font-semibold text-fg">
          {opts.title}
        </h2>
        {opts.message != null && (
          <div id={descriptionId} className="mt-2 text-xs whitespace-pre-line text-fg-muted">
            {opts.message}
          </div>
        )}
        <div className="mt-4 flex justify-end gap-2">
          <button className="btn" autoFocus={opts.danger} onClick={() => onResult(false)}>
            {opts.cancelLabel ?? "Cancel"}
          </button>
          <button
            autoFocus={!opts.danger}
            className={`btn ${opts.danger ? "text-danger" : "btn-accent"}`}
            style={
              opts.danger
                ? { borderColor: "color-mix(in srgb, var(--color-danger) 45%, transparent)" }
                : undefined
            }
            onClick={() => onResult(true)}
          >
            {opts.confirmLabel ?? "Confirm"}
          </button>
        </div>
      </div>
    </Overlay>
  );
}

function PromptModal({ opts, onResult }: { opts: PromptOpts; onResult: (s: string | null) => void }) {
  const ref = useFocusTrap<HTMLDivElement>(true);
  const inputRef = useRef<HTMLInputElement>(null);
  const [value, setValue] = useState(opts.initial ?? "");
  useEscape(() => onResult(null));
  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  const canSubmit = opts.allowEmpty || value.trim().length > 0;
  const submit = () => {
    if (canSubmit) onResult(value);
  };

  return (
    <Overlay onClose={() => onResult(null)}>
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby="prompt-dialog-title"
        className="w-full max-w-[380px] rounded-lg border border-border bg-surface p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id="prompt-dialog-title" className="text-sm font-semibold text-fg">
          {opts.title}
        </h2>
        {opts.message != null && <div className="mt-2 text-xs text-fg-muted">{opts.message}</div>}
        <input
          ref={inputRef}
          type={opts.password ? "password" : "text"}
          className="field mt-3"
          value={value}
          placeholder={opts.placeholder}
          spellCheck={false}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") submit();
          }}
        />
        <div className="mt-4 flex justify-end gap-2">
          <button className="btn" onClick={() => onResult(null)}>
            Cancel
          </button>
          <button className="btn btn-accent" onClick={submit} disabled={!canSubmit}>
            {opts.confirmLabel ?? "OK"}
          </button>
        </div>
      </div>
    </Overlay>
  );
}
