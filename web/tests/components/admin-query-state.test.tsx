import { HttpResponse, http } from "msw";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import type { FlagInfo } from "../../src/api/admin";
import { Administration } from "../../src/components/Settings";
import { Toaster } from "../../src/components/Toaster";
import { renderApp } from "./render";
import { server } from "./server";

const status = {
  bind: "127.0.0.1:7333",
  localhost_only: true,
  tls: false,
  auth: "off",
  mcp: "off",
  network_writes: false,
  exposed_without_auth: false,
  token_count: 0,
};

const usage = {
  data_dir: "/data",
  library_db_bytes: 1,
  server_db_bytes: 1,
  thumbnails: { bytes: 0, files: 0, budget_bytes: 1, hits: 0, misses: 0, evictions: 0, stale_deleted: 0 },
  previews: { bytes: 0, files: 0, budget_bytes: 1, hits: 0, misses: 0, evictions: 0, stale_deleted: 0 },
  peer_previews: { bytes: 0, files: 0, budget_bytes: 1, hits: 0, misses: 0, evictions: 0, stale_deleted: 0 },
  asset_count: 0,
  source_count: 0,
};

function networkWrites(version = 7, value = false): FlagInfo {
  return {
    key: "network_writes",
    value,
    version,
    live: true,
    exposure_increasing: true,
  };
}

function serveAdministration(flags: () => FlagInfo[]) {
  const reads = { status: 0, flags: 0, tokens: 0, audit: 0, storage: 0 };
  server.use(
    http.get("http://localhost/api/v1/whoami", () =>
      HttpResponse.json({ identity: "owner", anonymous: false, scopes: ["admin"] }),
    ),
    http.get("http://localhost/admin/api/status", () => {
      reads.status += 1;
      return HttpResponse.json(status);
    }),
    http.get("http://localhost/admin/api/flags", () => {
      reads.flags += 1;
      return HttpResponse.json(flags());
    }),
    http.get("http://localhost/admin/api/tokens", () => {
      reads.tokens += 1;
      return HttpResponse.json([]);
    }),
    http.get("http://localhost/admin/api/audit", () => {
      reads.audit += 1;
      return HttpResponse.json([]);
    }),
    http.get("http://localhost/admin/api/maintenance/usage", () => {
      reads.storage += 1;
      return HttpResponse.json(usage);
    }),
  );
  return reads;
}

function renderAdministration() {
  return renderApp(
    <>
      <Administration />
      <Toaster />
    </>,
  );
}

test("a flag conflict preserves the expected version and surfaces the server error", async () => {
  const user = userEvent.setup();
  const reads = serveAdministration(() => [networkWrites()]);
  const bodies: unknown[] = [];
  server.use(
    http.put("http://localhost/admin/api/flags/network_writes", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json(
        { code: "conflict", message: "Flag changed elsewhere; reload and try again." },
        { status: 409 },
      );
    }),
  );

  renderAdministration();
  await user.click(await screen.findByRole("switch", { name: "Network writes" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Flag changed elsewhere; reload and try again.",
  );
  expect(bodies).toEqual([{ value: true, expected_version: 7 }]);
  // Failed mutations do not pretend the cache changed or trigger a manual reload.
  expect(reads).toEqual({ status: 1, flags: 1, tokens: 1, audit: 1, storage: 1 });
});

test("an exposure confirmation retries once and invalidates the dependent overview queries", async () => {
  const user = userEvent.setup();
  let current = networkWrites();
  const reads = serveAdministration(() => [current]);
  const bodies: Array<Record<string, unknown>> = [];
  server.use(
    http.put("http://localhost/admin/api/flags/network_writes", async ({ request }) => {
      const body = (await request.json()) as Record<string, unknown>;
      bodies.push(body);
      if (body.confirm !== true) {
        return HttpResponse.json(
          { code: "confirmation_required", message: "This change increases exposure." },
          { status: 400 },
        );
      }
      current = networkWrites(8, true);
      return HttpResponse.json(current);
    }),
  );

  renderAdministration();
  await user.click(await screen.findByRole("switch", { name: "Network writes" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Increase exposure?" });
  await user.click(within(dialog).getByRole("button", { name: "Apply anyway" }));

  await waitFor(() =>
    expect(reads).toEqual({ status: 2, flags: 2, tokens: 2, audit: 2, storage: 1 }),
  );
  expect(bodies).toEqual([
    { value: true, expected_version: 7 },
    { value: true, expected_version: 7, confirm: true },
  ]);
  expect(screen.getByRole("switch", { name: "Network writes" })).toHaveAttribute(
    "aria-checked",
    "true",
  );
});
