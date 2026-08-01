import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

test("native credential survives hostile prototype replacement without disclosure", async () => {
  const sentinel = "dam_XSS_SENTINEL_NATIVE_SECRET";
  const templateUrl = new URL(
    "../../crates/3dam-desktop/src/native-credential.js",
    import.meta.url,
  );
  const template = await readFile(templateUrl, "utf8");
  const script = template
    .replace("__3DAM_CREDENTIAL_JSON__", JSON.stringify(sentinel))
    .replace("__3DAM_ORIGIN_JSON__", JSON.stringify("https://library.test"))
    .replace("__3DAM_PATH_JSON__", JSON.stringify("/dam"))
    .replace("__3DAM_BASE_JSON__", JSON.stringify("/dam"))
    .replace("__3DAM_FORGETTABLE_JSON__", "true");

  const delivered: Array<{ input: unknown; init: { headers?: FakeHeaders } | undefined }> = [];
  const observedByPage: string[] = [];

  class FakeHeaders {
    values = new Map<string, string>();
    constructor(source?: FakeHeaders | Record<string, string>) {
      if (source instanceof FakeHeaders) this.values = new Map(source.values);
      else if (source) Object.entries(source).forEach(([key, value]) => this.set(key, value));
    }
    set(name: string, value: string) {
      this.values.set(name.toLowerCase(), value);
    }
    has(name: string) {
      return this.values.has(name.toLowerCase());
    }
    forEach(callback: (value: string, name: string) => void) {
      this.values.forEach(callback);
    }
  }
  class FakeRequest {
    private readonly requestUrl: string;
    private readonly requestInit: { headers?: FakeHeaders };
    constructor(requestUrl: string, requestInit: { headers?: FakeHeaders } = {}) {
      this.requestUrl = requestUrl;
      this.requestInit = requestInit;
    }
    get url() {
      return this.requestUrl;
    }
    get headers() {
      return this.requestInit.headers ?? new FakeHeaders();
    }
  }
  class FakeURL {
    private readonly parsed: URL;
    constructor(value: string, base?: string) {
      this.parsed = new URL(value, base);
    }
    get origin() {
      return this.parsed.origin;
    }
    get pathname() {
      return this.parsed.pathname;
    }
  }

  const context = {
    Headers: FakeHeaders,
    Request: FakeRequest,
    URL: FakeURL,
    location: { href: "https://library.test/dam/" },
    window: {
      __3DAM_NATIVE_BASE__: undefined as string | undefined,
      __3DAM_NATIVE_CREDENTIAL_FORGETTABLE__: undefined as boolean | undefined,
      fetch: async (input: unknown, init?: { headers?: FakeHeaders }) => {
        delivered.push({ input, init });
        return { ok: true };
      },
    },
  };
  vm.runInNewContext(script, context);
  assert.equal(context.window.__3DAM_NATIVE_BASE__, "/dam");
  assert.equal(context.window.__3DAM_NATIVE_CREDENTIAL_FORGETTABLE__, true);

  // Simulate an XSS replacing every obvious observation point after native initialization.
  FakeHeaders.prototype.set = function (name: string, value: string) {
    observedByPage.push(value);
    this.values.set(name.toLowerCase(), value);
  };
  Object.defineProperty(FakeRequest.prototype, "headers", {
    get() {
      observedByPage.push("request headers getter reached");
      return new FakeHeaders();
    },
  });
  Object.defineProperty(FakeURL.prototype, "origin", {
    get() {
      observedByPage.push("URL origin getter reached");
      return "https://attacker.test";
    },
  });

  await context.window.fetch("https://library.test/dam/api/version", {});
  await context.window.fetch("https://library.test/dam-evil/collect", {});

  assert.equal(delivered.length, 2);
  assert.equal(delivered[0].init?.headers?.values.get("authorization"), `Bearer ${sentinel}`);
  assert.equal(delivered[1].init?.headers, undefined, "mount-prefix lookalike is not authorized");
  assert.equal(
    observedByPage.some((value) => value.includes(sentinel)),
    false,
    "page prototype hooks never observe the bearer",
  );
});
