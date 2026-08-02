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
        "src/components/Inspector.tsx",
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
