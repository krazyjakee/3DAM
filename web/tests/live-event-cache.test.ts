import assert from "node:assert/strict";
import test from "node:test";

import { QueryClient } from "@tanstack/react-query";
import { LiveEventCacheBatcher } from "../src/api/live-event-cache.ts";
import type { JobStatus, LibraryEvent } from "../src/api/types.ts";

function recordingClient(): { client: QueryClient; invalidations: string[] } {
  const client = new QueryClient();
  const invalidations: string[] = [];
  const invalidate = client.invalidateQueries.bind(client);
  client.invalidateQueries = ((filters, options) => {
    invalidations.push(String(filters?.queryKey?.[0]));
    return invalidate(filters, options);
  }) as QueryClient["invalidateQueries"];
  return { client, invalidations };
}

const changed: LibraryEvent = {
  type: "asset_changed",
  id: "asset-1",
  source_id: "source-1",
  kind: "metadata",
};

function job(state: JobStatus["state"], done: number): LibraryEvent {
  return {
    type: "job_progress",
    id: "job-1",
    kind: "scan",
    state,
    progress: { done, total: 100_000, current: null },
    error: null,
    sources: ["source-1"],
  };
}

test("100k asset events produce one trailing refresh per query family", async () => {
  const { client, invalidations } = recordingClient();
  const batch = new LiveEventCacheBatcher(client, 60_000);

  for (let index = 0; index < 100_000; index += 1) batch.handle(changed);
  await batch.flush();

  assert.deepEqual(invalidations.sort(), [
    "asset",
    "assets",
    "comments",
    "duplicates",
    "folders",
    "similar",
    "stats",
  ]);
  batch.dispose();
});

test("a slow job batch patches progress and refreshes once at its terminal boundary", async () => {
  const { client, invalidations } = recordingClient();
  const batch = new LiveEventCacheBatcher(client, 60_000);
  client.setQueryData(["jobs", {}], {
    items: [job("queued", 0)],
    cursor: null,
    total: 1,
    partial: { complete: true },
  });

  batch.handle(job("running", 1));
  for (let index = 0; index < 100_000; index += 1) batch.handle(changed);
  assert.equal(invalidations.length, 0, "an active bulk job must not refetch per asset");

  await batch.handle(job("done", 100_000));
  const cached = client.getQueryData<{ items: JobStatus[] }>(["jobs", {}]);
  assert.equal(cached?.items[0]?.state, "done");
  assert.equal(cached?.items[0]?.progress.done, 100_000);
  assert.equal(invalidations.filter((family) => family === "jobs").length, 1);
  assert.equal(invalidations.filter((family) => family === "assets").length, 1);
  assert.equal(new Set(invalidations).size, invalidations.length);
  batch.dispose();
});

test("safe row patches remove only local assets and update existing source state", () => {
  const { client } = recordingClient();
  const batch = new LiveEventCacheBatcher(client, 60_000);
  const local = { id: "same-id", name: "local", origin: "local" };
  const peer = { id: "same-id", name: "peer", origin: { peer: "remote" } };
  client.setQueryData(["assets", "fixture"], {
    pages: [
      { items: [local, peer], cursor: null, total: 2, partial: { complete: true } },
    ],
    pageParams: [null],
  });
  client.setQueryData(["sources"], [
    { id: "source-1", name: "Fixture", state: "online" },
  ]);
  client.setQueryData(["asset", "local-or-legacy", "same-id"], { id: "same-id" });
  client.setQueryData(["asset", "source-1", "same-id"], { id: "same-id" });
  client.setQueryData(["asset", "other-source", "same-id"], { id: "same-id" });

  batch.handle({ type: "asset_removed", id: "same-id", source_id: "source-1" });
  batch.handle({ type: "source_state", id: "source-1", state: "offline" });

  const assets = client.getQueryData<{ pages: { items: typeof peer[]; total: number }[] }>([
    "assets",
    "fixture",
  ]);
  const sources = client.getQueryData<{ id: string; state: string }[]>(["sources"]);
  assert.deepEqual(assets?.pages[0]?.items, [peer]);
  assert.equal(assets?.pages[0]?.total, 1);
  assert.equal(sources?.[0]?.state, "offline");
  assert.equal(client.getQueryData(["asset", "local-or-legacy", "same-id"]), undefined);
  assert.equal(client.getQueryData(["asset", "source-1", "same-id"]), undefined);
  assert.deepEqual(client.getQueryData(["asset", "other-source", "same-id"]), {
    id: "same-id",
  });
  batch.dispose();
});

