import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  API_V1_FIELDLESS_ENUMS,
  API_V1_REPRESENTATIVES,
  API_V1_VARIANT_EXAMPLES,
} from "../src/api/contract-fixtures.ts";

test("TypeScript wire fixtures match the committed Rust contract", async () => {
  const path = new URL("../../contracts/api-v1.json", import.meta.url);
  const contract = JSON.parse(await readFile(path, "utf8")) as {
    fieldless_enums: unknown;
    variant_examples: unknown;
    representatives: Record<string, unknown>;
  };

  assert.deepEqual(API_V1_FIELDLESS_ENUMS, contract.fieldless_enums);
  assert.deepEqual(API_V1_VARIANT_EXAMPLES, contract.variant_examples);
  for (const [name, value] of Object.entries(API_V1_REPRESENTATIVES)) {
    assert.deepEqual(value, contract.representatives[name]);
  }
});
