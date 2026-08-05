// The status bar's "time left" estimator (issue #168). It used to live inside `StatusBar.tsx` as a
// hook over two refs, so its behaviour could only be probed by rendering a job pill and feeding it
// props. As a pure step function over an explicit state, the interesting cases — smoothing, a
// stalled tick, a restarted job, an indeterminate phase — are direct assertions.

import assert from "node:assert/strict";
import test from "node:test";

import { fmtDuration, idleEta, stepEta, type EtaState } from "../src/lib/eta.ts";

/** Drive a sequence of (now, done) observations against one total, as the hook's effect would. */
function run(total: number | null, ticks: [number, number][]): { state: EtaState; eta: string | null } {
  let state = idleEta();
  let eta: string | null = null;
  for (const [now, done] of ticks) {
    const next = stepEta(state, now, done, total);
    eta = next.eta;
    state = { sample: next.sample, rate: next.rate };
  }
  return { state, eta };
}

test("the first observation cannot estimate; the second measures a rate", () => {
  const first = stepEta(idleEta(), 1_000, 10, 100);
  assert.equal(first.eta, null, "one sample is a position, not a speed");
  assert.equal(first.rate, 0);
  assert.deepEqual(first.sample, { t: 1_000, done: 10 });

  // 10 more items in 1s → 0.01 items/ms, 80 remaining → 8s left.
  const second = stepEta(first, 2_000, 20, 100);
  assert.equal(second.rate, 0.01);
  assert.equal(second.eta, "8s");
});

test("the rate is an EMA, so one fast interval only bends the estimate", () => {
  // 0.01 items/ms established, then an interval four times faster.
  const { state, eta } = run(1_000, [
    [0, 0],
    [1_000, 10],
    [2_000, 50],
  ]);
  // A raw last-interval rate would be 0.04 (→ 24s for the 950 left); the EMA keeps 60% of the
  // old one, so the estimate only comes down to 43s.
  assert.equal(state.rate, 0.01 * 0.6 + 0.04 * 0.4);
  assert.equal(eta, "43s");
});

test("a stalled or backwards tick keeps the last known rate rather than estimating forever", () => {
  const moving = run(100, [
    [0, 0],
    [1_000, 10],
  ]);
  // No progress: the rate is untouched and the estimate simply reflects the unchanged remainder.
  const stalled = stepEta(moving.state, 5_000, 10, 100);
  assert.equal(stalled.rate, moving.state.rate);
  assert.equal(stalled.eta, "9s");

  // A restarted job reports fewer items done; a naive delta would go negative and invert the ETA.
  const restarted = stepEta(stalled, 6_000, 2, 100);
  assert.equal(restarted.rate, stalled.rate);
  assert.equal(restarted.eta, "10s");

  // Two observations at the same instant would divide by zero.
  const sameInstant = stepEta(restarted, 6_000, 40, 100);
  assert.equal(sameInstant.rate, restarted.rate);
});

test("an indeterminate phase reports nothing and leaves the estimator untouched", () => {
  const measured = run(100, [
    [0, 0],
    [1_000, 10],
  ]).state;

  for (const total of [null, 0, -1]) {
    const step = stepEta(measured, 2_000, 20, total);
    assert.equal(step.eta, null);
    assert.deepEqual(step.sample, measured.sample, "an unknown total must not move the sample");
    assert.equal(step.rate, measured.rate, "nor poison the rate for a later determinate phase");
  }
});

test("a finished job stops predicting", () => {
  const { eta } = run(100, [
    [0, 0],
    [1_000, 50],
    [2_000, 100],
  ]);
  assert.equal(eta, null, "zero remaining is done, not '0s left'");
});

test("durations stay compact across the scales a scan actually spans", () => {
  assert.equal(fmtDuration(0), "0s");
  assert.equal(fmtDuration(999), "1s");
  assert.equal(fmtDuration(45_000), "45s");
  assert.equal(fmtDuration(60_000), "1m");
  assert.equal(fmtDuration(80_000), "1m 20s");
  assert.equal(fmtDuration(3_600_000), "1h 0m");
  assert.equal(fmtDuration(3_780_000), "1h 3m");
});
