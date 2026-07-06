//! Spike: vector index at scale — sqlite-vec (embedded) vs usearch (sidecar HNSW).
//!
//! Settles the tech-spec 02/05 open question: for 3DAM's similarity index at 1M+ assets,
//! embed the ANN in SQLite (sqlite-vec) or run a standalone HNSW crate (usearch), and how do
//! build time / query latency / recall / on-disk size compare.
//!
//! Usage: spike-vector-index [N] [DIM] [Q]      (defaults: 1_000_000  512  200)
//!
//! Data is synthetic: N unit-normalised random vectors + Q separate query vectors. Cosine
//! throughout. Ground-truth top-K is computed by a rayon brute-force scan; recall@K is the
//! overlap between each engine's top-K and that ground truth.

use rayon::prelude::*;
use std::time::Instant;

const K: usize = 10;
const OUT_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// Tiny deterministic PRNG (splitmix64) — no rand dependency, reproducible across runs.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    /// Cheap centred uniform in ~[-0.5, 0.5]; fine for a mechanical benchmark.
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
}

fn normalize(row: &mut [f32]) {
    let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in row.iter_mut() {
        *x /= norm;
    }
}

fn gen_normalized(n: usize, dim: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng(seed);
    let mut v = vec![0.0f32; n * dim];
    for row in v.chunks_mut(dim) {
        for x in row.iter_mut() {
            *x = rng.next_f32();
        }
        normalize(row);
    }
    v
}

/// Clustered data — the realistic case for embeddings, which live on a manifold, not uniformly
/// in the hypercube. `n` vectors are drawn near `c` random centroids with noise `eps`; ANN
/// recall is only meaningful on data with real neighbourhood structure (uniform-random vectors
/// are the pathological worst case where nearest neighbours are barely nearer than average).
fn gen_clustered(centroids: &[f32], n: usize, dim: usize, eps: f32, seed: u64) -> Vec<f32> {
    let c = centroids.len() / dim;
    let mut rng = Rng(seed);
    let mut v = vec![0.0f32; n * dim];
    for (i, row) in v.chunks_mut(dim).enumerate() {
        let ctr = &centroids[(i % c) * dim..(i % c + 1) * dim];
        for (x, &cx) in row.iter_mut().zip(ctr) {
            *x = cx + eps * rng.next_f32();
        }
        normalize(row);
    }
    v
}

fn percentiles(mut us: Vec<f64>) -> (f64, f64, f64) {
    us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |p: f64| us[((us.len() as f64 * p) as usize).min(us.len() - 1)];
    (pick(0.50), pick(0.95), pick(0.99))
}

/// Overlap of `got` (engine top-K keys) with `truth` (exact top-K keys).
fn recall(got: &[u64], truth: &[u64]) -> f64 {
    let hits = got.iter().filter(|k| truth.contains(k)).count();
    hits as f64 / truth.len() as f64
}

struct Res {
    name: &'static str,
    build_s: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    recall: f64,
    disk_mb: f64,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let dim: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(512);
    let q: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
    let ef: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(64);
    std::env::set_var("SPIKE_EF", ef.to_string());

    println!("== vector-index spike ==  N={n} DIM={dim} Q={q} K={K}");
    println!("raw vectors: {:.2} GB f32 in RAM\n", (n * dim * 4) as f64 / 1e9);

    let t = Instant::now();
    let n_centroids = (n / 500).clamp(64, 4096);
    let centroids = gen_normalized(n_centroids, dim, 0xCE47);
    // queries sit near the same centroids (with independent noise) so they have real neighbours.
    let data = gen_clustered(&centroids, n, dim, 0.15, 0xA11CE);
    let queries = gen_clustered(&centroids, q, dim, 0.15, 0xB0B);
    println!("gen: {:?} ({n_centroids} clusters)", t.elapsed());
    let _ = gen_normalized; // kept for reference / uniform-random comparison

    // ---- Ground truth: exact top-K by cosine (dot, since normalised), parallel scan ----
    let t = Instant::now();
    let truth: Vec<Vec<u64>> = queries
        .par_chunks(dim)
        .map(|qv| {
            let mut best: Vec<(f32, u64)> = Vec::with_capacity(K + 1);
            for (i, dv) in data.chunks(dim).enumerate() {
                let dot: f32 = qv.iter().zip(dv).map(|(a, b)| a * b).sum();
                if best.len() < K {
                    best.push((dot, i as u64));
                    best.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
                } else if dot > best[K - 1].0 {
                    best[K - 1] = (dot, i as u64);
                    best.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
                }
            }
            best.into_iter().map(|(_, i)| i).collect()
        })
        .collect();
    let gt_ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "ground-truth brute force: {:.0} ms total, {:.2} ms/query (the naive baseline)\n",
        gt_ms,
        gt_ms / q as f64
    );

    let usearch_res = bench_usearch(&data, &queries, n, dim, q, &truth);
    let sqlitevec_res = bench_sqlite_vec(&data, &queries, n, dim, q, &truth);

    println!("\n================= SUMMARY =================");
    println!(
        "{:<12} {:>9} {:>10} {:>10} {:>10} {:>10} {:>9}",
        "engine", "build", "q_p50", "q_p95", "q_p99", "recall@10", "disk"
    );
    for r in [&usearch_res, &sqlitevec_res] {
        println!(
            "{:<12} {:>7.1}s {:>8.3}ms {:>8.3}ms {:>8.3}ms {:>9.1}% {:>7.0}MB",
            r.name, r.build_s, r.p50_ms, r.p95_ms, r.p99_ms, r.recall * 100.0, r.disk_mb
        );
    }
    println!(
        "{:<12} {:>7} {:>8.3}ms {:>10} {:>10} {:>9}   (exact)",
        "brute-force", "-", gt_ms / q as f64, "-", "-", "100.0%"
    );
}

