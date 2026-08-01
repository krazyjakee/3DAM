//! Transfer-size/CPU benchmark for issue #149.
//!
//! Run with `cargo bench -p dam-server --bench compression`. When a web build is present the
//! benchmark uses its largest JS and WASM artifacts; otherwise deterministic stand-ins keep it
//! runnable in a fresh source checkout. JSON models a large query page.

use flate2::write::GzEncoder;
use flate2::Compression;
use std::fs;
use std::hint::black_box;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SAMPLES: u32 = 20;

fn largest_asset(extension: &str) -> Option<Vec<u8>> {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist/assets");
    let mut candidates: Vec<PathBuf> = fs::read_dir(assets)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some(extension))
        .collect();
    candidates.sort_by_key(|path| {
        fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    });
    fs::read(candidates.pop()?).ok()
}

fn representative_js() -> Vec<u8> {
    largest_asset("js").unwrap_or_else(|| {
        let module = b"export function tile(asset){return {id:asset.id,name:asset.name,tags:asset.tags.map(String)}};\n";
        module.iter().copied().cycle().take(553 * 1024).collect()
    })
}

fn representative_wasm() -> Vec<u8> {
    largest_asset("wasm").unwrap_or_else(|| {
        let mut wasm = b"\0asm\x01\0\0\0".to_vec();
        wasm.extend((0_u32..(4 * 1024 * 1024 / 4)).flat_map(u32::to_le_bytes));
        wasm
    })
}

fn representative_json() -> Vec<u8> {
    let assets: Vec<_> = (0..2_000)
        .map(|index| {
            serde_json::json!({
                "id": format!("asset-{index:08}"),
                "name": format!("production_texture_{index}.png"),
                "media_type": "image",
                "tags": ["approved", "environment", "tileable"],
                "width": 4096,
                "height": 4096,
                "favorite": index % 7 == 0,
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "items": assets,
        "next": "asset-00002000",
    }))
    .unwrap()
}

fn gzip(input: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

fn brotli(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        // Quality 4 is representative of streamed HTTP response compression; static sidecars use
        // the slower maximum-quality encoder once during the web build.
        let mut encoder = brotli::CompressorWriter::new(&mut output, 4096, 4, 22);
        encoder.write_all(input).unwrap();
    }
    output
}

fn average_runtime(input: &[u8], encode: fn(&[u8]) -> Vec<u8>) -> (Duration, usize) {
    let started = Instant::now();
    let mut size = 0;
    for _ in 0..SAMPLES {
        size = black_box(encode(black_box(input))).len();
    }
    (started.elapsed() / SAMPLES, size)
}

fn report(name: &str, input: &[u8]) {
    let (gzip_cpu, gzip_size) = average_runtime(input, gzip);
    let (brotli_cpu, brotli_size) = average_runtime(input, brotli);
    println!(
        "{name:<5} identity={:>8} B  gzip={:>8} B ({:>5.1}%, {:>8?})  br={:>8} B ({:>5.1}%, {:>8?})",
        input.len(),
        gzip_size,
        gzip_size as f64 * 100.0 / input.len() as f64,
        gzip_cpu,
        brotli_size,
        brotli_size as f64 * 100.0 / input.len() as f64,
        brotli_cpu,
    );
}

fn main() {
    println!("average encode CPU over {SAMPLES} samples (lower is better)");
    report("JS", &representative_js());
    report("WASM", &representative_wasm());
    report("JSON", &representative_json());
}
