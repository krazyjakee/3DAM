//! Reproducible, unprivileged shared-storage scenario. The serial device inserts request latency
//! and bandwidth limits even when the backing test directory is warm or on an SSD. It is a
//! scheduling experiment, not a claim about the incident server's physical hardware.

use super::*;
use crate::cache::{CacheOptions, Controller, Tier};
use dam_api::dto::{MediaType, QueryRequest};
use dam_api::service::Visibility;
use dam_sources::{open_source, SourceConnection};
use dam_store::{NewAsset, Store};
use std::sync::{Barrier, Condvar};

#[derive(Default)]
struct DiskState {
    issued: u64,
    serving: u64,
    bytes: u64,
    max_queue: u64,
    latencies: Vec<f64>,
}

#[derive(Default)]
struct SlowDisk {
    state: Mutex<DiskState>,
    ready: Condvar,
}

impl SlowDisk {
    fn request<T>(&self, bytes: u64, operation: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let mut state = self.state.lock().unwrap();
        let ticket = state.issued;
        state.issued += 1;
        state.max_queue = state.max_queue.max(state.issued - state.serving);
        while state.serving != ticket {
            state = self.ready.wait(state).unwrap();
        }
        drop(state);
        // 8 ms request cost + 32 MiB/s transfer, one serial HDD resource for all participants.
        std::thread::sleep(
            Duration::from_millis(8)
                + Duration::from_secs_f64(bytes as f64 / (32.0 * 1024.0 * 1024.0)),
        );
        let result = operation();
        let mut state = self.state.lock().unwrap();
        state.bytes += bytes;
        state.latencies.push(start.elapsed().as_secs_f64() * 1000.0);
        state.serving += 1;
        self.ready.notify_all();
        result
    }
}

fn percentile(mut values: Vec<f64>, fraction: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * fraction).ceil() as usize]
}

