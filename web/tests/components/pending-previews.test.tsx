import { HttpResponse, http } from "msw";
import { act, screen, waitFor } from "@testing-library/react";
import { expect, test } from "vitest";
import type { AssetSummary } from "../../src/api/types";
import { Thumbnail } from "../../src/components/Thumbnail";
import { Preview } from "../../src/components/inspector/preview";
import { asset, assetSummary } from "./fixtures";
import { renderApp } from "./render";
import { server } from "./server";

const pendingAttrs = { ingest_status: "pending_verification" };

test("pending thumbnails do not fetch and resume when verification finishes", async () => {
  let requests = 0;
  server.use(
    http.get("http://localhost/api/v1/assets/:id/thumbnail", () => {
      requests += 1;
      return new HttpResponse(new Uint8Array([1, 2, 3]), {
        headers: { "content-type": "image/png" },
      });
    }),
  );
  const ready = assetSummary();
  const pending = assetSummary({ key_attrs: pendingAttrs });
  const { container, rerender } = renderApp(<Thumbnail asset={pending} />);
  expect(screen.getByText("Awaiting verification")).toBeVisible();
  expect(container.querySelector("img")).toBeNull();
  await act(async () => {});
  expect(requests).toBe(0);

  rerender(<Thumbnail asset={ready} />);
  await waitFor(() => expect(requests).toBe(1));
  expect(screen.queryByText("Awaiting verification")).toBeNull();

  // A newly queued revision also removes an already mounted thumbnail immediately.
  rerender(<Thumbnail asset={pending} />);
  expect(screen.getByText("Awaiting verification")).toBeVisible();
  expect(container.querySelector("img")).toBeNull();
  await act(async () => {});
  expect(requests).toBe(1);
});

test("pending inspector previews never fetch content or mint streaming tickets", async () => {
  let contentRequests = 0;
  let ticketRequests = 0;
  server.use(
    http.get("http://localhost/api/v1/assets/:id/content", () => {
      contentRequests += 1;
      return new HttpResponse(new Uint8Array([1]));
    }),
    http.get("http://localhost/api/v1/assets/:id/preview-mesh", () => {
      contentRequests += 1;
      return new HttpResponse(new Uint8Array([1]));
    }),
    http.post("http://localhost/api/v1/media-ticket", () => {
      ticketRequests += 1;
      return HttpResponse.json({ ticket: "unexpected" });
    }),
  );
  const media: [AssetSummary["media"], string][] = [
    ["image", "png"],
    ["audio", "wav"],
    ["video", "mp4"],
    ["model", "glb"],
    ["document", "pdf"],
  ];
  for (const [kind, format] of media) {
    const pending = asset({
      summary: assetSummary({ media: kind, format, key_attrs: pendingAttrs }),
    });
    const { container, unmount } = renderApp(<Preview asset={pending} />);
    expect(screen.getByText("Awaiting verification")).toBeVisible();
    expect(container.querySelector("img, audio, video, canvas")).toBeNull();
    expect(screen.queryByRole("link", { name: "Open original" })).toBeNull();
    await act(async () => {});
    unmount();
  }
  expect(contentRequests).toBe(0);
  expect(ticketRequests).toBe(0);
});
