# ADR 0012 — Asset transport: keep bytes on HTTP, warm the cache with a prefetch hint

Status: **Accepted** · Date: 2026-07-10 · Deciders: 3DAM core
Supersedes: — · Related: [ADR 0008](0008-web-client-stack.md), [tech-spec 09](../tech-spec/09-server-and-web.md), issues [#69](https://github.com/krazyjakee/3DAM/issues/69) (hosted-mode epic), [#71](https://github.com/krazyjakee/3DAM/issues/71) (background pipeline), [#72](https://github.com/krazyjakee/3DAM/issues/72) (this decision)

## Context

Hosted mode (milestone 7) has a client hold one open socket to an authoritative `3dam serve` and
receive "all assets and data through it for the fastest possible transfer." Today the WebSocket
(`/api/v1/ws`) carries **control-plane `LibraryEvent`s only**; every thumbnail, full content blob,
and preview mesh is a **separate HTTP GET** (`crates/3dam-server/src/lib.rs`). A fresh 100k-asset
grid therefore issues one GET per visible tile.

The question issue #72 poses: should the **bytes** move onto the socket — binary frames multiplexed
over the existing WS — to cut time-to-first-full-grid, or is HTTP already good enough?

## Spike: what actually limits time-to-first-full-grid

We compared three transports for the grid workload (many small-to-medium thumbnails; a few larger
preview meshes on demand):

1. **HTTP/1.1 + cache (today).** Correct, browser-native caching (content-hash keyed, `immutable`
   for `assets/*`), but limited to ~6 concurrent connections per origin — the classic head-of-line
   limit on a wide grid.
2. **HTTP/2 keep-alive multiplexing.** One connection, many concurrent streams with prioritisation,
   header compression, and the *same* browser cache + range/streaming semantics for free. For bulk
   bytes this is at or near the practical ceiling: the wire is saturated and the client caches
   without any bespoke code.
3. **Binary frames multiplexed over the WebSocket.** A single WS is **one ordered stream** — pushing
   N thumbnails through it reintroduces head-of-line blocking that HTTP/2 specifically removes, and
   it needs a hand-written client-side cache (blob-URL map, eviction, revalidation) to replace what
   the browser's HTTP cache does natively. It also bypasses `Cache-Control`/`ETag`, so a second view
   re-transfers everything.

The honest finding (anticipated in the issue): **for moving bytes, HTTP/2 wins and byte-push over a
single socket is a regression.** The real lever on time-to-first-full-grid is not the transport of
the bytes but **whether the derivative exists when the client asks** — i.e. cache warmth. With
lazy-on-request generation, the first client to view an asset paid the render latency inline; that,
not the byte transfer, dominated the cold grid.

## Decision

**Keep asset bytes on HTTP (HTTP/2 where the deployment terminates it); do not move them onto the
WebSocket. Attack latency by warming the cache ahead of render, two ways:**

1. **Proactive generation on ingest** — the background job pipeline (ADR-adjacent, issue #71)
   pre-renders thumbnails + preview meshes and runs analysis when a scan adds an asset, so a freshly
   connected client mostly hits a warm cache with no interaction at all.

2. **A prefetch hint** (issue #72) — a lightweight, fire-and-forget signal, **not the bytes**, on the
   existing request/response seam (`LibraryService::prefetch` → `POST /api/v1/prefetch`). A client
   names the assets (and thumbnail edge) it is about to render; the server warms exactly those
   derivatives so the following HTTP GETs are cache hits. It carries ids, not pixels, so it stays
   tiny and needs no client-side byte cache — the browser's HTTP cache still owns the bytes. Both
   clients call it from their grid data layer as each page loads; a miss degrades to the normal
   on-demand GET (HTTP fallback is inherent).

The WebSocket stays a **control-plane** channel (events + the prefetch signal's natural home is the
same request seam, kept there for automatic web-and-native parity since a browser's receive-only WS
can't easily send). No bespoke binary framing, no second cache.

## Consequences

- **Bytes keep every HTTP affordance**: `Cache-Control`/`ETag`, range requests, streaming, and the
  content-hash `immutable` caching that makes repeat views free — none of which a WS byte-push keeps.
- **Time-to-first-full-grid is bounded by cache warmth, which we now control** — the pipeline warms
  on ingest, prefetch warms the exact edge/assets about to show. On a warm library the grid is
  first-paint-limited, not render-limited.
- **Parity is free**: prefetch rides `LibraryService`, so embedded GUI, connected GUI, and web all
  get it through the one seam; the embedded engine warms its own cache, `ApiClient` POSTs the hint.
- **Small over-fetch risk**: prefetch may warm an edge the grid then doesn't request (the web tile
  edge is size-derived). Warming is idempotent and cheap (a cache check), and exact-edge alignment is
  a future refinement, not a correctness issue.
- **Revisit if** a deployment can't put HTTP/2 in front (e.g. plaintext HTTP/1.1 with no proxy) *and*
  profiles show the 6-connection limit dominating — then a QUIC/HTTP/3 front or targeted byte-push
  for the largest assets could be reconsidered, with this ADR as the baseline it must beat.
