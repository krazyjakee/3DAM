// The Tauri desktop shell (ADR 0013) — feature detection plus the sliver of its API the web client
// uses. The shell serves this exact client from an in-process server, so there is no bundler and no
// `@tauri-apps/api` import; instead the shell injects the IPC bundle as the `window.__TAURI__`
// global (`app.withGlobalTauri`), and a loopback-scoped capability in `crates/3dam-desktop`
// (`capabilities/loopback-dialog.json`) grants exactly the dialog-open command to that origin. In a
// plain browser the global is absent, everything here reports unavailable, and callers hide their
// desktop-only affordances.

/** The dialog-plugin surface we call (tauri-plugin-dialog v2, `dialog:allow-open`). */
interface TauriDialog {
  open(options: {
    directory?: boolean;
    multiple?: boolean;
    title?: string;
  }): Promise<string | string[] | null>;
  save(options: { title?: string; defaultPath?: string }): Promise<string | null>;
}

interface TauriOpener {
  openPath(path: string): Promise<void>;
}

declare global {
  interface Window {
    /** Injected by the desktop shell's webview; never present in a plain browser. */
    __TAURI__?: { dialog?: TauriDialog; opener?: TauriOpener };
    /** Immutable launch-mode bit injected before page scripts by the native shell. */
    __3DAM_EMBEDDED_SERVER__?: boolean;
  }
}

/** The shell's dialog API, or null outside the desktop shell — gate desktop-only UI on this. */
export function tauriDialog(): TauriDialog | null {
  return window.__TAURI__?.dialog ?? null;
}

/** True only when a native path and the active API resolve on the same computer. A Tauri shell may
 * remain available while its API points at a hosted server, so feature detection alone is not a
 * locality decision. */
export function hasLocalFilesystemAccess(): boolean {
  return window.__3DAM_EMBEDDED_SERVER__ === true && tauriDialog() !== null;
}

/** Open the native directory picker. Resolves to the chosen absolute path, or null when the user
 *  cancels or the shell isn't there. Rejects if the shell denies the call (e.g. the page came from
 *  a non-loopback `--connect` server, which the capability deliberately excludes). */
export async function pickDirectory(title: string): Promise<string | null> {
  const dialog = tauriDialog();
  if (!dialog) return null;
  const picked = await dialog.open({ directory: true, multiple: false, title });
  return typeof picked === "string" ? picked : null;
}

export async function pickFile(title: string): Promise<string | null> {
  const dialog = tauriDialog();
  if (!dialog) return null;
  const picked = await dialog.open({ directory: false, multiple: false, title });
  return typeof picked === "string" ? picked : null;
}

export async function saveFile(title: string, defaultPath: string): Promise<string | null> {
  const dialog = tauriDialog();
  return dialog ? dialog.save({ title, defaultPath }) : null;
}

export async function openNativePath(path: string): Promise<boolean> {
  const opener = window.__TAURI__?.opener;
  if (!opener) return false;
  await opener.openPath(path);
  return true;
}
