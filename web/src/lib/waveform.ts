/** A min/max pair for one horizontal waveform column. */
export interface WaveformRange {
  min: number;
  max: number;
}

/**
 * Reduce arbitrary mono samples to a fixed number of visible columns. When the canvas is wider
 * than the sample set, nearest buckets are repeated instead of producing empty columns. Keeping
 * this pure makes the lightweight waveform path testable without a DOM or GPU.
 */
export function reduceWaveform(
  samples: ArrayLike<number>,
  requestedColumns: number,
): WaveformRange[] {
  const columns = Math.max(1, Math.floor(requestedColumns));
  const ranges: WaveformRange[] = [];

  for (let column = 0; column < columns; column++) {
    if (samples.length === 0) {
      ranges.push({ min: 0, max: 0 });
      continue;
    }

    const start = Math.min(samples.length - 1, Math.floor((column * samples.length) / columns));
    const end = Math.min(
      samples.length,
      Math.max(start + 1, Math.ceil(((column + 1) * samples.length) / columns)),
    );
    let min = 1;
    let max = -1;
    for (let index = start; index < end; index++) {
      const sample = Math.max(-1, Math.min(1, Number(samples[index]) || 0));
      min = Math.min(min, sample);
      max = Math.max(max, sample);
    }
    ranges.push({ min, max });
  }

  return ranges;
}

/** Convert server-produced 0..1 peak buckets into the same signed input as decoded audio. */
export function expandSymmetricPeaks(peaks: readonly number[]): Float32Array {
  const samples = new Float32Array(peaks.length * 2);
  for (let index = 0; index < peaks.length; index++) {
    const peak = Math.max(0, Math.min(1, Number(peaks[index]) || 0));
    samples[index * 2] = peak === 0 ? 0 : -peak;
    samples[index * 2 + 1] = peak;
  }
  return samples;
}
