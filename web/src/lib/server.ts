// Runtime server endpoint and credential posture (hosted mode, issues #74/#128/#129).
//
// Only the non-secret server base is durable. Browser-entered bearer credentials live in
// sessionStorage, so closing the tab forgets them; they never enter localStorage, IndexedDB,
// query keys, URLs, or exported settings. The Tauri shell keeps its credential in the OS keychain
// and installs a narrow native fetch wrapper before application JavaScript starts; web code sees
// only the `nativeCredential` boolean, never that secret.

const KEY = "3dam.server";
const SESSION_TOKEN_KEY = "3dam.server.token";
const LEGACY_ADMIN_TOKEN_KEY = "dam_admin_token";
const MIGRATION_NOTICE_KEY = "3dam.server.credential-invalidated";

declare global {
  interface Window {
    /** Set by the Tauri initialization script. Contains no secret. */
    __3DAM_NATIVE_CREDENTIAL__?: boolean;
    /** Same-origin reverse-proxy mount selected by Tauri, or empty for the origin root. */
    __3DAM_NATIVE_BASE__?: string;
    /** True only for hosted credentials persisted in the OS keychain. */
    __3DAM_NATIVE_CREDENTIAL_FORGETTABLE__?: boolean;
  }
}

export interface ServerConfig {
  /** Origin (+ optional path) of the target server, e.g. "https://host:7878". Empty = same-origin. */
  base: string;
  /** Browser-only session credential. Never serialized to durable storage. */
  token: string;
  /** The native shell is attaching a keychain or per-launch credential below JavaScript. */
  nativeCredential: boolean;
  /** Whether File → Forget can delete a persisted hosted credential. */
  nativeCredentialForgettable: boolean;
}

function durableStorage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

function tabStorage(): Storage | null {
  try {
    return typeof sessionStorage === "undefined" ? null : sessionStorage;
  } catch {
    return null;
  }
}

function safeGet(storage: Storage | null, key: string): string | null {
  try {
    return storage?.getItem(key) ?? null;
  } catch {
    return null;
  }
}

function safeSet(storage: Storage | null, key: string, value: string): void {
  try {
    storage?.setItem(key, value);
  } catch {
    /* private/blocked storage: keep the in-memory config usable */
  }
}

function safeRemove(storage: Storage | null, key: string): void {
  try {
    storage?.removeItem(key);
  } catch {
    /* private/blocked storage: fail soft */
  }
}

function envBase(): string {
  return ((import.meta.env?.VITE_SERVER_BASE as string | undefined) ?? "").replace(/\/$/, "");
}

/** Erase legacy durable bearer copies. We deliberately invalidate them instead of silently moving
 * them into another JavaScript-readable persistent store; the UI tells the user to paste the token
 * again. The non-secret base URL survives the migration. */
function migrateDurableConfig(): string {
  const storage = durableStorage();
  if (!storage) return envBase();
  let base = envBase();
  let invalidated = false;
  try {
    const raw = storage.getItem(KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as { base?: unknown; token?: unknown };
      if (typeof parsed.base === "string") base = parsed.base.replace(/\/$/, "");
      invalidated = typeof parsed.token === "string" && parsed.token.length > 0;
      storage.setItem(KEY, JSON.stringify({ base }));
    }
  } catch {
    safeRemove(storage, KEY);
  }
  if (safeGet(storage, LEGACY_ADMIN_TOKEN_KEY)) invalidated = true;
  safeRemove(storage, LEGACY_ADMIN_TOKEN_KEY);
  if (invalidated) safeSet(storage, MIGRATION_NOTICE_KEY, "1");
  return base;
}

const durableBase = migrateDurableConfig();
const nativeCredential =
  typeof window !== "undefined" && window.__3DAM_NATIVE_CREDENTIAL__ === true;

