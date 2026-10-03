# Shared storage policy (issue #187)

Background work now has a storage budget independent of the CPU worker count. On Linux, paths
are resolved with filesystem device IDs: bind mounts share their backing device, partitions share
their parent disk, and device-mapper/RAID slaves share physical-device permits. An operation that
uses several resources takes them in a stable order with rollback; unrelated known devices do
not wait for one global permit. Failed topology discovery, network mounts and other platforms
use one conservative unknown-resource budget. Discovery reads metadata and sysfs; it never runs
a benchmark or writes to an asset source.

| Storage class | Default background read/write allowance | Concurrent operations |
| --- | ---: | ---: |
| Rotational | 8 MiB/s | 1 |
| Solid state | 128 MiB/s | 2 |
| Unknown/network | 4 MiB/s | 1 |

These are maximum admission rates, not promises of device performance. Copies conservatively
charge both their reads and writes to their participating resources. Shared-device copies use
one permit, rather than deadlocking while acquiring it twice. Observed device queue depth above
0.5 (2 for SSDs), or average request latency above 20 ms (5 ms for SSDs), halves the allowance,
down to 64 KiB/s. Healthy samples restore 1/16 of the configured allowance per sample. Sampling
is at most every 250 ms. Missing device observations retain the class's conservative rate.

Host and visible cgroup I/O pressure, memory headroom, and CPU load can pause admission entirely.
The process's cgroup membership and cgroup2 mount root determine which leaf and ancestor limits
are visible, including nested Docker/LXC limits. Host/leaf PSI `full total` deltas detect new stalls
within a 250 ms sample window; host and ancestor `full avg10` readings retain recovery hysteresis.
Invisible ancestors cannot be inferred from inside a cgroup namespace.

Local, SFTP and SMB source materialisation, scan hashing, and background cache publication check
admission and cancellation in chunks of at most 256 KiB. Cache inventory checks between metadata
operations, remains off the startup/request path, and cancels when its library is dropped. Cached
thumbnail/preview warming checks immutable files with metadata, including partial model-derivative
hits; it never loads a DMSH payload merely to discard it. Startup eviction sorts victims once
instead of repeatedly scanning the entire inventory for each deletion.

Foreground catalog/health/cache-hit work bypasses background permits. Interactive generation has
its own bounded worker. A waiting interactive request preempts a background warm of the same
asset at its next managed admission boundary; completed render output is reused at foreground
priority. Canceled request futures release their single-flight registrations, and shutdown stops
background cache writes. Cache reads
and chunked publications perform payload I/O outside the shared accounting mutex; reservations
keep simultaneous publications within the byte cap. Completed analysis/derivative work retains
its durable version markers. Pressure pauses resume automatically without a restart.

## Operator configuration and diagnostics

Global ceilings live in `[resources] io_max_mib_per_sec` and `io_concurrency`, or
`3DAM_IO_MAX_MIB_PER_SEC` and `3DAM_IO_CONCURRENCY`. File configuration takes precedence. Zero
bandwidth selects the class default; concurrency has a minimum of one. An explicit concurrency
cap overrides the class default without increasing the number of background CPU workers.
The existing `max_io_stall_pct`/`3DAM_MAX_IO_STALL_PCT` threshold remains available; values at
least 100 disable pressure gating but retain byte and concurrency admission limits.

When container mounts hide their topology, supply resource identities explicitly. Use the same
identity for every path that shares a disk. The longest matching path override wins; overrides
with the same identity share the most conservative class and strictest caps, independent of
access order. Overrides trust the operator's storage classification and have no device-stat probe.

```toml
[resources]
io_max_mib_per_sec = 8
io_concurrency = 1

[[resources.storage]]
path = "/var/lib/3dam"
resource = "data-hdd"
kind = "rotational"
max_mib_per_sec = 4

[[resources.storage]]
path = "/assets"
resource = "assets-hdd"
kind = "rotational"
max_mib_per_sec = 8
```

Use `solid_state` or `unknown` for the other classes. Relative/inaccessible paths fall back to
best-effort discovery; use absolute paths in overrides. Paths that cannot be discovered share
the unknown resource unless overridden. Native SFTP/SMB connections currently share the unknown
network resource; their scratch disk is separately identified.

`GET /admin/api/maintenance/usage` now includes `io_budgets`, `io_stall_pct`, and
`cache_inventory_ready`. Each budget reports its identity/class, configured and adaptive rates,
concurrency, active/deferred operations, accounted bytes, observed device throughput, queue depth,
request latency, probe availability and yield reason (`memory`, `cpu_load`, `io_stall`, `bandwidth`,
`concurrency`, `device_latency_or_queue`, or `ready`). Device observations are whole-device read +
write deltas and can include co-tenants; accounted bytes are admission estimates, not kernel I/O
counters. Missing observations are null and `probes_available` is false. Cache totals are partial
until `cache_inventory_ready` is true; diagnostics never wait for inventory to finish. Resolution
and host-pressure transitions are also logged.

