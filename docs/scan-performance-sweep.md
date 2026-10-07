# Manual production scan performance sweep

`scan_performance` drives `EmbeddedLibrary` jobs through the production scan coordinator,
source filtering, metadata admission, store batches, search/aggregate triggers and I/O scheduler.
It compares discovery, unchanged quick scans, verification, changed discovery, Delta and cancellation.
This is a manual local benchmark. It adds no scheduled workflow or shared-runner performance gate.

The generated backend streams 100k/1M names and bounded byte buffers rather than creating a million
source files. Each accepted file is ordinary text; unsupported `.dat` names exercise rejection before
metadata requests. Width/depth control the virtual directory shape. Source timestamps can be absent,
entries can disappear, and a few revisions change between phases. Generated fetches hash while writing
their private scratch representation, matching the built-in remote materialisation path.

The local backend wraps a real `LocalFsSource`. Its fetch capability decides whether scratch is used;
Linux descriptor-backed ingest is counted as a source read with zero copied scratch bytes. The sweep
never modifies local source files. Use a disposable fixture and `--pause-before-change true` to change
or remove entries after the baseline, then press Enter to continue. A fresh data directory is required.

## Reproducible commands

Run from the repository root. Each command writes a JSON report before returning a nonzero exit status
for failed assertions. Keep the report together with the exact command and host/storage notes.

```sh
rtk cargo run -p dam-core --release --example scan_performance -- \
  --data-dir /mnt/sweep/quick-100k-shared --paths 100000 \
  --placement shared --width 100 --depth 3 --rejected-percent 25 \
  --metadata-us 0 --cold-metadata-us 8000 --listing-us 8000 --seek-us 8000 \
  --transfer-mib 32 --io-mib 8 --foreground-budget-ms 60 --timeout-secs 7200

rtk cargo run -p dam-core --release --example scan_performance -- \
  --data-dir /mnt/sweep/quick-1m-separate --paths 1000000 \
  --placement separate --width 1000 --depth 1 --rejected-percent 75 \
  --absent-timestamp-percent 10 --changed 10 --removed 10 \
  --metadata-us 0 --cold-metadata-us 8000 --listing-us 8000 --seek-us 8000 \
  --transfer-mib 32 --io-mib 8 --data-mib 128 --timeout-secs 86400
```

One million genuinely cold metadata operations at 8 ms each have an hours-scale baseline. The model
does not hide that cost behind a warmed host filesystem. Use lower delays for implementation sweeps,
then repeat the declared HDD profile when sufficient time is available. `--quick-budget-ms` sets an
explicit machine/profile budget; there is no universal quick-duration threshold across these shapes.

Repeat with `--width 25 --depth 8` for narrow/deep trees and `--width 1000 --depth 1` for wide trees.
`--round-trip-us 2000` adds a modeled SMB/SFTP request cost independently of seek latency and transfer
bandwidth. It counts modeled requests, not actual protocol packets or authentication handshakes.
`--large-bytes 4294967296` makes the first changed accepted path a streamed 4 GiB asset. No large source
blob or large allocation is created, but its real scratch copy needs disk space and verification time.

For physical storage, place a disposable fixture on the source device and the fresh catalog directory
on the declared data device. Zero injected transfer bandwidth means no artificial transfer delay:

```sh
rtk cargo run -p dam-core --release --example scan_performance -- \
  --backend local --source-root /mnt/hdd/scan-fixture \
  --data-dir /mnt/ssd/sweep-catalog --placement separate \
  --transfer-mib 0 --pressure-policy production --background true --co-tenant-cache evict \
  --pause-before-change true --topology detected --io-mib 0 --timeout-secs 7200
```

`--background true` starts the real warming/analysis pipeline after baseline verification. It is
available for native local fixtures, including kernel-mounted network filesystems. Generated virtual
paths cannot be reopened by the native analysis backend, so generated mode rejects that combination.
Browsing and a 4 KiB read co-tenant run concurrently in every profile. Their p95 and p99 are reported;
the default assertion is p95 ≤ 60 ms, with an explicit `--foreground-budget-ms` override. Actual warming
and analysis source traffic bypasses the scan wrapper, so process I/O and resource diagnostics capture
that interference while the source wrapper counters describe only the selected scan.

The default `--co-tenant-cache cached` repeatedly reads the same 4 KiB OS page and measures cached
foreground reads. It cannot establish physical HDD small-I/O latency. For physical runs, select
`--co-tenant-cache evict`: Linux syncs the fixture once, then requests `POSIX_FADV_DONTNEED` before
each timed read. Unsupported platforms and failed advice calls fail visibly. This requests page-cache
eviction; filesystems and drives can still cache the block, so verify physical reads with process I/O
and external tracing. JSON records the mode and its limits. Separate placement uses `--data-mib` for
modeled foreground data-device transfers; `--transfer-mib 0` disables all artificial transfer delays.
Injected disk latency covers source operations, scratch copies and the foreground read requests.
SQLite writes and checkpoints use the real catalog filesystem; the model does not inject HDD seek
latency into WAL commits. Interpret generated writer latency accordingly and repeat on physical media.