let config: ServerConfig = {
  // A connected Tauri window already navigated to the selected server. Ignore any stale browser
  // connection target stored under that origin: native credential attachment is deliberately
  // scoped to the loaded origin+mount, so same-origin is the only honest API base in this mode.
  base: nativeCredential ? (window.__3DAM_NATIVE_BASE__ ?? "") : durableBase,
  token: safeGet(tabStorage(), SESSION_TOKEN_KEY) ?? "",
  nativeCredential,
  nativeCredentialForgettable:
    nativeCredential && window.__3DAM_NATIVE_CREDENTIAL_FORGETTABLE__ === true,
};

export function getServer(): ServerConfig {
  return config;
}

export function hasBearerCredential(): boolean {
  return !!config.token || config.nativeCredential;
}

/** Consume the one-time recovery notice left when a legacy durable token was invalidated. */
export function takeCredentialMigrationNotice(): boolean {
  const storage = durableStorage();
  if (safeGet(storage, MIGRATION_NOTICE_KEY) !== "1") return false;
  safeRemove(storage, MIGRATION_NOTICE_KEY);
  return true;
}

/** Is the client pointed at a remote server (vs the origin that served it)? */
export function isRemote(): boolean {
  return config.base !== "";
}

/** A short host[:port] label for the connection chip. */
export function serverLabel(): string {
  if (!config.base) return "Local";
  try {
    return new URL(config.base).host;
  } catch {
    return config.base;
  }
}

export function setServer(base: string, token: string): void {
  config = {
    base: base.trim().replace(/\/$/, ""),
    token: token.trim(),
    nativeCredential: false,
    nativeCredentialForgettable: false,
  };
  safeSet(durableStorage(), KEY, JSON.stringify({ base: config.base }));
  const tabs = tabStorage();
  if (config.token) safeSet(tabs, SESSION_TOKEN_KEY, config.token);
  else safeRemove(tabs, SESSION_TOKEN_KEY);
}

export function clearServer(): void {
  config = {
    base: "",
    token: "",
    nativeCredential: false,
    nativeCredentialForgettable: false,
  };
  safeRemove(durableStorage(), KEY);
  safeRemove(tabStorage(), SESSION_TOKEN_KEY);
}

/** Forget the browser-session bearer while retaining the non-secret server base. A native
 * keychain credential is forgotten from the desktop application's native menu, where page script
 * has no access to the secret-store operation. */
export function clearToken(): void {
  config = { ...config, token: "" };
  safeRemove(tabStorage(), SESSION_TOKEN_KEY);
}

/** Resolve an API path against the configured base (relative/same-origin when unset). */
export function resolveUrl(path: string): string {
  return config.base ? `${config.base}${path}` : path;
}

/** `Authorization` header for `fetch` when a browser-session token is configured. In Tauri this is
 * empty: the initialization wrapper attaches its keychain secret inside a closure below page JS. */
export function authHeaders(): Record<string, string> {
  return config.token ? { authorization: `Bearer ${config.token}` } : {};
}

/** Fetch a media/API resource with the same bearer-header posture as the typed JSON client. */
export function authenticatedFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  for (const [name, value] of Object.entries(authHeaders())) headers.set(name, value);
  const url = /^https?:\/\//i.test(path) ? path : resolveUrl(path);
  return fetch(url, { ...init, headers });
}

/** The `x-dam-csrf` header for cookie-session-authenticated requests (user accounts, issue #42). */
export function csrfHeaders(): Record<string, string> {
  const m = document.cookie.match(/(?:^|;\s*)dam_csrf=([^;]+)/);
  return m ? { "x-dam-csrf": decodeURIComponent(m[1]) } : {};
}

/** Media resources are ordinary paths. Callers fetch them with [`authenticatedFetch`] and hand a
 * blob URL to elements/islands; a long-lived bearer is never serialized into the URL. */
export function mediaUrl(path: string): string {
  return path;
}

/** WebSocket URL carrying only a short-lived, one-use server ticket. */
export function wsUrl(path: string, ticket: string): string {
  const httpUrl = new URL(path, config.base || location.origin);
  httpUrl.protocol = httpUrl.protocol === "https:" ? "wss:" : "ws:";
  httpUrl.searchParams.set("ticket", ticket);
  return httpUrl.toString();
}
