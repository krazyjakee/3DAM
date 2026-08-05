// The Inspector's panels were extracted into `src/components/inspector/` (issue #166). The point of
// the split is that each panel is reachable on its own: one asset prop, its own query/mutation pair,
// no workspace and no Inspector shell around it. These tests mount the panels directly — if one ever
// grows a hidden dependency on its old parent, the mount here is what fails first.

import { HttpResponse, http } from "msw";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import { qk } from "../../src/api/queries";
import type { DupGroup, MediaAttributes, Page, SimilarHit } from "../../src/api/types";
import { CollectionsGroup } from "../../src/components/inspector/Collections";
import { DuplicatesSection } from "../../src/components/inspector/Duplicates";
import { LicenseSection } from "../../src/components/inspector/License";
import { MediaFacts } from "../../src/components/inspector/MediaFacts";
import { NoteEditor } from "../../src/components/inspector/NoteEditor";
import { SimilarSection } from "../../src/components/inspector/Similar";
import { TagList } from "../../src/components/inspector/Tags";
import { asset, assetSummary } from "./fixtures";
import { renderApp, testQueryClient } from "./render";
import { server } from "./server";

/** A client that answers every ambient query a panel makes (scopes, vocabulary), so a panel test
 *  only has to install handlers for the one endpoint it is actually about. */
function panelClient() {
  const client = testQueryClient();
  client.setQueryData([...qk.whoami, "", "session"], {
    identity: "test",
    scopes: ["read", "write", "admin", "mcp_use", "federate"],
    anonymous: false,
  });
  client.setQueryData([...qk.tags, "vocabulary", ""], []);
  return client;
}

/** The duplicate/similar tiles render real `Thumbnail`s, which fetch the server PNG. */
function serveThumbnails() {
  server.use(
    http.get("http://localhost/api/v1/assets/:id/thumbnail", () =>
      new HttpResponse(new Uint8Array([1, 2, 3]), { headers: { "content-type": "image/png" } }),
    ),
  );
}

test("media facts render from attributes alone — no query, no asset, no shell", () => {
  const audio: MediaAttributes = {
    media: "audio",
    duration_ms: 1500,
    sample_rate: 48000,
    bit_depth: 24,
    channels: 2,
    codec: "pcm",
    container: "wav",
    loudness_lufs: -14.2,
    brightness: 0.42,
    harmonicity: 0.75,
  };
  renderApp(<MediaFacts attrs={audio} />, { client: panelClient() });
  expect(screen.getByText("48000 Hz")).toBeVisible();
  expect(screen.getByText("Stereo")).toBeVisible();
  // The derived acoustic signals are their own group, not container metadata rows.
  expect(screen.getByText("Extracted features")).toBeVisible();
  expect(screen.getByText("-14.2 LUFS")).toBeVisible();
  expect(screen.getByText("Brightness")).toBeVisible();
  expect(screen.getByText("Harmonicity")).toBeVisible();
});

test("media facts cover every media arm, including the derived seamlessness block", () => {
  const cases: [MediaAttributes, string][] = [
    [
      {
        media: "image",
        width: 512,
        height: 512,
        color_depth: 8,
        has_alpha: false,
        color_space: "srgb",
        tileability: 0.93,
        tile_class: "seamless",
        repeat_period: 128,
      },
      "Seamlessness",
    ],
    [
      {
        media: "model",
        vertex_count: 2048,
        triangle_count: 1024,
        mesh_count: null,
        has_rig: true,
      },
      "Vertices",
    ],
    [
      { media: "video", duration_ms: 4000, width: 1920, height: 1080, fps: 29.97 },
      "Frame rate",
    ],
    [{ media: "document", title: "Readme", page_count: 3, word_count: null }, "Pages"],
  ];
  for (const [attrs, expected] of cases) {
    const { unmount } = renderApp(<MediaFacts attrs={attrs} />, { client: panelClient() });
    expect(screen.getByText(expected)).toBeVisible();
    unmount();
  }
  // "none" (and an unmeasured asset) contributes nothing at all rather than an empty group.
  const { container } = renderApp(<MediaFacts attrs={{ media: "none" }} />, {
    client: panelClient(),
  });
  expect(container).toBeEmptyDOMElement();
});