fn bench_usearch(
    data: &[f32],
    queries: &[f32],
    n: usize,
    dim: usize,
    q: usize,
    truth: &[Vec<u64>],
) -> Res {
    use usearch::{ffi::IndexOptions, MetricKind, ScalarKind};
    println!("---- usearch (HNSW) ----");
    let opts = IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 16,      // M — graph degree
        expansion_add: 128,    // ef_construction — build-time candidate breadth
        expansion_search: std::env::var("SPIKE_EF").ok().and_then(|s| s.parse().ok()).unwrap_or(64), // ef_search
        multi: false,
    };
    let index = usearch::new_index(&opts).expect("new_index");
    index.reserve(n).expect("reserve");

    // usearch supports concurrent insertion — add in parallel across rayon threads.
    let t = Instant::now();
    data.par_chunks(dim).enumerate().for_each(|(i, v)| {
        index.add(i as u64, v).expect("add");
    });
    let build_s = t.elapsed().as_secs_f64();
    println!("build (add {n}): {:.2}s  ({:.0} vec/s)", build_s, n as f64 / build_s);

    let path = format!("{OUT_DIR}/out-usearch.idx");
    index.save(&path).expect("save");
    let disk_mb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1e6;

    let mut lat = Vec::with_capacity(q);
    let mut rec = 0.0;
    for (qi, qv) in queries.chunks(dim).enumerate() {
        let t = Instant::now();
        let m = index.search(qv, K).expect("search");
        lat.push(t.elapsed().as_secs_f64() * 1e3);
        rec += recall(&m.keys, &truth[qi]);
    }
    let (p50, p95, p99) = percentiles(lat);
    let recall = rec / q as f64;
    println!("query p50={p50:.3}ms p99={p99:.3}ms  recall@{K}={:.1}%  disk={disk_mb:.0}MB\n", recall * 100.0);
    Res { name: "usearch", build_s, p50_ms: p50, p95_ms: p95, p99_ms: p99, recall, disk_mb }
}

fn bench_sqlite_vec(
    data: &[f32],
    queries: &[f32],
    n: usize,
    dim: usize,
    q: usize,
    truth: &[Vec<u64>],
) -> Res {
    use rusqlite::{ffi::sqlite3_auto_extension, Connection};
    use sqlite_vec::sqlite3_vec_init;
    println!("---- sqlite-vec (vec0) ----");
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())));
    }
    let path = format!("{OUT_DIR}/out-sqlite-vec.db");
    let _ = std::fs::remove_file(&path);
    let conn = Connection::open(&path).expect("open db");
    conn.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;").ok();
    conn.execute(
        &format!("CREATE VIRTUAL TABLE vec USING vec0(embedding float[{dim}] distance_metric=cosine)"),
        [],
    )
    .expect("create vec0");

    let t = Instant::now();
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut stmt = conn.prepare("INSERT INTO vec(rowid, embedding) VALUES (?1, ?2)").unwrap();
        for (i, v) in data.chunks(dim).enumerate() {
            let blob: &[u8] = bytemuck::cast_slice(v);
            stmt.execute(rusqlite::params![i as i64, blob]).expect("insert");
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    let build_s = t.elapsed().as_secs_f64();
    println!("build (insert {n}): {:.2}s  ({:.0} row/s)", build_s, n as f64 / build_s);

    // vec0 is EXACT (linear scan) in 0.1.x — expect recall 100% but O(N) queries.
    let mut lat = Vec::with_capacity(q);
    let mut rec = 0.0;
    let mut stmt = conn
        .prepare("SELECT rowid FROM vec WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2")
        .expect("prepare knn");
    for (qi, qv) in queries.chunks(dim).enumerate() {
        let blob: &[u8] = bytemuck::cast_slice(qv);
        let t = Instant::now();
        let got: Vec<u64> = stmt
            .query_map(rusqlite::params![blob, K as i64], |r| r.get::<_, i64>(0).map(|x| x as u64))
            .expect("query")
            .map(|x| x.unwrap())
            .collect();
        lat.push(t.elapsed().as_secs_f64() * 1e3);
        rec += recall(&got, &truth[qi]);
    }
    let (p50, p95, p99) = percentiles(lat);
    let recall = rec / q as f64;
    let disk_mb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1e6;
    println!("query p50={p50:.3}ms p99={p99:.3}ms  recall@{K}={:.1}%  disk={disk_mb:.0}MB\n", recall * 100.0);
    Res { name: "sqlite-vec", build_s, p50_ms: p50, p95_ms: p95, p99_ms: p99, recall, disk_mb }
}
