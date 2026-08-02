import "@testing-library/jest-dom/vitest";

import { cleanup } from "@testing-library/react";
import { afterAll, afterEach, beforeAll } from "vitest";
import { clearServer } from "../../src/lib/server";
import { server } from "./server";

const nativeFetch = globalThis.fetch;
let interceptedFetch: typeof fetch;

beforeAll(() => {
  server.listen({ onUnhandledRequest: "error" });
  interceptedFetch = globalThis.fetch;
  // The production client deliberately uses same-origin paths. Node's fetch requires absolute URLs,
  // so resolve them exactly as a browser at the configured jsdom origin would before MSW sees them.
  globalThis.fetch = (input, init) => {
    const resolved =
      typeof input === "string" && input.startsWith("/")
        ? new URL(input, window.location.origin)
        : input;
    return interceptedFetch(resolved, init);
  };
});

afterEach(() => {
  cleanup();
  server.resetHandlers();
  clearServer();
  localStorage.clear();
  sessionStorage.clear();
  document.cookie = "dam_csrf=; Max-Age=0; path=/";
});

afterAll(() => {
  globalThis.fetch = interceptedFetch;
  server.close();
  globalThis.fetch = nativeFetch;
});

Object.defineProperties(HTMLElement.prototype, {
  clientWidth: { configurable: true, get: () => 1024 },
  clientHeight: { configurable: true, get: () => 768 },
  offsetWidth: { configurable: true, get: () => 1024 },
  offsetHeight: { configurable: true, get: () => 48 },
  // jsdom has no layout, while the production focus trap intentionally excludes display:none.
  offsetParent: {
    configurable: true,
    get: function (this: HTMLElement) {
      return this.parentElement;
    },
  },
});

HTMLElement.prototype.getBoundingClientRect = () =>
  ({
    x: 0,
    y: 0,
    top: 0,
    right: 1024,
    bottom: 768,
    left: 0,
    width: 1024,
    height: 768,
    toJSON: () => ({}),
  }) as DOMRect;

class TestResizeObserver implements ResizeObserver {
  constructor(private readonly callback: ResizeObserverCallback) {}
  disconnect(): void {}
  unobserve(): void {}
  observe(target: Element): void {
    this.callback(
      [{ target, contentRect: target.getBoundingClientRect() } as ResizeObserverEntry],
      this,
    );
  }
}

globalThis.ResizeObserver = TestResizeObserver;
window.matchMedia = (query: string): MediaQueryList =>
  ({
    matches: false,
    media: query,
    onchange: null,
    addListener: () => {},
    removeListener: () => {},
    addEventListener: () => {},
    removeEventListener: () => {},
    dispatchEvent: () => false,
  }) as MediaQueryList;

let animationFrame = 0;
globalThis.requestAnimationFrame = (callback: FrameRequestCallback) => {
  callback(performance.now());
  animationFrame += 1;
  return animationFrame;
};
globalThis.cancelAnimationFrame = () => {};

if (!globalThis.CSS) Object.defineProperty(globalThis, "CSS", { value: {} });
if (!globalThis.CSS.escape) globalThis.CSS.escape = (value: string) => value.replace(/[^\w-]/g, "\\$&");
if (!URL.createObjectURL) URL.createObjectURL = () => "blob:http://localhost/test-media";
if (!URL.revokeObjectURL) URL.revokeObjectURL = () => {};