test("the similar panel offers analysis first, then ranks neighbours on demand", async () => {
  const user = userEvent.setup();
  const unanalyzed = asset({ timestamps: { created: 1, modified: 2, scanned: 3, analyzed: null } });
  const { unmount } = renderApp(<SimilarSection asset={unanalyzed} />, { client: panelClient() });
  // No vector yet: offer to analyze rather than query into the void.
  expect(screen.getByText("Analyze this asset to find visually similar ones.")).toBeVisible();
  unmount();

  const hits: Page<SimilarHit> = {
    items: [
      {
        asset: assetSummary({ id: "asset-b", name: "Albedo-2.png" }),
        score: 0.87,
        space: "v1@1+image+64+cosine",
      },
    ],
    cursor: null,
    total: 1,
    partial: { complete: true },
  };
  let searched = 0;
  serveThumbnails();
  server.use(
    http.post("http://localhost/api/v1/similar", () => {
      searched += 1;
      return HttpResponse.json(hits);
    }),
  );

  renderApp(<SimilarSection asset={asset()} />, { client: panelClient() });
  // Opt-in: an analyzed asset still doesn't search until asked.
  expect(searched).toBe(0);
  await user.click(screen.getByRole("button", { name: /Find similar/ }));
  const hit = await screen.findByRole("button", { name: /Albedo-2\.png/ });
  expect(hit).toHaveAttribute("title", expect.stringContaining("87% similar"));
  expect(within(hit).getByText("87%")).toBeVisible();
  await waitFor(() => expect(searched).toBe(1));
});

test("the duplicates panel enumerates the byte-identical set and stays absent without one", () => {
  const client = panelClient();
  client.setQueryData([...qk.duplicates, "asset", "asset-a"], null);
  const { unmount, container } = renderApp(<DuplicatesSection asset={asset()} />, { client });
  expect(container).toBeEmptyDOMElement();
  unmount();

  const group: DupGroup = {
    kind: "exact",
    media: "image",
    group: "hash-1",
    review: "review-1",
    review_state: "pending",
    members: [
      { asset: assetSummary(), path: "a/Albedo.png", source: "source-a" },
      {
        asset: assetSummary({ id: "asset-b", name: "Albedo copy.png" }),
        path: "b/Albedo copy.png",
        source: "source-a",
      },
    ],
    total_members: 2,
    members_cursor: null,
    signal: "identical content hash",
    suggested_keep: "asset-a",
    suggested_keep_reason: "largest",
  };
  const withGroup = panelClient();
  withGroup.setQueryData([...qk.duplicates, "asset", "asset-a"], group);
  serveThumbnails();
  renderApp(<DuplicatesSection asset={asset()} />, { client: withGroup });
  expect(screen.getByText("Duplicates (1)")).toBeVisible();
  expect(screen.getByText(/1 byte-identical copy/)).toBeVisible();
  expect(screen.getByRole("button", { name: /Albedo copy\.png/ })).toBeVisible();
  expect(screen.getByText("Keep")).toBeVisible();
});

test("the collections panel lists memberships and offers the manual ones to add", () => {
  const client = panelClient();
  client.setQueryData(qk.collections, [
    { id: "col-a", name: "Rock kit", kind: "manual", count: 3, created_at: 1, updated_at: 1 },
    { id: "col-b", name: "Recent PNGs", kind: "smart", count: 9, created_at: 1, updated_at: 1 },
    { id: "col-c", name: "Hero props", kind: "manual", count: 0, created_at: 1, updated_at: 1 },
  ]);
  renderApp(<CollectionsGroup asset={asset({ collections: ["col-a", "col-b"] })} />, { client });
  expect(screen.getByRole("button", { name: "Remove from Rock kit" })).toBeVisible();
  // A smart folder's membership is query-driven, so it gets no remove control.
  expect(screen.queryByRole("button", { name: "Remove from Recent PNGs" })).toBeNull();
  expect(screen.getByRole("option", { name: "Hero props" })).toBeInTheDocument();
});

test("the note, tag, and licence panels each mount on their own from one asset prop", () => {
  const client = panelClient();
  const detail = asset({
    note: { body: "Recorded on the roof.", updated_at: 5, updated_by: "jake" },
    tags: [{ name: "brick", state: "confirmed", source: "user", confidence: null }],
  });
  renderApp(
    <>
      <NoteEditor asset={detail} />
      <TagList assetId="asset-a" tags={detail.tags} origin="local" />
      <LicenseSection asset={detail} />
    </>,
    { client },
  );
  expect(screen.getByLabelText("Asset note")).toHaveValue("Recorded on the roof.");
  expect(screen.getByText("brick")).toBeVisible();
  expect(screen.getByRole("button", { name: "Edit licence" })).toBeEnabled();
});
