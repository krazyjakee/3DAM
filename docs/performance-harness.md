# Reproducible scale and performance harness

The scale harness turns the 100,000- and 1,000,000-asset targets into repeatable regression checks without committing a generated catalog. It creates a migrated SQLite library under `target/`, measures store and browser browse-window behavior, compares the result with a profile-specific ratio baseline, writes JSON, and removes the catalog.

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

The fixture contains local filesystem, SFTP, SMB, and federated sources; all five media types; paths up to the configured depth; confirmed tags; normalized embeddings; and repeatable exact-duplicate groups. Names include a stable search term. The generated database and WAL never enter git.

## Measurements and reports

The harness measures median first-page, late-page, lexical-search, faceted-query, stats, analysis-planning, and exact-duplicate latency; scan/upsert and bounded export throughput; process peak RSS; and the serialized initial browse payload. The Node browser profile traverses every logical page while retaining only the production browse window and reports long-scroll p99 work time and post-midpoint heap growth.

Reports include every raw timing sample, median and p95, units, lower/higher-is-better direction, fixture seed/version/profile, database size, OS/architecture/CPU/memory, Rust version, git revision, browser status, comparisons, and the overall result. Missing/non-finite metrics, unknown baseline keys, direction mismatches, and browser failures are errors rather than silent passes.

## Ratio baselines

[`perf/baselines.json`](../perf/baselines.json) contains a reference value and maximum allowed ratio for every metric and profile. For lower-is-better metrics the ratio is `measured / reference`; for higher-is-better metrics it is `reference / measured`. A ratio over `max_ratio` fails the command.

Baselines are intentionally profile- and machine-class-specific reference points, not absolute product promises. Recalibrate them only from several clean optimized runs on the documented Linux CI class, review the raw samples and machine metadata, and commit the baseline change separately with a reason. Do not bless a one-off slow run.

## Automation

[`performance.yml`](../.github/workflows/performance.yml) runs the bounded smoke profile on pull requests and main pushes. A nightly schedule runs 100k and 1m independently; workflow dispatch can run any one profile or both full profiles for pre-release qualification. JSON reports upload with `if: always()`, including on a threshold failure, so regressions retain evidence.
