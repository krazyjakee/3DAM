import assert from "node:assert/strict";
import test from "node:test";
import { expandSymmetricPeaks, reduceWaveform } from "../src/lib/waveform.ts";

test("reduces samples to stable min/max columns", () => {
  assert.deepEqual(reduceWaveform(new Float32Array([-1, 0.25, -0.5, 1]), 2), [
    { min: -1, max: 0.25 },
    { min: -0.5, max: 1 },
  ]);
});

test("repeats sparse samples across a wider canvas without empty columns", () => {
  assert.deepEqual(reduceWaveform(new Float32Array([-0.75, 0.5]), 4), [
    { min: -0.75, max: -0.75 },
    { min: -0.75, max: -0.75 },
    { min: 0.5, max: 0.5 },
    { min: 0.5, max: 0.5 },
  ]);
});

test("clamps untrusted server peaks into signed waveform samples", () => {
  const samples = expandSymmetricPeaks([-1, 0.4, 2]);
  assert.deepEqual([samples[0], samples[1], samples[4], samples[5]], [0, 0, -1, 1]);
  assert.ok(Math.abs(samples[2] + 0.4) < 0.000_001);
  assert.ok(Math.abs(samples[3] - 0.4) < 0.000_001);
});
