import { HttpResponse, http } from "msw";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import { StorageSection } from "../../src/components/settings/StorageSection";
import { renderApp } from "./render";
import { server } from "./server";

test("factory reset cannot cross the destructive confirmation by accident", async () => {
  const user = userEvent.setup();
  let usageReads = 0;
  const bodies: unknown[] = [];
  server.use(
    http.get("http://localhost/admin/api/maintenance/usage", () => {
      usageReads += 1;
      return HttpResponse.json({
        data_dir: "/data",
        library_db_bytes: 1,
        server_db_bytes: 1,
        thumbnails: { bytes: 0, files: 0, budget_bytes: 1, hits: 0, misses: 0, evictions: 0, stale_deleted: 0 },
        previews: { bytes: 0, files: 0, budget_bytes: 1, hits: 0, misses: 0, evictions: 0, stale_deleted: 0 },
        peer_previews: { bytes: 0, files: 0, budget_bytes: 1, hits: 0, misses: 0, evictions: 0, stale_deleted: 0 },
        asset_count: 0,
        source_count: 0,
      });
    }),
    http.post("http://localhost/admin/api/maintenance/factory-reset", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json({
        catalog: { assets_removed: 4, sources_removed: 1, collections_removed: 0, tags_removed: 2 },
        cache: { bytes_freed: 256, files_deleted: 3 },
        tokens_removed: 1,
      });
    }),
  );

  renderApp(<StorageSection />);
  await waitFor(() => expect(usageReads).toBe(1));

  await user.click(screen.getByRole("button", { name: "Factory reset" }));
  let dialog = screen.getByRole("alertdialog", { name: "Factory reset everything?" });
  await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
  expect(bodies).toHaveLength(0);
  expect(usageReads).toBe(1);

  await user.click(screen.getByRole("button", { name: "Factory reset" }));
  dialog = screen.getByRole("alertdialog", { name: "Factory reset everything?" });
  await user.click(within(dialog).getByRole("button", { name: "Factory reset" }));

  await waitFor(() => expect(usageReads).toBe(2));
  expect(bodies).toEqual([{ confirm: true }]);
});
