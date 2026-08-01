import assert from "node:assert/strict";
import test from "node:test";

class MemoryStorage {
  private readonly values = new Map<string, string>();
  get length() {
    return this.values.size;
  }
  clear() {
    this.values.clear();
  }
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  key(index: number) {
    return [...this.values.keys()][index] ?? null;
  }
  removeItem(key: string) {
    this.values.delete(key);
  }
  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
  dump() {
    return JSON.stringify(Object.fromEntries(this.values));
  }
}

test("legacy and current browser credentials never enter durable storage or URLs", async () => {
  const sentinel = "dam_XSS_SENTINEL_BROWSER_SECRET";
  const local = new MemoryStorage();
  const session = new MemoryStorage();
  local.setItem(
    "3dam.server",
    JSON.stringify({ base: "https://library.test", token: sentinel }),
  );
  Object.assign(globalThis, {
    localStorage: local,
    sessionStorage: session,
    window: globalThis,
    location: { origin: "https://client.test" },
  });

  const server = await import("../src/lib/server.ts");
  assert.equal(server.getServer().base, "https://library.test");
  assert.equal(server.getServer().token, "", "legacy durable credential is invalidated");
  assert.equal(local.dump().includes(sentinel), false, "XSS-style localStorage read finds no token");
  assert.equal(server.takeCredentialMigrationNotice(), true);

  server.setServer("https://library.test", sentinel);
  assert.equal(local.dump().includes(sentinel), false);
  assert.equal(session.getItem("3dam.server.token"), sentinel, "browser posture is tab/session-only");
  assert.equal(server.mediaUrl("/api/v1/assets/abc/content").includes(sentinel), false);
  assert.equal(server.wsUrl("/api/v1/ws", "dam_ws_derived").includes(sentinel), false);

  server.clearToken();
  assert.equal(session.dump().includes(sentinel), false, "Sign out forgets the tab credential");
});

test("native credential mode ignores a stale browser-selected server base", async () => {
  const local = new MemoryStorage();
  const session = new MemoryStorage();
  local.setItem("3dam.server", JSON.stringify({ base: "https://stale.test" }));
  Object.assign(globalThis, {
    localStorage: local,
    sessionStorage: session,
    window: Object.assign(globalThis, {
      __3DAM_NATIVE_CREDENTIAL__: true,
      __3DAM_NATIVE_BASE__: "/dam",
      __3DAM_NATIVE_CREDENTIAL_FORGETTABLE__: true,
    }),
    location: { origin: "https://selected.test" },
  });

  const server = await import(`../src/lib/server.ts?native=${Date.now()}`);
  assert.equal(server.getServer().base, "/dam");
  assert.equal(server.getServer().nativeCredential, true);
  assert.equal(server.getServer().nativeCredentialForgettable, true);
  assert.equal(server.resolveUrl("/api/version"), "/dam/api/version");
});
