// The status bar's "time left" estimator (issue #168, parent #96) — lifted out of `StatusBar.tsx`.
//
// StatusBar itself is deliberately *not* decomposed: it is already thirteen small chip components,
// and splitting them into thirteen files would be import churn for nothing. This is the one part
// that was neither a chip nor declarative — an EMA rate estimator whose whole behaviour (does a
// stalled job keep its last estimate? does a restarted job forget the old rate?) lived inside a
// `useRef` and could only be exercised by rendering a job pill and driving props at it.
//
// So the arithmetic is a pure step function over an explicit state value, and `useEta` is the thin
// React wrapper that carries that state in refs. The estimator is unit-tested directly by the node
// runner (`tests/eta.test.ts`); the wrapper has nothing left to get wrong.

import { useEffect, useRef, useState } from "react";

/** One (wall-clock, items-done) observation of a running job. */
export interface EtaSample {
  t: number;
  done: number;
}

/** Everything the estimator remembers between observations. */
export interface EtaState {
  /** The previous observation, or null before the first one (or after a reset). */
  sample: EtaSample | null;
  /** Smoothed throughput in items per millisecond; 0 until two observations show progress. */
  rate: number;
}

/** The state a fresh — or newly reset — estimator starts from. */
export function idleEta(): EtaState {
  return { sample: null, rate: 0 };
}

/** Weight kept from the running average when a new instantaneous rate arrives. Smoothing matters
 *  because per-item work is wildly uneven (a 4K texture next to a 2KB README), so a raw
 *  last-interval rate would make the estimate jitter by minutes between ticks. */
const EMA_KEEP = 0.6;

/** Fold one observation into the estimator: returns the next state plus the "time left" string to
 *  display, or `null` when nothing can honestly be estimated — an unknown/zero total, no measured
 *  throughput yet, or a job that has already finished.
 *
 *  A total that isn't a positive count leaves the state untouched: an indeterminate job must not
 *  poison the rate for the determinate phase that may follow. */
export function stepEta(
  prev: EtaState,
  now: number,
  done: number,
  total: number | null,
): EtaState & { eta: string | null } {
  if (total == null || total <= 0) return { ...prev, eta: null };
  let rate = prev.rate;
  const last = prev.sample;
  // Only a strictly forward step in both time and progress carries information. Equal timestamps
  // would divide by zero, and a `done` that went backwards (a job restarting) would go negative.
  if (last && now > last.t && done > last.done) {
    const inst = (done - last.done) / (now - last.t);
    rate = rate === 0 ? inst : rate * EMA_KEEP + inst * (1 - EMA_KEEP);
  }
  const remaining = Math.max(0, total - done);
  return {
    sample: { t: now, done },
    rate,
    eta: remaining > 0 && rate > 0 ? fmtDuration(remaining / rate) : null,
  };
}

/** ms → a compact "1m 20s" / "45s" / "1h 3m" duration. */
export function fmtDuration(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) {
    const rem = s % 60;
    return rem ? `${m}m ${rem}s` : `${m}m`;
  }
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

/** Live ETA from throughput: tracks `done` over wall-clock time (per `key`), smooths the rate with
 *  an EMA, and returns a formatted "time left" string — or null when it can't estimate (unknown
 *  total, no progress yet, or done). */
export function useEta(key: string, done: number, total: number | null): string | null {
  const [eta, setEta] = useState<string | null>(null);
  const state = useRef<EtaState>(idleEta());

  useEffect(() => {
    // Reset the estimator when the tracked job/aggregate identity changes.
    state.current = idleEta();
    setEta(null);
  }, [key]);

  useEffect(() => {
    const { eta: next, ...rest } = stepEta(state.current, Date.now(), done, total);
    state.current = rest;
    setEta(next);
  }, [done, total]);

  return eta;
}
