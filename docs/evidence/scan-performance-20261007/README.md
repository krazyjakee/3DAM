# Scan performance evidence — 7 October 2026

These reports exercise production `EmbeddedLibrary` scan jobs. They are manual measurements,
not a scheduled performance gate. See [the harness guide](../../scan-performance-sweep.md)
for policy, counters, cache limits and reproducible options.

The initial implementation is `7d778ad`; `9b43439` additionally removes unchanged pending-revision
presence writes from the catalogue WAL; `1dd9002` moves ordinary checkpoints off the writer mutex. Each JSON records its executable revision, effective
I/O policy and per-phase assertions. `working_tree_dirty=true` reflects deliberately preserved
pre-existing user files and evidence documentation; executable implementation was committed before these runs.

## Host and fixtures

[host.json](host.json) records device models, mounts, available memory and fixture/cache notes.
The native source is a disposable 4000-entry Linux ext4 fixture on a Seagate rotational HDD:
2000 accepted tiny PNG files and 2000 rejected `.dat` files. Shared placement puts the catalogue
on that disk; separate placement puts it on a Toshiba rotational HDD. Resource detection resolves
whole devices independently and uses conservative 8 MiB/s defaults. Native runs retain production
pressure gates and enable real warming/analysis after initial verification, browsing, favourite
writes, and a 4 KiB co-tenant requesting Linux page-cache eviction.

Directory metadata is warm from creation. The first shared run requested payload eviction;
later runs reuse a warmed source. Linux `fadvise(DONTNEED)` does not bypass filesystem/drive
caches. Sub-millisecond co-tenant values therefore do **not** demonstrate cold physical HDD seeks.
The initial PNGs share a content hash, allowing thumbnail reuse; analysis is active, but this is
not a diverse-media decoder benchmark. No global caches were dropped or unrelated services stopped.

Generated 100k narrow/deep and 1M wide profiles use all accepted 32-byte text files, no injected
source delay, a real NVMe-backed catalogue and cached co-tenant reads. They establish path/database
scale, including one million verified catalogue rows, rather than cold HDD throughput. The separate
HDD/remote model injects seek/listing/cold-metadata costs and protocol round trips independently;
it does not inject seek delay into SQLite commits. The large-file profile streams a changed 4 GiB
asset to real scratch files with bounded buffers. It does not allocate a 4 GiB source blob.

## Reading the reports

Every quick phase requires zero source fetches, payload reads and scratch writes. New revisions
remain pending until explicit Full verification in this experiment; automatic enrichment is
intentionally deferred so its traffic cannot contaminate discovery counters. Production enrichment
runs separately. Missing timestamps cause conservative Delta verification rather than a false
unchanged claim.

Each profile reports browse/co-tenant/foreground-writer p95 and p99, writer SQL/explicit commits,
asset update hooks, process I/O, WAL observations, logical checkpoint traffic, RSS, pressure,
first observed committed progress and cancellation source quiescence. Read-pool SQL and scratch
read bytes are unavailable and remain null. The writer counters include deliberate favourite
edits and concurrent analysis; native unchanged Delta asset updates cannot be attributed to scan
alone. Generated runs omit these edits and report zero unchanged Delta asset-row updates.

WAL length is not cumulative traffic. Passive-checkpoint frames are logical bytes, while process
`write_bytes` includes all threads and files. A single-run time difference is not proof of a
throughput improvement. Compare matching phases and recorded cache/topology conditions.

## Validation and remaining platforms

[validation.json](validation.json) records passing native, feature-matrix, web/WASM and integrated
checks. The canonical CI gate was executed as its component checks: already passing checks were
reused while their inputs were unchanged, and affected checks were rerun after fixes. Final
workspace tests passed 681 tests with 8 ignored; ANN tests passed 148 with 6 ignored; the production
example passed 5 tests. Strict lint and workspace/release builds passed.

