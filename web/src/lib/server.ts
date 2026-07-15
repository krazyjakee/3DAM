// Runtime server endpoint config (hosted mode, issue #74): point the web client at an arbitrary
// 3DAM server instead of only the origin that served it.
//
// The default is same-origin (empty base) — the embedded-SPA deploy is unchanged. When a base URL
// (and optional bearer token) are set they persist in localStorage and every transport targets that
// server: REST/JSON carries the token in the `Authorization` header; the WebSocket firehose and
// media element loads (`<img>`/`<audio>`) carry it as a `?token=` query param, since neither can set
// a header. A build can also ship a fixed default via `VITE_SERVER_BASE`.

const KEY = "3dam.server";

export interface ServerConfig {
  /** Origin (+ optional path) of the target server, e.g. "https://host:7878". Empty = same-origin. */
  base: string;
  /** Bearer token for a token-gated server, or "" for none. */
  token: string;
}

/** One-time migration: the admin surface used to keep its own token under this key. There is one
 *  credential now (front-door auth) — adopt the legacy value into the server config if it has no
 *  token of its own, then retire the key. */
const LEGACY_ADMIN_TOKEN_KEY = "dam_admin_token";

function load(): ServerConfig {
  let cfg: ServerConfig | null = null;
  try {
    const raw = localStorage.getItem(KEY);
    if (raw) {
      const p = JSON.parse(raw) as Partial<ServerConfig>;
      cfg = {
        base: typeof p.base === "string" ? p.base : "",
        token: typeof p.token === "string" ? p.token : "",
      };
    }
  } catch {
    /* corrupt entry — fall through to defaults */
  }
  if (!cfg) {
    const envBase = (import.meta.env.VITE_SERVER_BASE as string | undefined) ?? "";
    cfg = { base: envBase.replace(/\/$/, ""), token: "" };
  }
  try {
    const legacy = localStorage.getItem(LEGACY_ADMIN_TOKEN_KEY);
    if (legacy) {
      if (!cfg.token) {
        cfg.token = legacy;
        localStorage.setItem(KEY, JSON.stringify(cfg));
      }
      localStorage.removeItem(LEGACY_ADMIN_TOKEN_KEY);
    }
  } catch {
    /* storage unavailable — nothing to migrate */
  }
  return cfg;
}

let config = load();

export function getServer(): ServerConfig {
  return config;
}

/** Is the client pointed at a remote server (vs the origin that served it)? */
export function isRemote(): boolean {
  return config.base !== "";
}

/** A short host[:port] label for the connection chip. */
export function serverLabel(): string {
  if (!config.base) return "Local";
  try {
    const u = new URL(config.base);
    return u.host;
  } catch {
    return config.base;
  }
}

export function setServer(base: string, token: string): void {
  config = { base: base.trim().replace(/\/$/, ""), token: token.trim() };
  localStorage.setItem(KEY, JSON.stringify(config));
}

export function clearServer(): void {
  config = { base: "", token: "" };
  localStorage.removeItem(KEY);
}

/** Resolve an API path against the configured base (relative/same-origin when unset). */
export function resolveUrl(path: string): string {
  return config.base ? `${config.base}${path}` : path;
}

/** `Authorization` header for `fetch` when a token is configured. */
export function authHeaders(): Record<string, string> {
  return config.token ? { authorization: `Bearer ${config.token}` } : {};
}

/** A media URL (`<img>`/`<audio>`/mesh `src`): base-resolved, with the token as a `?token=` query
 *  param since element loads can't set an `Authorization` header. */
export function mediaUrl(path: string): string {
  const url = resolveUrl(path);
  if (!config.token) return url;
  return url + (url.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(config.token);
}

/** The `ws(s)://` firehose URL, base-resolved, with the token as a `?token=` query param. */
export function wsUrl(path: string): string {
  const httpUrl = new URL(path, config.base || location.origin);
  httpUrl.protocol = httpUrl.protocol === "https:" ? "wss:" : "ws:";
  if (config.token) httpUrl.searchParams.set("token", config.token);
  return httpUrl.toString();
}
