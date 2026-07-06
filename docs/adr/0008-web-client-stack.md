# ADR 0008 — Web-client stack: React + TypeScript + Tailwind

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Supersedes: — · Related: [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §10,
[tech-spec 09](../tech-spec/09-server-and-web-client.md) §B,
[PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §9, [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §4

## Context

The v1 UI ships as a **separate React + CSS web app** built before the desktop GUI, with WASM
only for viewer/render islands (PRODUCT_SPEC §7, the web-first front-end sequence in §9).
Tech-spec 09 §B.2 framed a candidate stack — Vite, a client router, a query/cache layer, CSS —
but left the exact choices "framed, not frozen" and carried them as an open question in
[09](../tech-spec/09-server-and-web-client.md#open-questions) and PRODUCT_SPEC §10. This ADR
freezes the load-bearing choices so the web-client build starts from a decided foundation rather
than re-litigating them.

## Decision

**Adopt React + TypeScript + Tailwind CSS as the web-client stack**, on the Vite/pnpm base
already framed in [tech-spec 09 §B.2](../tech-spec/09-server-and-web-client.md):

- **Framework: React** with **TypeScript** throughout (typed API client over the file-03
  endpoints; no untyped JS in the app).
- **Styling: Tailwind CSS** — the "small utility layer" §B.2 anticipated, now named. Dark-first,
  low-chrome, information-dense per [DESIGN_GUIDELINES §4](../DESIGN_GUIDELINES.md); one restrained
  accent driven through Tailwind theme tokens for selection/focus/primary. No heavyweight
  component framework — the design language is dense tables and tight grids, not card chrome.
- **Typography: a modern sans typeface** (e.g. Inter or a comparable variable font), self-hosted
  and shipped in the embedded bundle; the exact face is a design-build detail, but the direction
  is a clean, legible, modern type scale per the design guidelines — not a browser default stack.
- **Base tooling (from §B.2, unchanged): Vite** dev server/bundler, **pnpm**, a lightweight
  client router (URL owns view state), and a query/cache layer (e.g. TanStack Query) over the
  typed file-03 client with the `/api/ws` socket invalidating cached queries.

The `pnpm build → web/dist/` output is consumed by the `rust-embed` step ([09 §A.4](../tech-spec/09-server-and-web-client.md))
so the whole web client — React bundle **and** WASM viewer islands — ships in the one binary.

## Consequences

**Positive**
- Removes the biggest v1-UI unknown; the web build starts from a decided base and iterates on
  design, not stack selection.
- Tailwind's utility model + design tokens make the dense, dark-first, single-accent language of
  DESIGN_GUIDELINES §4 cheap to apply consistently and to keep consistent as the UI grows.
- TypeScript over the file-03 API client catches contract drift at the DOM boundary at compile
  time, before it reaches the running grid.

**Negative / risks**
- Tailwind utility classes can bloat markup; mitigated by extracting repeated patterns into
  components and `@apply` for the few genuinely shared primitives.
- A pinned typeface adds bundle weight; mitigated by subsetting and a single self-hosted variable
  font.

**Follow-ups (still open, not frozen by this ADR)**
- **WASM-island packaging & data handoff** (§B.3) — the wasm-bindgen boundary and WebGPU/WebGL2
  fallback remain open; owned by files 06/12/09, unaffected by the framework choice here.
- Exact router and query-layer packages are directional (§B.2); swappable within this decision.
- The concrete typeface and full type scale are settled during the design-build pass, against
  DESIGN_GUIDELINES §4.
