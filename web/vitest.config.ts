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
        // Settings server state moved behind its own TanStack Query hooks (issue #163). Both the
        // orchestrator and hook module must remain visible to per-file thresholds.
        "src/api/admin-queries.ts",
        "src/api/updates.ts",
        "src/components/Updates.tsx",
        "src/components/UpdateNotice.tsx",
        "src/components/AuthGate.tsx",
        // Standalone forms extracted from AuthGate.tsx (issue #169). This is an allowlist, so each
        // new module must be named or per-file thresholds would silently stop applying to it.
        "src/components/auth/AccountLoginForm.tsx",
        "src/components/auth/ClaimScreen.tsx",
        "src/components/auth/TokenLoginForm.tsx",
        "src/components/Browser.tsx",
        // Extracted out of Browser.tsx (issue #164) — same allowlist rule as the Inspector split
        // below: unnamed extractions silently leave coverage.
        "src/components/browser/Breadcrumb.tsx",
        "src/components/browser/SelectionBar.tsx",
        "src/components/browser/Toolbar.tsx",
        // The grid/table renderers behind the `ListProps` contract (issue #165) — same rule.
        "src/components/browser/DupBadge.tsx",
        "src/components/browser/FavoriteStar.tsx",
        "src/components/browser/GridList.tsx",
        "src/components/browser/TableList.tsx",
        "src/components/browser/item.ts",
        "src/components/browser/types.ts",
        "src/components/browser/useBrowseWindowLoading.ts",
        "src/components/browser/useRovingFocus.ts",
        // Extracted out of Navigation.tsx (issue #168). Keep both modules named explicitly:
        // `thresholds.perFile` cannot protect files omitted from this allowlist.
        "src/components/navigation/Collections.tsx",
        "src/components/navigation/SourceRow.tsx",
        "src/components/Inspector.tsx",
        "src/components/Settings.tsx",
        // Extracted out of Inspector.tsx (issues #167, #166). `thresholds.perFile` is true and this
        // list is an allowlist, so code that leaves Inspector.tsx has to be named here or it leaves
        // coverage entirely and the threshold passes vacuously.
        "src/components/inspector/Collections.tsx",
        "src/components/inspector/Duplicates.tsx",
        "src/components/inspector/License.tsx",
        "src/components/inspector/MediaFacts.tsx",
        "src/components/inspector/NoteEditor.tsx",
        "src/components/inspector/preview/index.tsx",
        "src/components/inspector/primitives.tsx",
        "src/components/inspector/Similar.tsx",
        "src/components/inspector/Tags.tsx",
        // Extracted out of Settings.tsx (issues #161, #162) — same allowlist rule again.
        "src/components/settings/AccountsSection.tsx",
        "src/components/settings/GroupsSection.tsx",
        "src/components/settings/OidcSection.tsx",
        "src/components/settings/StorageSection.tsx",
        "src/components/settings/TokensSection.tsx",
        // The ref-backed upload batch and its row renderer extracted from Upload.tsx (issue #169).
        "src/components/upload/UploadRow.tsx",
        "src/components/upload/useUploadQueue.ts",
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
