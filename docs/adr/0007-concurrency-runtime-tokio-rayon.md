# ADR 0007 — Concurrency runtime: tokio (async I/O) + rayon (CPU)

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Supersedes: — · Related: [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §7, §8,
[tech-spec 14](../tech-spec/14-concurrency-performance-reliability.md),
[DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §1.1

## Context

3DAM targets 60 fps browsing at 100k+ visible-scale, instant search, and analysis that
saturates cores without blocking the UI, designed for 1M+ assets (PRODUCT_SPEC §8,
DESIGN_GUIDELINES §1.1). The work splits cleanly into two profiles: **I/O-bound, high-cardinality
waiting** (source access over local FS / SFTP / SMB, federated peer queries, the HTTP/WS server,
SQLite) and **CPU-bound, core-saturating compute** (decode, hashing, DSP, embedding inference,
thumbnail/convert transcode).

PRODUCT_SPEC §7 named an **async runtime on `tokio`** for I/O and **`rayon`** for CPU-parallel
analysis; tech-spec 14 built the whole execution model on that split (the `cpu()` hand-off, the
bounded `Stage<I,O>` pools, cancellation, out-of-core access). This was the working direction but
was never ratified, and a throughput-validation spike was floated.

## Decision

**Adopt `tokio` (async I/O runtime) + `rayon` (CPU pool) as the concurrency foundation**, exactly
as modelled in [tech-spec 14](../tech-spec/14-concurrency-performance-reliability.md). We ratify
the model rather than gating it on a dedicated throughput spike; scale is validated by the
benchmark harness in [tech-spec 15](../tech-spec/15-observability-config-testing-packaging.md),
not by a go/no-go experiment.

Rules (from tech-spec 14, now normative):

- **tokio** owns everything that mostly *waits*: source I/O, federated fan-out, the axum
  server, and all SQLite access. Thousands can be in flight cheaply.
- **rayon** owns everything that *computes* and wants all cores. CPU work **never** runs on a
  tokio worker (a 200 ms decode on an async worker stalls every future sharing that thread).
- **Async→CPU hand-off** is a one-shot round-trip: the tokio task parks on `rx.await` (yielding
  its worker) while rayon runs the closure and returns the result (tech-spec 14 §hand-off).
- Both live in **`3dam-core`** — the only crate allowed to depend on `tokio` *and* `rayon`;
  front-ends never see them ([tech-spec 01](../tech-spec/01-architecture-and-crates.md)).

## Consequences

**Positive**
- One well-trodden pairing (tokio + rayon) with mature ecosystems; axum/hyper (server) and most
  I/O crates are already tokio-native, so no runtime-bridging friction.
- The two-pool split directly enforces "heavy work never blocks the UI/async path"
  (DESIGN_GUIDELINES §1.1) by construction.
- tech-spec 14's primitives (`Stage`, `cpu()`, cancellation tokens) become the standard the
  media/analysis/convert/source files (04/05/07/08) schedule onto.

**Negative / risks**
- **Two runtimes to reason about.** The discipline "CPU work off tokio workers, always via the
  hand-off" must be enforced — a stray blocking call on an async worker is the classic footgun.
  Guard with review and, where feasible, lints/tests.
- Bridging cost between the pools exists but is negligible versus the work sizes (ms-scale
  compute vs µs-scale hand-off).

**Follow-ups**
- The scan→analyse **throughput/backpressure benchmark** (tech-spec 15 harness) becomes a
  standing perf-regression guard, not a one-off spike.
- Confirm pool sizing / byte-budget knobs (tech-spec 14) against real large, messy libraries.
