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
}

declare global {
  interface Window {
    /** Injected by the desktop shell's webview; never present in a plain browser. */
    __TAURI__?: { dialog?: TauriDialog };
  }
}

/** The shell's dialog API, or null outside the desktop shell — gate desktop-only UI on this. */
export function tauriDialog(): TauriDialog | null {
  return window.__TAURI__?.dialog ?? null;
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