test("asset additions replace an already-cached local summary without inserting new rows", () => {
  const { client } = recordingClient();
  const batch = new LiveEventCacheBatcher(client, 60_000);
  const oldLocal = { id: "asset-1", name: "old", origin: "local" };
  const peer = { id: "asset-1", name: "peer", origin: { peer: "remote" } };
  client.setQueryData(["assets", "fixture"], {
    pages: [{ items: [oldLocal, peer], cursor: null, total: 2, partial: { complete: true } }],
    pageParams: [null],
  });

  batch.handle({
    type: "asset_added",
    id: "asset-1",
    name: "fresh",
    media: "image",
    format: "png",
    size: 42,
    license: { id: null, status: "unknown" },
    top_tags: [],
    origin: "local",
    key_attrs: {},
    favorite: false,
    source_id: "source-1",
  });
  batch.handle({
    type: "asset_added",
    id: "not-cached",
    name: "must not be inserted",
    media: "image",
    format: "png",
    size: 1,
    license: { id: null, status: "unknown" },
    top_tags: [],
    origin: "local",
    key_attrs: {},
    favorite: false,
    source_id: "source-1",
  });

  const cached = client.getQueryData<{ pages: { items: { name: string }[] }[] }>([
    "assets",
    "fixture",
  ]);
  assert.deepEqual(cached?.pages[0]?.items.map((item) => item.name), ["fresh", "peer"]);
  assert.equal(cached?.pages[0]?.items.length, 2, "an unknown result cannot be inserted safely");
  batch.dispose();
});

test("job progress merges into durable detail without erasing result data", () => {
  const { client } = recordingClient();
  const batch = new LiveEventCacheBatcher(client, 60_000);
  client.setQueryData(["jobs", "detail", "job-1"], {
    ...job("queued", 0),
    type: undefined,
    result: { kind: "export", report: { files: 3 } },
    result_artifacts: [{ label: "manifest" }],
  });

  batch.handle(job("running", 7));

  const detail = client.getQueryData<Record<string, unknown>>(["jobs", "detail", "job-1"]);
  assert.deepEqual(detail?.result, { kind: "export", report: { files: 3 } });
  assert.deepEqual(detail?.result_artifacts, [{ label: "manifest" }]);
  assert.deepEqual(detail?.progress, { done: 7, total: 100_000, current: null });
  batch.dispose();
});

test("multiple active jobs defer catalog refresh until the last terminal boundary", async () => {
  const { client, invalidations } = recordingClient();
  const batch = new LiveEventCacheBatcher(client, 60_000);
  client.setQueryData(["jobs", {}], {
    items: [job("queued", 0), { ...job("queued", 0), id: "job-2" }],
    cursor: null,
    total: 2,
    partial: { complete: true },
  });

  batch.handle(job("running", 1));
  batch.handle({ ...job("running", 1), id: "job-2" });
  batch.handle(changed);
  await batch.handle(job("done", 100_000));
  assert.equal(invalidations.length, 0, "the second active job still owns the batch boundary");

  await batch.handle({ ...job("done", 100_000), id: "job-2" });
  assert.equal(invalidations.filter((family) => family === "assets").length, 1);
  assert.equal(invalidations.filter((family) => family === "jobs").length, 1);
  batch.dispose();
});

test("a newly observed external active job gets one bounded jobs-only refresh", async () => {
  const { client, invalidations } = recordingClient();
  const batch = new LiveEventCacheBatcher(client, 60_000);

  batch.handle(job("running", 1));
  await batch.flushJobs();

  assert.deepEqual(invalidations, ["jobs"]);
  batch.dispose();
});

test("lag forces one full resync and dispose suppresses pending work", async () => {
  const { client, invalidations } = recordingClient();
  const batch = new LiveEventCacheBatcher(client, 60_000);
  batch.handle(changed);
  await batch.handle({ type: "stream_lagged" });
  assert.deepEqual(new Set(invalidations), new Set([
    "assets",
    "stats",
    "sources",
    "jobs",
    "duplicates",
    "asset",
    "comments",
    "similar",
    "folders",
  ]));

  const count = invalidations.length;
  batch.handle(changed);
  batch.dispose();
  await batch.flush();
  assert.equal(invalidations.length, count);
});

test("resync forgets jobs that may have ended inside the stream gap", async () => {
  const { client, invalidations } = recordingClient();
  const batch = new LiveEventCacheBatcher(client, 1);
  client.setQueryData(["jobs", {}], {
    items: [job("queued", 0)],
    cursor: null,
    total: 1,
    partial: { complete: true },
  });
  batch.handle(job("running", 1));
  await batch.resync();
  invalidations.length = 0;

  batch.handle(changed);
  await new Promise((resolve) => setTimeout(resolve, 10));

  assert.ok(invalidations.includes("assets"), "a stale active-job id must not suppress the timer");
  batch.dispose();
});