Add `--foreground-writes true` to toggle a fixture asset's favourite periodically through
`LibraryService`, recording foreground writer p95/p99 against the same budget. Leave it false when
comparing unchanged-scan WAL/SQL traffic without deliberate foreground mutations. These edits apply
only to the fresh benchmark catalog, and scans preserve the favourite flag.

## What the evidence measures

The report records revision, configuration, backing-resource overrides, effective I/O budgets, sampled
yield reasons, PSI/load snapshots, observed RSS, first observed committed progress, job status and warnings,
scan duration, payload/fetch counters, browse/co-tenant latency and cancellation source quiescence.
Shared placement groups source and data overrides into one resource; separate placement gives source
and data distinct identities and caps. These are explicit operator/model declarations; inspect the
reported devices and actual mounts before interpreting a physical run.

Generated mode uses declared topology. Native mode defaults to detected topology, retaining partition/
mapper deduplication and device queue/latency probes; `--io-mib 0` uses hardware defaults. A positive
`--io-mib` with detected topology is a global per-device cap. Placement and `--data-mib` determine caps
only in declared topology; choose `--topology declared` for explicit hidden-resource overrides, and
record that those overrides have no native device-stat probe. The JSON budgets show the resolved
identities and rates actually used.

The profile sequence is:

1. First quick discovery with modeled cold metadata.
2. Unchanged quick discovery with cold metadata, then warm metadata.
3. First full byte verification.
4. Few-change quick discovery and full verification of the resulting source state.
5. Unchanged Delta and unchanged full verification.
6. A full scan cancelled after the configured delay.

Automatic enrichment is deferred during the sweep. Quick discovery still persists production pending
state, but full jobs verify it explicitly so an unrelated enrichment worker cannot contaminate quick
counters or try to reopen generated paths. Consequently the report separates discovery completion
from verification availability; it does not claim quick completion includes finished enrichment.

Every quick profile asserts zero fetch entries, source payload bytes and scratch writes. Generated
listing/stat counters are explicit model operations: unsupported paths have no file-token metadata
request, directory requests match the generated hierarchy, and accepted paths have one token stat.
For native local walks, counters represent eligibility callbacks and listing/metadata admissions rather
than exact OS syscalls; backends can require extra type/stat operations. Source payload bytes describe
successfully returned logical representations, not physical reads, and native partial failed fetches
need external tracing. Cancellation reports both whether a cancellation actually landed and time until
source callbacks stopped; an immediately updated cancelled job row alone is not worker quiescence.
Cancellation acceptance requires a requested cancellation, a Cancelled job and finite source callback
quiescence within 30 seconds. A fixture that completes before the deadline fails as inconclusive;
use a larger fixture or earlier `--cancel-after-ms`. `first_observed_committed_progress_ms` records when
the harness consumes the first positive-done event, including queued events drained at completion.
It includes event-delivery delay and does not timestamp the underlying database commit.

Linux `/proc/self/io` deltas include all engine threads, SQLite, browsing, inventory and co-tenant
work. `read_bytes`/`write_bytes` describe process-attributed storage I/O; `rchar`/`wchar` include cached
and other I/O. Neither attributes traffic to a specific source, scratch directory or SQL statement.
Observed WAL length/growth can miss reuse or short peaks and is not cumulative WAL traffic. Passive
checkpoint frame counts multiplied by page size are logical checkpoint bytes, not physical writes.
Checkpoint process-I/O deltas are recorded separately. Non-Linux probes and unavailable precise
metrics are null, rather than invented zeroes.

The manual harness enables SQLite statement/profile tracing on the catalog writer. It reports actual
outer writer statements, trigger statements, commit statements and writer profile time independently.
`sql_commit_statements` counts explicit SQL COMMIT statements, excluding implicit autocommit commits.
Profile timing uses SQLite's coarse millisecond resolution and includes nested trigger work. The
counters cover the measured catalog writer, including concurrent analysis and foreground edits, but
exclude read-pool statements, physical I/O and the separate connection used for passive checkpointing.
Asset-row updates use SQLite’s update hook, including attempted updates later rolled back; unchanged successful profiles have no such rollback ambiguity. Use one measured catalog per process. Read-pool SQL counts and scratch-read bytes remain null. For a
physical Linux run, collect syscall evidence beside the JSON, accounting for tracing overhead:

```sh
rtk cargo build -p dam-core --release --example scan_performance
rtk strace -f -yy -o /mnt/sweep/scan-syscalls.log \
  -e trace=openat,close,getdents64,newfstatat,statx,read,pread64,write,pwrite64,fdatasync,fsync \
  target/release/examples/scan_performance --backend local \
  --source-root /mnt/hdd/scan-fixture --data-dir /mnt/ssd/traced-catalog \
  --placement separate --topology detected --io-mib 0 --transfer-mib 0 --pressure-policy production
```