fn phase(root: &Path, cold: bool, governed: bool) -> serde_json::Value {
    let disk = SlowDisk::default();
    let stop = AtomicBool::new(false);
    let foreground = Mutex::new(Vec::new());
    let competitor = Mutex::new(Vec::new());
    let progress = std::sync::atomic::AtomicU64::new(0);
    let store = Store::open(&root.join("data")).unwrap();
    let cache = Controller::new(
        &root.join("data"),
        CacheOptions {
            local_bytes: Some(u64::MAX),
            peer_bytes: Some(u64::MAX),
        },
    );
    let mut governor = Governor::with_io(Some(0), Some(100.0), IoOptions::default());
    governor.max_load_per_cpu = f64::INFINITY;
    let device = Arc::new(Device::new(
        "simulated-shared-hdd".into(),
        Kind::Rotational,
        None,
        IoOptions::default(),
    ));
    // Both source and scratch resolve to the same physical resource, as in the incident.
    governor
        .io
        .devices
        .lock()
        .unwrap()
        .insert(device.id.clone(), device.clone());
    governor
        .io
        .paths
        .lock()
        .unwrap()
        .insert(root.join("assets"), vec![device.clone()]);
    governor
        .io
        .paths
        .lock()
        .unwrap()
        .insert(root.join("scratch"), vec![device]);
    let cancel = AtomicBool::new(false);
    let worker_count = if cold { 1 } else { 4 };
    let barrier = Barrier::new(worker_count + 2);
    let start = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            barrier.wait();
            while !stop.load(Ordering::Relaxed) {
                let at = Instant::now();
                disk.request(4096, || {
                    assert_eq!(store.stats(None, &Visibility::Full).unwrap().total, 128);
                    assert!(!store
                        .query_assets_semantic(&QueryRequest::default(), None, &Visibility::Full)
                        .unwrap()
                        .items
                        .is_empty());
                    assert!(cache
                        .read(&root.join("data/cache/thumbnails/hit.png"), Tier::Thumbnail)
                        .is_some());
                });
                foreground
                    .lock()
                    .unwrap()
                    .push(at.elapsed().as_secs_f64() * 1000.0);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        scope.spawn(|| {
            barrier.wait();
            while !stop.load(Ordering::Relaxed) {
                let at = Instant::now();
                disk.request(4096, || std::fs::read(root.join("competitor.bin")).unwrap());
                competitor
                    .lock()
                    .unwrap()
                    .push(at.elapsed().as_secs_f64() * 1000.0);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let mut workers = Vec::new();
        for index in 0..worker_count {
            let disk = &disk;
            let barrier = &barrier;
            let governor = &governor;
            let cache = &cache;
            let progress = &progress;
            let cancel = &cancel;
            workers.push(scope.spawn(move || {
                barrier.wait();
                if cold {
                    // Startup cache validation/warming used to open payloads solely to find a
                    // hit. The new warming path uses the production metadata-only probe.
                    // Hosted warming is sequential; the sustained analysis-copy phase uses
                    // the default four-worker pool. Do not inflate baseline warming concurrency.
                    for preview_index in 0..4 {
                        let preview =
                            root.join(format!("data/cache/previews/{preview_index}.dmsh"));
                        if governed {
                            let work = governor
                                .io
                                .acquire(governor, &[Some(&root.join("scratch"))], cancel)
                                .unwrap();
                            work.pace(4096).unwrap();
                            assert!(disk.request(4096, || cache.contains(&preview, Tier::Preview)));
                        } else {
                            assert!(disk
                                .request(8 * 1024 * 1024, || cache.read(&preview, Tier::Preview))
                                .is_some());
                        }
                        progress.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    let source = open_source(
                        &SourceConnection::LocalFs {
                            root: root.join("assets").display().to_string(),
                        },
                        &root.join("scratch"),
                    )
                    .unwrap();
                    let work = governed.then(|| {
                        governor
                            .io
                            .acquire(
                                governor,
                                &[Some(&root.join("assets")), Some(&root.join("scratch"))],
                                cancel,
                            )
                            .unwrap()
                    });
                    let fetched = source
                        .fetch_paced(&format!("{index}.bin"), &mut |bytes| {
                            if let Some(work) = &work {
                                work.pace(bytes * 2)?;
                            }
                            disk.request(bytes * 2, || ());
                            Ok(())
                        })
                        .unwrap();
                    assert_eq!(
                        std::fs::read(fetched.path()).unwrap(),
                        vec![index as u8; 8 * 1024 * 1024]
                    );
                    progress.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
    });
    let elapsed = start.elapsed().as_secs_f64();
    let foreground = foreground.into_inner().unwrap();
    let competitor = competitor.into_inner().unwrap();
    let state = disk.state.into_inner().unwrap();
    serde_json::json!({
        "elapsed_s": elapsed, "progress": progress.load(Ordering::Relaxed),
        "foreground_samples": foreground.len(), "foreground_p95_ms": percentile(foreground, 0.95),
        "competitor_samples": competitor.len(), "competitor_p95_ms": percentile(competitor, 0.95),
        "disk_request_p95_ms": percentile(state.latencies, 0.95), "max_queue": state.max_queue,
        "transferred_bytes": state.bytes, "throughput_mib_s": state.bytes as f64 / elapsed / 1024.0 / 1024.0,
    })
}

#[test]
#[ignore = "wall-clock shared storage experiment; run explicitly with --ignored --nocapture"]
fn shared_hdd_scenario() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    for dir in [
        "assets",
        "scratch",
        "data/cache/previews",
        "data/cache/thumbnails",
    ] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    for index in 0..4 {
        std::fs::write(
            root.join(format!("assets/{index}.bin")),
            vec![index as u8; 8 * 1024 * 1024],
        )
        .unwrap();
        std::fs::write(
            root.join(format!("data/cache/previews/{index}.dmsh")),
            vec![0; 8 * 1024 * 1024],
        )
        .unwrap();
    }
    std::fs::write(root.join("data/cache/thumbnails/hit.png"), vec![0; 4096]).unwrap();
    std::fs::write(root.join("competitor.bin"), vec![0; 4096]).unwrap();
    let store = Store::open(&root.join("data")).unwrap();
    let source_id = store
        .add_source(
            &SourceConnection::LocalFs {
                root: root.join("assets").display().to_string(),
            },
            "scenario",
            false,
        )
        .unwrap();
    for index in 0..128 {
        store
            .upsert_asset(&NewAsset {
                source_id,
                path: format!("{index}.bin"),
                filename: format!("{index}.bin"),
                content_hash: None,
                size_bytes: Some(8 * 1024 * 1024),
                source_modified_at: Some(0),
                scanned_at: 0,
                media_type: MediaType::Image,
                format: "png".to_string(),
            })
            .unwrap();
    }
    drop(store);
    let mut report = serde_json::Map::new();
    for (name, cold) in [("cold_cache", true), ("sustained_copy", false)] {
        let before = phase(root, cold, false);
        let after = phase(root, cold, true);
        // Foreground and co-tenant budgets: p95 <= 60 ms. Legacy bulk work must reproduce a
        // violation; otherwise this fixture cannot detect the regression it claims to cover.
        for metric in ["foreground_p95_ms", "competitor_p95_ms"] {
            assert!(
                before[metric].as_f64().unwrap() > 60.0,
                "baseline must violate {metric}: {before}"
            );
            assert!(
                after[metric].as_f64().unwrap() <= 60.0,
                "paced work must meet {metric}: {after}"
            );
        }
        assert_eq!(after["progress"], 4);
        report.insert(
            name.into(),
            serde_json::json!({ "before": before, "after": after }),
        );
    }
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
