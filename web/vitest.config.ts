import { fileURLToPath, URL } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  test: {
    environment: "jsdom",
    environmentOptions: { jsdom: { url: "http://localhost/" } },
    include: ["tests/components/**/*.test.tsx"],
    setupFiles: ["./tests/components/setup.ts"],
    clearMocks: true,
    restoreMocks: true,
    coverage: {
      provider: "v8",
      reporter: ["text", "json-summary"],
      reportsDirectory: "coverage",
      include: [
        "src/components/AuthGate.tsx",
        "src/components/Browser.tsx",
        // Extracted out of Browser.tsx (issue #164) — same allowlist rule as the Inspector split
        // below: unnamed extractions silently leave coverage.
        "src/components/browser/Breadcrumb.tsx",
        "src/components/browser/SelectionBar.tsx",
        "src/components/browser/Toolbar.tsx",
        "src/components/Inspector.tsx",
        // Extracted out of Inspector.tsx (issue #167). `thresholds.perFile` is true and this list is
        // an allowlist, so code that leaves Inspector.tsx has to be named here or it leaves coverage
        // entirely and the threshold passes vacuously.
        "src/components/inspector/preview/index.tsx",
        // Extracted out of Settings.tsx (issue #161) — same allowlist rule again.
        "src/components/settings/AccountsSection.tsx",
        "src/components/settings/GroupsSection.tsx",
        "src/components/settings/StorageSection.tsx",
        "src/lib/dialogs.tsx",
        "src/lib/use-focus-trap.ts",
      ],
      thresholds: {
        perFile: true,
        lines: 15,
        functions: 10,
        branches: 10,
        statements: 15,
      },
    },
  },
});
