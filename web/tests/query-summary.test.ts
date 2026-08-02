import assert from "node:assert/strict";
import test from "node:test";
import type { Filter, QueryRequest, SourceInfo } from "../src/api/types.ts";
import { savedQuery, summarizeQuery } from "../src/lib/query-summary.ts";

const source = {
  id: "peer-1",
  name: "Studio peer",
} as SourceInfo;

test("saved-query summaries retain and explain every discovery scope", () => {
  const query: QueryRequest = {
    text: "kick",
    mode: "hybrid",
    filters: [
      { field: "source", op: "eq", value: { str: source.id } },
      { field: "folder", op: "eq", value: { str: "Drums/Kicks/" } },
      { field: "license", op: "eq", value: { str: "permissive" } },
      { field: "usage_right", op: "eq", value: { str: "commercial" } },
      { field: "tag", op: "eq", value: { str: "punchy" } },
      { field: "bpm", op: "range", value: { range: [100, 130] } },
    ],
    sort: { field: "size", dir: "desc" },
    page: { after: "opaque", limit: 24 },
    include_facets: true,
    include_total: true,
  };

  const saved = savedQuery(query);
  assert.equal(saved.page, undefined);
  assert.equal(saved.include_facets, undefined);
  assert.equal(saved.include_total, undefined);
  assert.deepEqual(saved.filters, query.filters);

  const summary = summarizeQuery(saved, [source]);
  assert.deepEqual(summary.warnings, []);
  assert.ok(summary.lines.includes("Keywords + similar: “kick”"));
  assert.ok(summary.lines.includes("Source is Studio peer"));
  assert.ok(summary.lines.includes("Folder is Drums/Kicks/"));
  assert.ok(summary.lines.includes("License is permissive"));
  assert.ok(summary.lines.includes("Usage right is commercial"));
  assert.ok(summary.lines.includes("Tag is punchy"));
  assert.ok(summary.lines.includes("Tempo is between 100 and 130"));
});

test("removed and incompatible facets degrade to visible warnings", () => {
  const removed = summarizeQuery(
    { filters: [{ field: "source", op: "eq", value: { str: "gone" } }] },
    [source],
  );
  assert.match(removed.warnings[0] ?? "", /no longer available/i);

  const incompatible = summarizeQuery({
    filters: [{ field: "removed_facet", op: "eq", value: { str: "x" } } as Filter],
  });
  assert.match(incompatible.warnings[0] ?? "", /not supported/i);
  assert.match(summarizeQuery(null).warnings[0] ?? "", /cannot be read/i);
});
