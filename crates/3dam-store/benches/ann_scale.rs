//! Reproducible 100k/1M lifecycle evidence for issue #141.
//!
//! Examples:
//! `DAM_ANN_BENCH_SIZE=100000 cargo bench -p dam-store --features ann-bench --bench ann_scale`
//! `DAM_ANN_BENCH_SIZE=1000000 DAM_ANN_BENCH_DIM=512 cargo bench -p dam-store --features ann-bench --bench ann_scale`

use dam_api::id::AssetId;
fn main() {
    let count = configured("DAM_ANN_BENCH_SIZE", 100_000);
    assert!(
        matches!(count, 100_000 | 1_000_000),
        "benchmark size must be an acceptance scale: 100000 or 1000000"
    );
    let dim = configured("DAM_ANN_BENCH_DIM", 512);
    let query_count = configured("DAM_ANN_BENCH_QUERIES", 50);
    let candidate_limit = configured("DAM_ANN_BENCH_CANDIDATES", 80);
    assert!(
        (10..=8_192).contains(&candidate_limit),
        "candidate count must cover top-10 truth and stay within the product bound"
    );
    let mut items = Vec::with_capacity(count);
    let mut ids = Vec::with_capacity(count);
    for index in 0..count {
        let id = AssetId::new();
        ids.push(id);
        items.push((id, representative_vector(index, dim)));
    }
    let queries: Vec<_> = (0..query_count)
        .map(|index| {
            let group = (index * 997) % (count / 10);
            (
                representative_vector(group * 10, dim),
                ids[group * 10..group * 10 + 10].to_vec(),
            )
        })
        .collect();
    let vectors_kib = count.saturating_mul(dim).saturating_mul(4) / 1024;
    let result = dam_store::benchmark_ann_build(items, &queries, candidate_limit);
    println!(
        "{{\"backend\":\"usearch-2.25.3-f16-ef2048\",\"vectors\":{count},\"dims\":{dim},\
         \"build_ms\":{},\"serialize_ms\":{},\"deserialize_ms\":{},\
         \"ann_lookup_p50_us\":{},\"exact_rerank_cpu_p50_us\":{},\
         \"candidate_limit\":{candidate_limit},\
         \"product_candidate_recall_at_10\":{:.6},\"candidate_count\":{},\
         \"sidecar_bytes\":{},\"vector_payload_kib\":{vectors_kib},\
         \"rss_before_kib\":{},\"rss_index_kib\":{},\"graph_rss_delta_kib\":{},\
         \"rss_lifecycle_peak_kib\":{}}}",
        result.build.as_millis(),
        result.serialize.as_millis(),
        result.deserialize.as_millis(),
        result.search_p50.as_micros(),
        result.exact_rerank_p50.as_micros(),
        result.recall_at_k,
        result.candidate_count,
        result.sidecar_bytes,
        result.rss_before_kib,
        result.rss_index_kib,
        result.graph_rss_delta_kib,
        result.rss_lifecycle_peak_kib,
    );
}

fn configured(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn representative_vector(seed: usize, dim: usize) -> Vec<f32> {
    // Ten tight points per well-separated deterministic cluster give an exact known top-10. This
    // makes recall measurement O(n) to generate rather than hiding a second O(n*q*d) exact scan
    // inside a benchmark intended to measure the index lifecycle itself.
    let group = seed / 10;
    let member = seed % 10;
    let mut state = (group as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut vector = Vec::with_capacity(dim);
    for _ in 0..dim {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        vector.push(((state >> 40) as f32 / (1u32 << 24) as f32) - 0.5);
    }
    vector[(group + member * 31) % dim] += member as f32 * 0.0001;
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    vector.iter_mut().for_each(|value| *value /= norm);
    vector
}
