# Reproducible scale and performance harness

The scale harness turns the 100,000- and 1,000,000-asset targets into repeatable regression checks without committing a generated catalog. It creates a migrated SQLite library and a profile-sized derivative-cache tree under `target/`, measures store, derivative-cache startup, and browser browse-window behavior, compares the result with a profile-specific ratio baseline, writes JSON, and removes the fixtures.

## Running it

Run the bounded local/PR profile from the repository root:

```sh
cargo run --locked -p xtask -- perf --profile smoke
```

Run the representative profiles with optimized harness code:

```sh
cargo run --release --locked -p xtask -- perf --profile 100k
cargo run --release --locked -p xtask -- perf --profile 1m
```

Reports default to `perf-results/<profile>.json`; catalogs default to `target/perf-catalog-<profile>` and are deleted after the report is written. `--output`, `--work-dir`, `--baseline`, and `--profiles` override those paths. `--keep-catalog` retains a catalog for investigation. `--skip-browser` is for store-only diagnosis and records an explicit skipped browser status; CI never uses it.

## Deterministic fixture

[`perf/profiles.json`](../perf/profiles.json) is a strict, versioned profile schema. Every profile uses the fixed seed and fixture recipe version recorded in its report. The generator first opens the directory through `Store::open`, applying the supported forward migrations, then populates the public schema in 10,000-row transactions using the versioned recipe. That boundary keeps million-row setup practical while making schema drift fail visibly.

The loader temporarily suspends only the migrated `folder_` triggers while inserting deterministic
assets, rebuilds the folder read model once with V20's canonical recursive CTE, and restores the
exact trigger definitions before any measurement. Production scans still exercise per-asset folder
maintenance; bulk fixture construction avoids millions of redundant ancestor walks. V26 aggregate
triggers remain enabled throughout generation so their production write cost is represented.

The fixture contains local filesystem, SFTP, SMB, and federated sources; all five media types; paths up to the configured depth; confirmed tags; normalized embeddings; and repeatable exact-duplicate groups. Names include a stable search term. It also creates one tiny cached-thumbnail entry per profile asset in the production flat cache layout. The generated database, WAL, and cache tree never enter git.

## Measurements and reports

The harness measures median first-page, late-page, lexical-search, faceted-query, library-stats,
and confirmed-tag-facet latency; analysis-planning and exact-duplicate latency; scan/upsert and
bounded export throughput; browse latency sampled during that write workload; process peak RSS; and
the serialized initial browse payload. The stats
and tag-facet budgets are intentionally identical for smoke, 100k, and 1M profiles: growth in asset
rows cannot buy a looser threshold, so a return to catalog scans fails the scale profile.

Write evidence has three complementary measures. `scan_upsert_assets_per_second` exercises the real
production upsert path with aggregate triggers enabled. `aggregate_write_overhead_ratio` compares
an aggregate-relevant no-op asset update with an otherwise identical indexed no-op update against
the production schema, in rolled-back transactions over deterministic fixture ids; this isolates
the maintained-count trigger cost without changing the measured catalog.

`browse_under_write_ms` is the reader half of that same workload, and the regression guard for
[issue #137](https://github.com/krazyjakee/3DAM/issues/137)'s one-writer/pooled-readers connection
ownership. A sampler thread browses the catalog once per millisecond for as long as the upsert loop
above runs — the same writes, not a second workload — issuing the default first page with
`include_total: false`, so the sample is a keyset page fetch whose cost is a function of page size
rather than of catalog size. Two consequences follow. The measurement is charged honestly: the
sampler's contention is included in `scan_upsert_assets_per_second`, because a scan rate that only
holds when nobody is browsing is not a rate worth recording. And the threshold is the **p95, not the
median** — when reads serialise behind the writer the latency distribution goes bimodal rather than
shifting, since most browses still slip into the gap between two writes while the unlucky ones wait
out a whole write (or several, `std::sync::Mutex` being unfair). Forcing `Db::read` back onto the
writer mutex on the smoke profile moved the median only 0.6 ms → 1.5 ms but moved the p95
1.2 ms → 289 ms, and cut completed browses from 1,231 to 40 in the same wall clock. All three
profiles share one 20 ms reference at a 2.0 ratio, because a browse under write must not scale with
catalog size when keyset pagination and the read pool are both working — measured, the pooled p95 is
1.18 ms on smoke and 1.10 ms on 100k. That leaves roughly 30x of headroom in the 40 ms ceiling for a
loaded CI runner, while the serialised regression overshoots it by between 1.4x and 8x.

The Node browser profile traverses every logical page while retaining only the production browse
window and reports long-scroll p99 work time and post-midpoint heap growth.

`derivative_cache_first_thumbnail_hit_ms` measures an existing thumbnail read immediately after the
production cache controller starts its inventory against that profile-sized tree. The inventory is
joined after the timed hit so its I/O cannot leak into later store/browser samples or fixture
cleanup. All profiles deliberately share the same 5 ms reference and 5.0 ratio (25 ms ceiling): a
first hit may not buy a looser budget as the cache grows from 2,000 to one million entries. The
metric therefore fails a regression that puts the recursive inventory back on the request path.

Reports include every raw timing sample, median and p95, units, lower/higher-is-better direction, fixture seed/version/profile, database size, OS/architecture/CPU/memory, Rust version, git revision, browser status, comparisons, and the overall result. Missing/non-finite metrics, unknown baseline keys, direction mismatches, and browser failures are errors rather than silent passes.

## Ratio baselines

[`perf/baselines.json`](../perf/baselines.json) contains a reference value and maximum allowed ratio for every metric and profile. For lower-is-better metrics the ratio is `measured / reference`; for higher-is-better metrics it is `reference / measured`. A ratio over `max_ratio` fails the command.

Baselines are intentionally profile- and machine-class-specific reference points, not absolute product promises. Recalibrate them only from several clean optimized runs on the documented Linux CI class, review the raw samples and machine metadata, and commit the baseline change separately with a reason. Do not bless a one-off slow run.

## Automation

[`performance.yml`](../.github/workflows/performance.yml) runs the bounded smoke profile on pull requests and main pushes. A nightly schedule runs 100k and 1m independently; workflow dispatch can run any one profile or both full profiles for pre-release qualification. JSON reports upload with `if: always()`, including on a threshold failure, so regressions retain evidence.
