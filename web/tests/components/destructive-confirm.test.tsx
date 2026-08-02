import { HttpResponse, http } from "msw";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test, vi } from "vitest";
import { StorageSection } from "../../src/components/settings/StorageSection";
import { renderApp } from "./render";
import { server } from "./server";

test("factory reset cannot cross the destructive confirmation by accident", async () => {
  const user = userEvent.setup();
  const changed = vi.fn();
  const bodies: unknown[] = [];
  server.use(
    http.post("http://localhost/admin/api/maintenance/factory-reset", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json({
        catalog: { assets_removed: 4, sources_removed: 1, collections_removed: 0, tags_removed: 2 },
        cache: { bytes_freed: 256, files_deleted: 3 },
        tokens_removed: 1,
      });
    }),
  );

  renderApp(<StorageSection usage={null} error={null} onChange={changed} />);

  await user.click(screen.getByRole("button", { name: "Factory reset" }));
  let dialog = screen.getByRole("alertdialog", { name: "Factory reset everything?" });
  await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
  expect(bodies).toHaveLength(0);
  expect(changed).not.toHaveBeenCalled();

  await user.click(screen.getByRole("button", { name: "Factory reset" }));
  dialog = screen.getByRole("alertdialog", { name: "Factory reset everything?" });
  await user.click(within(dialog).getByRole("button", { name: "Factory reset" }));

  await waitFor(() => expect(changed).toHaveBeenCalledTimes(1));
  expect(bodies).toEqual([{ confirm: true }]);
});