## Reproducible contention experiment

Run without root, FUSE, or a physical HDD:

```sh
cargo test -p dam-core --lib shared_hdd_scenario -- --ignored --nocapture
```

The fixture uses one serial resource with 8 ms/request latency and 32 MiB/s transfer speed,
32 MiB of populated preview caches, four 8 MiB assets copied through the production local-source
chunk loop, a 128-row SQLite catalog with concurrent query/thumbnail reads, and a competing
4 KiB workload. The limiter applies even on a warm SSD. The baseline replays current main
(`66d05a6`) access patterns with unpaced concurrent copies and payload-reading cache warm checks.
It is not a benchmark of an unmodified-main binary. The after run uses the production scheduler
and metadata-only cache probe. Cache warming uses one sequential worker, matching the hosted
warming loop; sustained copies use four concurrent workers. The phases have separate measurements;
startup inventory responsiveness is also exercised by the `maintenance` integration tests under
an impossible memory floor. Ordinary tests cover device topology, independent devices, pressure
detection, yield/resume/cancellation, cleanup, and queue-driven allowance reduction/recovery.

Both foreground and competing workloads must have p95 latency at most **60 ms** in each after
phase, and each baseline phase must violate that budget. All four work items must complete with
correct contents. Recorded results from 3 October 2026 are in
[the JSON report](measurements/issue-187-shared-storage.json):

| Phase | Foreground p95 before → after | Competing p95 before → after | Maximum queued requests before → after | Progress |
| --- | ---: | ---: | ---: | ---: |
| Cached previews at startup | 271.0 → 23.9 ms | 271.0 → 23.9 ms | 3 → 3 | 4/4 |
| Sustained asset copies | 106.1 → 31.9 ms | 106.1 → 33.9 ms | 6 → 3 | 4/4 |

Sustained total throughput drops from 16.9 to 5.3 MiB/s and elapsed time grows from 3.8 to 12.9 s:
the foreground/co-tenant budget, rather than maximum bulk throughput, is the objective. The
simulator combines read/write service costs; these latencies are not physical read/write latency
or kernel PSI measurements. The JSON deliberately leaves physical host/cgroup PSI null. The short cold-cache phase has only three after samples;
the report preserves sample counts so that its p95 is not mistaken for a long-run estimate.

## Deployment assessment and remaining acceptance work

The reported `fceaca4f` 0.1.0 deployment predates current main's I/O PSI governor, asynchronous
cache inventory, and durable derivative backlog. Upgrading provides those improvements, but the
current-main replay still violates both latency budgets. The new chunk admission and metadata-only
warm probes address additional gaps; neither this replay nor source inspection establishes that
an upgrade alone resolves that server's incident.

The following evidence remains required before closing
[issue #187](https://github.com/krazyjakee/3DAM/issues/187):

- Run current main and this change on the reported shared HDD deployment, record actual
  per-device read/write latency, queue/throughput, host and visible-cgroup PSI deltas, browser
  request latency and co-tenant small-I/O latency across cold startup and sustained work. Record
  the binary revision and mount/override identities, rather than inferring freshness from `edge`.
- Path-based third-party media/Assimp/semantic decoders and external ffmpeg processes remain
  opaque within an item. They have device concurrency admission and an input-size allowance,
  but a decoder can reread a large scratch file or companion file in a burst; prepaying its input
  allowance does not rate-limit those actual reads. Their running kernel I/O/native calls cannot
  be interrupted by cooperative cancellation. Bounded response applies at the next managed
  chunk/entry boundary (250 ms sample + 25 ms polling, plus the current I/O call).
- Explicit bulk conversion/export, cache clearing/lifecycle deletion, and SQLite maintenance
  still need finer scheduling and real-device latency coverage. The simulator covers warm-cache probes and source copies;
  it does not claim measured end-to-end analysis/render/maintenance or startup inventory PSI.
- Validate native network backends on real SFTP/SMB servers and faster independent disks.
  Third-party `FileSource` implementations that only implement `fetch` retain a compatibility
  fallback with one admission check, rather than automatic chunk interception.

These gaps remain tracked by #187 and its existing related work
[#144](https://github.com/krazyjakee/3DAM/issues/144),
[#183](https://github.com/krazyjakee/3DAM/issues/183), and
[#184](https://github.com/krazyjakee/3DAM/issues/184). No schema migration or writes to registered
source contents are introduced. The admin response additions have serde defaults for older peers.
