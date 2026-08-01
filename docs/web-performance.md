# Web bundle budgets and load measurements

The production client has three deliberate delivery tiers:

1. the application shell and authentication/status chrome;
2. route chunks, loaded only for the selected route;
3. the model-viewer JS glue and WASM, loaded only when a 3D preview is opened.

Audio waveforms are Canvas2D and use the peaks already present on an analysed asset. They must not
request `dam_viewer` JS or WASM. All generated files remain below `web/dist/`, are precompressed,
and are embedded by `dam-server`; no runtime asset comes from a CDN or external origin.

## Checked artifact budgets

`web/bundle-budgets.json` is the reviewable source of truth. `pnpm build` emits Vite's manifest,
precompresses the artifacts, and then runs `pnpm bundle:check`. The checker derives the initial
graph from synchronous manifest imports, treats every other JS file as lazy, verifies no WASM is in
the initial graph, and checks every JS/WASM artifact in raw and Brotli form. It also caps the total
initial JavaScript. CI and release both enter through `cargo xtask web`, so they cannot bypass it.

Raise a budget only with before/after measurements and an explanation in the pull request. A new
artifact is checked automatically; content-hashed filenames never need to be copied into config.

## Repeatable cold-load measurement

Build and serve the release assets with `cargo xtask web` followed by
`cargo run -p dam -- serve --addr 127.0.0.1:7333`. In a Chromium incognito window:

1. Open DevTools Network, enable Disable cache, choose the agreed network/CPU throttling profile,
   clear the log, and navigate directly to `/`.
2. Export a HAR and record Navigation Timing (`domContentLoadedEventEnd`, `loadEventEnd`), LCP,
   transferred bytes, and the initial JS filenames. There must be no route other than Workspace and
   no `.wasm` request.
3. Clear resource timings (`performance.clearResourceTimings()`), select an analysed audio asset,
   and record time until the waveform is visible plus transferred bytes. Confirm
   `performance.getEntriesByType("resource")` contains no `dam_viewer` or `.wasm` entry.
4. Reload cold, clear resource timings, select a 3D asset, and record time until the model is
   interactive. Record the lazy model-viewer JS and WASM transfer/response sizes separately.

Use three runs from a fresh incognito window and report the median, browser version, machine,
throttling profile, commit, and whether server peaks were present. Keep the HAR files as CI or pull
request artifacts when changing budgets; do not commit them to the repository.