[#208](https://github.com/krazyjakee/3DAM/issues/208) retains the physical SMB/SFTP, NTFS/FUSE,
100k syscall-trace and diverse-media measurement matrix requiring additional controlled fixtures.
The original deployment incident remains [#187](https://github.com/krazyjakee/3DAM/issues/187);
these local tests do not establish that upgrading that separate deployment resolves its contention.
The vector-index acceptance in [#43](https://github.com/krazyjakee/3DAM/issues/43) is separate from
this one-million-path production scan measurement.

## Measured phases

Times are seconds. The cold/warm labels below describe each report's modeled phase; physical
native metadata was warm as explained above. The latency column is the maximum per-phase p95
in milliseconds for browse / co-tenant / foreground writer. Zero writer latency means no samples
because foreground edits were disabled or no verified row was yet available. Native Delta updates
include analysis/favourite writes; generated Delta values isolate the scan writer.

| Profile | First Quick | Unchanged Quick cold / warm | Few-change Quick | Unchanged Delta asset updates | Worst p95 browse / read / edit |
|---|---:|---:|---:|---:|---:|
| [Physical shared HDD, initial fixes](native-hdd-shared.json) | 3.067 | 4.068 / 3.735 | 4.962 | 23 | 1.10 / 15.76 / 53.26 |
| [Physical shared HDD, pending-spool fix](native-hdd-shared-after.json) | 3.066 | 3.625 / 3.625 | 3.626 | 23 | 0.75 / 5.69 / 59.92 |
| [Physical separate HDDs, **failed writer target**](native-hdd-separate-before.json) | 2.019 | 2.043 / 2.044 | 2.041 | 21 | 1.91 / 39.36 / 275.01 |
| [100k narrow/deep, before pending-spool fix](generated-100k-before.json) | 11.133 | 8.308 / 8.237 | 9.409 | 0 | 4.84 / 0.01 / 0.00 |
| [100k narrow/deep, after pending-spool fix](generated-100k-after.json) | 12.257 | 8.926 / 8.802 | 8.811 | 0 | 3.40 / 0.01 / 0.00 |
| [1M wide, NVMe/virtual-source scale](generated-1m.json) | 53.350 | 25.315 / 26.060 | 36.155 | 0 | 24.39 / 0.01 / 0.00 |
| [1k paths, 75% rejected, HDD/request-cost model](generated-hdd-remote.json) | 8.117 | 7.994 / 4.427 | 4.117 | 19 | 30.80 / 26.78 / 0.00 |
| [32 initial paths; one changed to 4 GiB](generated-large.json) | 0.005 | 0.007 / 0.010 | 0.010 | 0 | 3.12 / 0.01 / 0.00 |

Every Quick phase read zero payload bytes and wrote zero scratch bytes. The 4 GiB change took
10.35 ms to discover, then 35.64 seconds to verify with a 128 MiB/s source cap and separate 512 MiB/s
scratch cap. One million accepted paths were persisted and verified; unchanged Delta made zero
asset updates, completed in 17.11 seconds and stayed below 24.39 ms browse p95 across all phases.
Observed RSS peaked around 64 MiB. The native separate-HDD initial verification failed its 60 ms
edit budget and is retained as the before report for [#207](https://github.com/krazyjakee/3DAM/issues/207).

The pending-spool fix reduced shared-HDD unchanged queued-scan logical checkpoint traffic from
712,704 to 159,744 bytes (77.6%). Its matching warm pass went from 667,648 to 159,744 bytes. A 100k
unchanged queued pass changed process write_bytes from 67.8 MB to 22.7 MB on the first repeat and
0.4 MB on the warm repeat; temporary spool writeback/coalescing and process-wide accounting matter.
Its Quick wall time was 8.9 seconds versus 8.3 seconds before, so these runs establish less write
traffic rather than a universal clock-time speedup.

## Ordered and shuffled observation replay

[marker-replay.json](marker-replay.json) compares the old indexed asset-presence UPDATE with the
production private-spool observation SELECT/INSERT, on 20k verified rows copied from the 100k
fixture under schema V29. Both use 128-entry chunks, 157 explicit commits, WAL/NORMAL, default
1000-page auto-checkpoint and Python SQLite 3.45.1. Warm NVMe-backed `/tmp`; source walking,
progress and final missing reconciliation are excluded. The original audit in #190 used schema V27;
these current-schema figures are a separate replay, not a repeated historical end-to-end baseline.

| Observation strategy | Order | Time (ms) | Asset updates | Process write_bytes | Catalogue WAL at end |
|---|---|---:|---:|---:|---:|
| legacy_marker | insertion | 221.2 | 20000 | 20,930,560 | 4,202,432 |
| legacy_marker | shuffled_seed_42 | 1039.5 | 20000 | 259,067,904 | 5,178,872 |
| private_spool | insertion | 181.0 | 0 | 598,016 | 0 |
| private_spool | shuffled_seed_42 | 262.8 | 0 | 581,632 | 0 |

The private spool still writes a small disposable file; zero catalogue WAL does not mean zero
storage work. [run-marker-replay.py](run-marker-replay.py) records the replay used here.
[run-native.py](run-native.py) records the native fixture mutation and invocation for this host;
restore the two changed disposable files and use a fresh catalogue before repeating it.
[commands.txt](commands.txt) records each invocation reconstructed from the report configuration.
The initial shared report's `paths=100000` was a generated-mode default; actual native counters
record 4000 entries and 2000 accepted assets. Subsequent native invocations set `--paths 4000`.

## HDD checkpoint follow-up

[The first post-fix separate-HDD report](native-hdd-separate-after.json) reduced initial Full
edit p95 from 275.01 to 34.25 ms, but its 20-sample few-change phase reached 60.88 ms and failed
unchanged 60 ms acceptance. This failure remains recorded. The percentile method uses the higher
sample quantile (`ceil((n-1)*p)`); with 20 samples, p95 is the maximum. No estimator or target was
changed to turn that report into a pass.

[The untraced separate-HDD unchanged-source run](native-hdd-separate-unchanged.json) passed every
phase: initial Full edit p95 was 44.45 ms; the post-Full Quick was 2.32 seconds with warming/analysis
active and edit p95 of 57.04 ms. `changed=removed=0` and no fixture edits make its `few_change_quick`
phase an actual verified unchanged Quick. [The final shared-HDD run](native-hdd-shared-final.json)
uses the same unchanged-source option and also passed all phases: post-Full Quick 3.92 seconds,
edit p95 0.24 ms; initial Full edit p95 40.74 ms. The native fixture's 4000 entries/2000 accepted files
and production resource defaults were retained.

[The strace diagnostic](native-hdd-separate-trace.json) retains its two failed edit budgets; syscall
tracing affects timing, so this is not an untraced acceptance result. [sync-trace-summary.json](sync-trace-summary.json)
records all 61 completed fsync events with timestamps, thread IDs and observed paths. Separate-connection
checkpoint syncs can take over 100 ms without holding the store writer mutex, as independently proved
by the worker completion/main-file-copy regressions. Ordinary WAL recycling and the harness's
explicit end checkpoints still cause syncs. These results establish a large measured checkpoint
improvement, not a universal 60 ms hardware deadline. Broader cold-platform and tail-latency
qualification remains in [#208](https://github.com/krazyjakee/3DAM/issues/208).

The independent worker coalesces notifications and retries incomplete PASSIVE work at 500 ms intervals;
idle completed work performs no checkpoint I/O. Startup failure retains automatic checkpoints;
runtime failure restores that policy on the next commit. Exclusive maintenance drains the worker,
and shutdown stops retries after a final attempt. PASSIVE may copy all eligible frames; external
pinned readers or writes outrunning checkpoint throughput can still grow WAL, as with the default
policy. No hard WAL-size or physical latency cap is promised.

## Serial-storage scheduler control

[shared-hdd-scheduler.json](shared-hdd-scheduler.json) records the explicitly invoked ignored
scenario at `1dd9002`. It compares current governed and ungoverned policy on a serial request-cost
model and real temporary cache/asset files. Sustained copy completed all four 8 MiB assets while
foreground p95 fell from 102.66 to 32.67 ms and small-read p95 from 103.16 to 33.09 ms; accounted
aggregate throughput was 8.00 MiB/s at the 8 MiB/s shared-device cap. Both limits were asserted.
The cold-cache case avoids full preview-cache reads with bounded validation, so the governed and
ungoverned cases transfer different bytes; its short duration is not a same-work throughput claim.
No OS cache reset or physical disk queue measurement is implied by this scenario.

## Filesystem notification qualification

[#209](https://github.com/krazyjakee/3DAM/issues/209) closes a correctness gap in the watcher shortcut:
`LocalFs` does not establish that remote-client changes produce notifications. Trusted clean skips
and scoped walks require conservative filesystem coverage, unchanged mount topology and the original
watched root identity. Network/unknown coverage uses full polling; other platforms keep notifications
as full-reconciliation hints. Deterministic coverage and root-replacement tests are listed in
[validation.json](validation.json). These benchmark profiles submit explicit scans without a trusted
watch journal, so their reported scan counters and durations are unaffected by the capability gate.
[Read-only mount context](watcher-mount-context.json) confirms the host has both ext4 HDD mounts
and a CIFS mount; it does not exercise external-client notifications. Live remote/platform
qualification remains #208.
