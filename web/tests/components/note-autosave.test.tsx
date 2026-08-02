import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, expect, test, vi } from "vitest";
import { qk } from "../../src/api/queries";
import { Inspector } from "../../src/components/Inspector";
import { asset } from "./fixtures";
import { renderApp, testQueryClient } from "./render";
import { server } from "./server";

afterEach(() => vi.useRealTimers());

test("asset notes save once after a typing pause and flush immediately on blur", async () => {
  const storedAsset = asset();
  const client = testQueryClient();
  client.setQueryData(qk.asset("asset-a"), storedAsset);
  client.setQueryData(qk.version, {
    api: "v1",
    server: "test",
    capabilities: [],
    auth: "off",
    accounts: false,
  });
  client.setQueryData([...qk.whoami, "", "session"], {
    identity: "test",
    scopes: ["read", "write", "admin", "mcp_use", "federate"],
    anonymous: false,
  });
  client.setQueryData(qk.sources, []);
  client.setQueryData(qk.collections, []);
  client.setQueryData([...qk.duplicates, "asset", "asset-a"], null);

  const saves: { id: string; body: unknown }[] = [];
  server.use(
    http.post("http://localhost/api/v1/tags/list", () => HttpResponse.json([])),
    http.get("http://localhost/api/v1/assets/asset-a/content", () =>
      new HttpResponse(new Uint8Array([1, 2, 3]), {
        headers: { "content-type": "image/png" },
      }),
    ),
    http.put("http://localhost/api/v1/assets/:id/note", async ({ params, request }) => {
      const payload = (await request.json()) as { body: unknown };
      saves.push({ id: String(params.id), body: payload.body });
      return HttpResponse.json({ body: payload.body, updated_at: 10, updated_by: "tester" });
    }),
  );

  renderApp(
    <Inspector
      open={false}
      onClose={() => {}}
      collapsed={false}
      onCollapse={() => {}}
      onExpand={() => {}}
    />,
    { route: "/?sel=asset-a", client },
  );
  const editor = screen.getByRole("textbox", { name: "Asset note" });

  vi.useFakeTimers();
  fireEvent.change(editor, { target: { value: "First draft" } });
  await act(() => vi.advanceTimersByTimeAsync(699));
  expect(saves).toHaveLength(0);
  await act(() => vi.advanceTimersByTimeAsync(1));
  vi.useRealTimers();
  await waitFor(() => expect(saves).toEqual([{ id: "asset-a", body: "First draft" }]));

  fireEvent.change(editor, { target: { value: "Saved on blur" } });
  fireEvent.blur(editor);
  await waitFor(() =>
    expect(saves).toEqual([
      { id: "asset-a", body: "First draft" },
      { id: "asset-a", body: "Saved on blur" },
    ]),
  );
});
