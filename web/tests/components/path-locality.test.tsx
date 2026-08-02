import { afterEach, describe, expect, it } from "vitest";
import { hasLocalFilesystemAccess } from "@/lib/tauri";

const dialog = {
  open: async () => null,
  save: async () => null,
};

afterEach(() => {
  window.__TAURI__ = undefined;
  window.__3DAM_EMBEDDED_SERVER__ = undefined;
});

describe("filesystem locality", () => {
  it("allows native paths only for an explicitly embedded shell", () => {
    window.__TAURI__ = { dialog };
    window.__3DAM_EMBEDDED_SERVER__ = true;
    expect(hasLocalFilesystemAccess()).toBe(true);
  });

  it("does not mistake a hosted Tauri page with an exposed dialog global for embedded mode", () => {
    window.__TAURI__ = { dialog };
    window.__3DAM_EMBEDDED_SERVER__ = false;
    expect(hasLocalFilesystemAccess()).toBe(false);
  });

  it("does not offer native paths in a browser or without an explicit launch-mode signal", () => {
    expect(hasLocalFilesystemAccess()).toBe(false);
    window.__TAURI__ = { dialog };
    expect(hasLocalFilesystemAccess()).toBe(false);
  });
});