Use descriptor paths to separate source reads, scratch reads/writes, catalog/WAL/spool writes and
co-tenant I/O. Syscall/fsync counts are not SQL statement or transaction counts. The focused store tests
in `assets.rs` trace real maintenance statements; the V28 migration test counts SQLite row changes,
including trigger effects. They cover unchanged verification, same-path byte edits, name changes,
body invalidation, aggregates, folder movement and deletion with production triggers enabled.

The production token-bucket tests in `resources/io.rs` use injected time at 4, 8, 128 and 1,024 MiB/s
for 1 KiB/256 KiB requests, including timer overshoot, bounded credit, directional devices, cancellation
and pressure. The example tests exercise real quick/Full/Delta jobs and payload counters. The streaming
integration test proves ready items commit while a later fetch continues and cancellation does not
perform authoritative missing reconciliation.

```sh
rtk cargo test -p dam-core --example scan_performance
rtk cargo test -p dam-core --test scan_processing_progress
rtk cargo test -p dam-core resources::io::tests
rtk cargo test -p dam-store verification_and_same_path_edits
rtk cargo test -p dam-store unchanged_index_migration
```

These commands are documented for later manual execution. No measurements are implied by adding this
harness or inspecting source. Compare two revisions on the same host, fixture, mount topology and
cache condition; record filesystem/mount options, device model, competing workload, free disk space,
subprocess probe availability and whether OS caches were actually cold. The harness never drops
system caches. Physical HDD p95/p99, WAL/checkpoint traffic and foreground writer latency require
separate measured evidence; the injected serial disk is a reproducible request-cost model.

## Scan policy used by the production path

Quick discovery queues durable pending revisions for enrichment. The sidebar selects Quick; Full
remains explicit byte verification, and existing Delta clients retain their compatibility behavior.
The CLI exposes `--quick` explicitly; `--delta` keeps its existing meaning.
Manual requests enter the coordinated priority path rather than waiting for a source polling timer.
Metadata work has one bounded operation slot per resource and shares the byte budget with bulk work;
it does not hold the catalog writer while waiting for admission. Large bulk permits therefore do not
occupy the metadata lane for an entire multi-gigabyte read.

Local notifications retain at most 512 dirty paths. Trusted clean Quick skips and scoped walks
require complete filesystem notification coverage and a stable watched root. Linux uses a conservative
local filesystem allow-list and rejects roots with descendant mounts or stacked covering mounts;
network, FUSE and unknown filesystems use full polling reconciliation. Other platforms retain
notifications as full-scan hints until completeness can be established. Root replacement or changed mount topology revokes trust.
Overflow or watcher errors request a full scan; initial registration is full, and a watchdog performs
full reconciliation within the configured maximum interval. Registration failure falls back to polling.
Remote sources use
`DAM_SOURCE_POLL_SECONDS` (default 60) and `DAM_SOURCE_POLL_MAX_SECONDS` (default 900, maximum 86,400).
Idle/offline intervals double up to the ceiling, successful changes reset the interval, and jitter is
stable, at most 10%, and bounded by the ceiling. A poll awaits completion before scheduling the next;
manual scans bypass that delay. Record overrides when comparing watcher/poll-triggered measurements;
this example measures explicitly submitted scans and does not claim to measure watcher delivery.

## Measured sweep on 7 October 2026

The [checked-in evidence](evidence/scan-performance-20261007/README.md) contains production
JSON, exact profile configurations, storage/cache notes, SQL replays and validation results.
Quick phases read zero payload bytes and wrote zero scratch bytes, including discovery of a
changed 4 GiB asset in 10.35 ms before its explicit 35.64-second verification. The 100k and
one-million-path profiles asserted Quick completion below 15 and 60 seconds respectively and
RSS below 128 MiB. These scale profiles use tiny virtual payloads and an NVMe catalogue;
they are not cold physical-HDD claims.

Physical shared/separate Linux ext4 HDD runs use a disposable 4000-entry fixture and retain
production pressure gates, real analysis/warming, browsing, foreground catalogue edits and
an evict-advised small-read co-tenant. The separate-HDD initial verification exposed a
275 ms foreground-edit p95 checkpoint stall, retained as failed evidence for #207. Checkpoint
offload reduced initial-verification edit p95 to 34 ms; later unchanged-source shared/separate runs
passed all phase budgets. A 60.9 ms small-sample phase and traced failures are retained too. The
pending-observation fix reduced matching shared-HDD unchanged queued-scan logical checkpoint
traffic by 77.6%. Timing, physical I/O and cache limits are reported independently.

The evidence validator preserves the expected failed baseline and checks the passing reports:

```sh
rtk python3 docs/evidence/scan-performance-20261007/validate_evidence.py
```

[#208](https://github.com/krazyjakee/3DAM/issues/208) retains the additional controlled
SMB/SFTP, NTFS/FUSE, large syscall-trace and diverse-media measurements. The original
Docker-in-LXC incident in #187 and vector-index acceptance in #43 remain separate follow-ups.
