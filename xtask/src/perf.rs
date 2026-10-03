use dam_api::dto::{AnalyzeRequest, DupRequest, JobState, QueryRequest};
use dam_api::id::{AssetId, ContentHash, SourceId};
use dam_api::page::{Cursor, PageParams};
use dam_api::service::{AuthContext, LibraryService, Visibility};
use dam_core::{EmbeddedLibrary, ResourceOptions};
use dam_store::{ExportSelection, NewAsset, Store};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RECIPE_VERSION: &str = "3dam-scale-v1";
const FIXED_SEED: u64 = 0x3da1_2026_0132;
const DEFAULT_PROFILES: &str = "perf/profiles.json";
const DEFAULT_BASELINES: &str = "perf/baselines.json";
const WORK_DIR_MARKER: &str = ".3dam-perf-workdir";

const STORE_METRICS: &[&str] = &[
    "first_page_ms",
    "late_page_ms",
    "search_ms",
    "faceted_query_ms",
    "stats_ms",
    "tag_facets_ms",
    "scan_upsert_assets_per_second",
    "browse_under_write_ms",
    "browse_under_analyze_ms",
    "analysis_rayon_core_utilization_pct",
    "aggregate_write_overhead_ratio",
    "analysis_plan_ms",
    "duplicate_detection_ms",
    "export_assets_per_second",
    "peak_rss_bytes",
    "browser_initial_payload_bytes",
    "derivative_cache_first_thumbnail_hit_ms",
];
const BROWSER_METRICS: &[&str] = &["browser_long_scroll_p99_ms", "browser_heap_growth_bytes"];

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    asset_count: usize,
    source_count: usize,
    max_depth: usize,
    tag_count: usize,
    embedding_every: usize,
    embedding_dim: usize,
    duplicate_every: usize,
    samples: usize,
    upsert_sample: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfilesFile {
    schema_version: u32,
    profiles: BTreeMap<String, Profile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselinesFile {
    schema_version: u32,
    profiles: BTreeMap<String, BaselineProfile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineProfile {
    metrics: BTreeMap<String, BaselineMetric>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineMetric {
    reference: f64,
    max_ratio: f64,
    direction: Direction,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Direction {
    LowerIsBetter,
    HigherIsBetter,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    generated_at_unix_ms: u128,
    fixture_recipe: FixtureRecipe,
    profile_name: String,
    profile: Profile,
    machine: Machine,
    browser: BrowserStatus,
    fixture_generate_seconds: f64,
    database_bytes: u64,
    metrics: BTreeMap<String, Metric>,
    comparisons: BTreeMap<String, Comparison>,
    passed: bool,
}

#[derive(Serialize)]
struct FixtureRecipe {
    version: &'static str,
    seed: u64,
    migration_path: &'static str,
}

#[derive(Serialize)]
struct Machine {
    os: String,
    arch: String,
    logical_cpus: usize,
    cpu_model: Option<String>,
    total_memory_bytes: Option<u64>,
    rustc: Option<String>,
    git_sha: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BrowserStatus {
    Measured {
        retained_pages: usize,
        cursor_entries: usize,
    },
    Failed {
        message: String,
    },
    Skipped {
        reason: String,
    },
}

#[derive(Serialize)]
struct Metric {
    value: f64,
    p95: f64,
    unit: &'static str,
    direction: Direction,
    samples: Vec<f64>,
}

#[derive(Serialize)]
struct Comparison {
    reference: f64,
    ratio: Option<f64>,
    max_ratio: f64,
    direction: Direction,
    status: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserResult {
    retained_pages: usize,
    cursor_entries: usize,
    heap_growth_bytes: f64,
    p99_ms: f64,
}

struct Options {
    profile: String,
    profiles_path: PathBuf,
    baseline_path: PathBuf,
    output: PathBuf,
    work_dir: PathBuf,
    skip_browser: bool,
    keep_catalog: bool,
}

pub fn run(args: Vec<String>) -> bool {
    match execute(parse_args(args)) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("performance harness failed: {error}");
            false
        }
    }
}

fn execute(options: Result<Options, String>) -> Result<(), String> {
    let options = validate_paths(options?)?;
    let profiles: ProfilesFile = load_json(&options.profiles_path)?;
    if profiles.schema_version != 1 {
        return Err(format!(
            "unsupported profile schema {}",
            profiles.schema_version
        ));
    }
    let profile = profiles
        .profiles
        .get(&options.profile)
        .cloned()
        .ok_or_else(|| format!("unknown profile '{}'", options.profile))?;
    validate_profile(&profile)?;
    let baselines: BaselinesFile = load_json(&options.baseline_path)?;
    if baselines.schema_version != 1 {
        return Err(format!(
            "unsupported baseline schema {}",
            baselines.schema_version
        ));
    }
    let baseline = baselines
        .profiles
        .get(&options.profile)
        .ok_or_else(|| format!("baseline has no '{}' profile", options.profile))?;
    validate_baseline(baseline)?;

    prepare_work_dir(&options.work_dir)?;

    eprintln!(
        "generating {} deterministic assets ({}, seed {FIXED_SEED:#x})",
        profile.asset_count, RECIPE_VERSION
    );
    let generation_started = Instant::now();
    generate_catalog(&options.work_dir, &profile)?;
    let thumbnail_hit = populate_derivative_cache(&options.work_dir, profile.asset_count)?;
    let analysis_targets = prepare_analysis_fixture(&options.work_dir, &profile)?;
    let fixture_generate_seconds = generation_started.elapsed().as_secs_f64();
    let database_path = options.work_dir.join("library.db");
    let database_bytes = fs::metadata(&database_path)
        .map_err(|e| format!("stat {}: {e}", database_path.display()))?
        .len();

    let derivative_cache_first_thumbnail_hit_ms =
        dam_core::measure_derivative_cache_first_hit(&options.work_dir, &thumbnail_hit)
            .map_err(|e| format!("measure derivative cache first hit: {e}"))?
            .as_secs_f64()
            * 1_000.0;

    let store = Store::open(&options.work_dir).map_err(|e| format!("open fixture: {e}"))?;
    let mut metrics = benchmark_store(
        &store,
        &profile,
        &database_path,
        &options.work_dir,
        analysis_targets,
    )?;
    insert_metric(
        &mut metrics,
        "derivative_cache_first_thumbnail_hit_ms",
        derivative_cache_first_thumbnail_hit_ms,
        "ms",
        Direction::LowerIsBetter,
        vec![derivative_cache_first_thumbnail_hit_ms],
    )?;
    let browser = if options.skip_browser {
        BrowserStatus::Skipped {
            reason: "requested with --skip-browser".into(),
        }
    } else {
        match benchmark_browser(profile.asset_count) {
            Ok(result) => {
                insert_metric(
                    &mut metrics,
                    "browser_long_scroll_p99_ms",
                    result.p99_ms,
                    "ms",
                    Direction::LowerIsBetter,
                    vec![result.p99_ms],
                )?;
                insert_metric(
                    &mut metrics,
                    "browser_heap_growth_bytes",
                    result.heap_growth_bytes,
                    "bytes",
                    Direction::LowerIsBetter,
                    vec![result.heap_growth_bytes],
                )?;
                BrowserStatus::Measured {
                    retained_pages: result.retained_pages,
                    cursor_entries: result.cursor_entries,
                }
            }
            Err(message) => BrowserStatus::Failed { message },
        }
    };

    let browser_failed = matches!(browser, BrowserStatus::Failed { .. });
    let (comparisons, comparisons_passed) =
        compare(&metrics, baseline, options.skip_browser, browser_failed)?;
    let browser_passed = !matches!(browser, BrowserStatus::Failed { .. });
    let passed = comparisons_passed && browser_passed;
    let report = Report {
        schema_version: 1,
        generated_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
        fixture_recipe: FixtureRecipe {
            version: RECIPE_VERSION,
            seed: FIXED_SEED,
            migration_path: "Store::open followed by fixture recipe v1",
        },
        profile_name: options.profile.clone(),
        profile,
        machine: machine_metadata(),
        browser,
        fixture_generate_seconds,
        database_bytes,
        metrics,
        comparisons,
        passed,
    };
    if let Some(parent) = options.output.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create report directory {}: {e}", parent.display()))?;
    }
    fs::write(
        &options.output,
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("write {}: {e}", options.output.display()))?;
    eprintln!("performance report: {}", options.output.display());

    drop(store);
    if !options.keep_catalog {
        remove_owned_work_dir(&options.work_dir)?;
    }
    if passed {
        Ok(())
    } else {
        Err("one or more performance thresholds failed; see the JSON report".into())
    }
}

fn parse_args(args: Vec<String>) -> Result<Options, String> {
    let mut profile = None;
    let mut profiles_path = PathBuf::from(DEFAULT_PROFILES);
    let mut baseline_path = PathBuf::from(DEFAULT_BASELINES);
    let mut output = None;
    let mut work_dir = None;
    let mut skip_browser = false;
    let mut keep_catalog = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--profile" => profile = Some(next_value(&args, &mut index, "--profile")?),
            "--profiles" => profiles_path = next_value(&args, &mut index, "--profiles")?.into(),
            "--baseline" => baseline_path = next_value(&args, &mut index, "--baseline")?.into(),
            "--output" => output = Some(PathBuf::from(next_value(&args, &mut index, "--output")?)),
            "--work-dir" => {
                work_dir = Some(PathBuf::from(next_value(&args, &mut index, "--work-dir")?))
            }
            "--skip-browser" => skip_browser = true,
            "--keep-catalog" => keep_catalog = true,
            "--help" | "-h" => {
                return Err("usage: cargo xtask perf --profile smoke|100k|1m [--output FILE] [--work-dir DIR] [--profiles FILE] [--baseline FILE] [--skip-browser] [--keep-catalog]".into());
            }
            other => return Err(format!("unknown perf argument '{other}'")),
        }
        index += 1;
    }
    let profile = profile.ok_or("--profile is required")?;
    Ok(Options {
        output: output.unwrap_or_else(|| PathBuf::from(format!("perf-results/{profile}.json"))),
        work_dir: work_dir
            .unwrap_or_else(|| PathBuf::from(format!("target/perf-catalog-{profile}"))),
        profile,
        profiles_path,
        baseline_path,
        skip_browser,
        keep_catalog,
    })
}

fn validate_paths(mut options: Options) -> Result<Options, String> {
    let current = fs::canonicalize(std::env::current_dir().map_err(|e| e.to_string())?)
        .map_err(|e| format!("resolve current directory: {e}"))?;
    options.work_dir = resolve_path(&options.work_dir)?;
    options.output = resolve_path(&options.output)?;
    let is_root = options.work_dir.parent().is_none();
    if is_root || current.starts_with(&options.work_dir) {
        return Err(format!(
            "refusing destructive work directory '{}': it is a filesystem root, the workspace, or an ancestor of the workspace",
            options.work_dir.display()
        ));
    }
    if options.output.starts_with(&options.work_dir) {
        return Err(format!(
            "report '{}' must be outside disposable work directory '{}'",
            options.output.display(),
            options.work_dir.display()
        ));
    }
    Ok(options)
}

fn prepare_work_dir(path: &Path) -> Result<(), String> {
    if path.exists() {
        remove_owned_work_dir(path)?;
    }
    fs::create_dir_all(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    fs::write(path.join(WORK_DIR_MARKER), RECIPE_VERSION)
        .map_err(|e| format!("mark disposable directory {}: {e}", path.display()))
}

fn remove_owned_work_dir(path: &Path) -> Result<(), String> {
    let marker = path.join(WORK_DIR_MARKER);
    let owner = fs::read_to_string(&marker).map_err(|_| {
        format!(
            "refusing to remove existing unowned work directory '{}'; choose an empty path or a directory created by this harness",
            path.display()
        )
    })?;
    if owner.trim() != RECIPE_VERSION {
        return Err(format!(
            "refusing to remove work directory '{}' with an unknown ownership marker",
            path.display()
        ));
    }
    fs::remove_dir_all(path).map_err(|e| format!("remove fixture {}: {e}", path.display()))
}

/// Resolve an existing path exactly, or canonicalize its nearest existing ancestor and append the
/// not-yet-created suffix. This closes `..` and symlink escapes before any recursive deletion.
fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let mut ancestor = absolute.clone();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| format!("cannot resolve path '{}'", path.display()))?;
        suffix.push(name.to_os_string());
        if !ancestor.pop() {
            return Err(format!("cannot resolve path '{}'", path.display()));
        }
    }
    let mut resolved =
        fs::canonicalize(&ancestor).map_err(|e| format!("resolve {}: {e}", ancestor.display()))?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn validate_profile(profile: &Profile) -> Result<(), String> {
    if profile.asset_count < 100
        || profile.source_count < 4
        || profile.max_depth < 2
        || profile.tag_count < 4
        || profile.embedding_every == 0
        || profile.embedding_dim < 4
        || profile.duplicate_every < 4
        || profile.samples == 0
        || profile.upsert_sample == 0
        || profile.upsert_sample > profile.asset_count
    {
        return Err("invalid profile: counts, samples, depth and fixture intervals must be positive and representative".into());
    }
    Ok(())
}

fn validate_baseline(profile: &BaselineProfile) -> Result<(), String> {
    let expected: BTreeSet<&str> = STORE_METRICS
        .iter()
        .chain(BROWSER_METRICS.iter())
        .copied()
        .collect();
    let actual: BTreeSet<&str> = profile.metrics.keys().map(String::as_str).collect();
    if actual != expected {
        let unknown: Vec<_> = actual.difference(&expected).copied().collect();
        let missing: Vec<_> = expected.difference(&actual).copied().collect();
        return Err(format!(
            "baseline metric keys do not match the harness (unknown={unknown:?}, missing={missing:?})"
        ));
    }
    for (name, metric) in &profile.metrics {
        if !metric.reference.is_finite()
            || metric.reference <= 0.0
            || !metric.max_ratio.is_finite()
            || metric.max_ratio < 1.0
        {
            return Err(format!("invalid baseline values for {name}"));
        }
    }
    Ok(())
}

fn generate_catalog(data_dir: &Path, profile: &Profile) -> Result<(), String> {
    // The supported API creates and migrates the database first. The fixture recipe below is
    // versioned because it deliberately populates the public schema in large transactions; using
    // Store::upsert_asset one million times would benchmark transaction setup, not dataset shape.
    drop(Store::open(data_dir).map_err(|e| format!("migrate fixture: {e}"))?);
    let mut connection =
        Connection::open(data_dir.join("library.db")).map_err(|e| e.to_string())?;
    connection
        .execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
        )
        .map_err(|e| e.to_string())?;
    // Maintaining every ancestor folder row per asset is correct for production's streamed scan,
    // but turns deterministic fixture construction into millions of recursive trigger walks. The
    // fixture recipe is already a bulk loader, so suspend only that derived read model, rebuild it
    // set-wise after the asset load, and restore the exact migrated trigger SQL before measuring.
    // Aggregate triggers deliberately remain live: generation and the write-overhead metric both
    // exercise the production V26 maintenance path.
    let folder_triggers = suspend_triggers(&connection, "folder_")?;
    if folder_triggers.is_empty() {
        return Err("migrated fixture schema had no folder triggers to suspend".into());
    }
    let now = 1_700_000_000_000_i64;
    let source_connections = [
        r#"{"kind":"local_fs","root":"/fixture/local"}"#,
        r#"{"kind":"sftp","host":"fixture.invalid","port":22,"username":"bench","base_path":"/assets"}"#,
        r#"{"kind":"smb","host":"fixture.invalid","port":445,"share":"assets","base_path":"library","username":"bench"}"#,
        r#"{"kind":"federated","endpoint":"https://fixture.invalid"}"#,
    ];
    let source_kinds = ["local_fs", "sftp", "smb", "federated"];
    {
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        for index in 0..profile.source_count {
            transaction
                .execute(
                    "INSERT INTO source(id,name,kind,connection,online,watch,created_at,updated_at) VALUES(?,?,?,?,1,0,?,?)",
                    params![deterministic_id(1, index as u64).to_vec(), format!("fixture-source-{index}"), source_kinds[index % 4], source_connections[index % 4], now, now],
                )
                .map_err(|e| e.to_string())?;
        }
        for index in 0..profile.tag_count {
            transaction
                .execute(
                    "INSERT INTO tag(id,name) VALUES(?,?)",
                    params![
                        deterministic_id(2, index as u64).to_vec(),
                        format!("fixture-tag-{index:03}")
                    ],
                )
                .map_err(|e| e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())?;
    }

    for batch_start in (0..profile.asset_count).step_by(10_000) {
        let batch_end = (batch_start + 10_000).min(profile.asset_count);
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        for index in batch_start..batch_end {
            let asset_id = deterministic_id(3, index as u64);
            let source = index % profile.source_count;
            let (media, extension) = match index % 5 {
                0 => ("image", "png"),
                1 => ("audio", "wav"),
                2 => ("model", "glb"),
                3 => ("video", "mp4"),
                _ => ("document", "pdf"),
            };
            let filename = if index % 10 == 0 {
                format!("hero_texture_{index:09}.{extension}")
            } else {
                format!("asset_{index:09}.{extension}")
            };
            let depth = 2 + index % (profile.max_depth - 1);
            let folders = (0..depth)
                .map(|level| format!("level{level:02}_{}", (index / (level + 1)) % 97))
                .collect::<Vec<_>>()
                .join("/");
            let path = format!("{folders}/{filename}");
            let remainder = index % profile.duplicate_every;
            let hash_index = if remainder < 3 {
                index - remainder
            } else {
                index
            };
            let hash = deterministic_hash(hash_index as u64);
            transaction
                .execute(
                    "INSERT INTO asset(id,content_hash,source_id,path,filename,size_bytes,source_modified_at,scanned_at,media_type,format,analysis_version,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,0,?,?)",
                    params![asset_id.to_vec(), hash.to_vec(), deterministic_id(1, source as u64).to_vec(), path, filename, 1024_i64 + (index % 1_000_000) as i64, now + index as i64, now, media, extension, now, now],
                )
                .map_err(|e| format!("insert asset {index}: {e}"))?;
            let first_tag = index % profile.tag_count;
            let second_tag = (index * 17 + 3) % profile.tag_count;
            for tag in [first_tag, second_tag] {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO asset_tag(asset_id,tag_id,state,source,confidence,extractor,created_at) VALUES(?,?,'confirmed','auto',0.9,?,?)",
                        params![asset_id.to_vec(), deterministic_id(2, tag as u64).to_vec(), RECIPE_VERSION, now],
                    )
                    .map_err(|e| e.to_string())?;
            }
            transaction
                .execute(
                    "UPDATE asset_fts SET tags=?, folder=? WHERE rowid=(SELECT rowid FROM asset WHERE id=?)",
                    params![format!("fixture-tag-{first_tag:03} fixture-tag-{second_tag:03}"), folders.replace('/', " ").to_lowercase(), asset_id.to_vec()],
                )
                .map_err(|e| e.to_string())?;
            if index % profile.embedding_every == 0 {
                let embedding = deterministic_embedding(index, profile.embedding_dim);
                transaction
                    .execute(
                        "INSERT INTO embedding(asset_id,space_id,media_type,dim,vec,extractor,created_at) VALUES(?,?,?,?,?,?,?)",
                        params![asset_id.to_vec(), format!("fixture-{media}-{}d", profile.embedding_dim), media, profile.embedding_dim as i64, embedding, RECIPE_VERSION, now],
                    )
                    .map_err(|e| e.to_string())?;
            }
        }
        transaction.commit().map_err(|e| e.to_string())?;
        if profile.asset_count >= 100_000 && batch_end % 100_000 == 0 {
            eprintln!("generated {batch_end}/{} assets", profile.asset_count);
        }
    }
    rebuild_folders(&mut connection)?;
    restore_triggers(&connection, &folder_triggers)?;
    let root_assets: i64 = connection
        .query_row(
            "SELECT COALESCE(SUM(descendant_asset_count), 0) FROM folder WHERE path = ''",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if root_assets != profile.asset_count as i64 {
        return Err(format!(
            "folder rebuild counted {root_assets} assets, expected {}",
            profile.asset_count
        ));
    }
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); ANALYZE;")
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Populate the same flat thumbnail tree production uses. The bytes are deliberately tiny: this
/// metric is about directory-entry count and first-hit latency, not cache capacity or image decode.
/// The final file is non-empty and is the known hit the production controller reads.
fn populate_derivative_cache(data_dir: &Path, entry_count: usize) -> Result<PathBuf, String> {
    let thumbnails = data_dir.join("cache/thumbnails");
    fs::create_dir_all(&thumbnails).map_err(|e| {
        format!(
            "create derivative cache fixture {}: {e}",
            thumbnails.display()
        )
    })?;
    let hit = thumbnails.join(format!("perf-{:08}.png", entry_count - 1));
    for index in 0..entry_count {
        let path = thumbnails.join(format!("perf-{index:08}.png"));
        let bytes: &[u8] = if path == hit { b"thumbnail-hit" } else { b"" };
        fs::write(&path, bytes)
            .map_err(|e| format!("write derivative cache fixture {}: {e}", path.display()))?;
    }
    Ok(hit)
}

/// Materialize a bounded subset of the catalog's existing local image rows. All paths hard-link a
/// deterministic 512px PNG, so profile size controls target count without multiplying fixture
/// bytes. The analysis still performs the production decode/feature/classify/write path per asset.
fn prepare_analysis_fixture(data_dir: &Path, profile: &Profile) -> Result<Vec<AssetId>, String> {
    const ANALYSIS_EDGE: u32 = 512;
    let root = data_dir.join("analysis-source");
    fs::create_dir_all(&root)
        .map_err(|error| format!("create analysis fixture {}: {error}", root.display()))?;
    let master = root.join("analysis-master.png");
    fs::write(&master, deterministic_png(ANALYSIS_EDGE))
        .map_err(|error| format!("write analysis fixture {}: {error}", master.display()))?;

    // Source zero is local in every deterministic profile. Point only that persisted source at the
    // harness-owned tree; the selected ids below are rows already assigned to it and typed image.
    let connection = serde_json::json!({
        "kind": "local_fs",
        "root": root.to_string_lossy(),
    })
    .to_string();
    let database_path = data_dir.join("library.db");
    let database = Connection::open(&database_path).map_err(|error| {
        format!(
            "open {} for analysis fixture: {error}",
            database_path.display()
        )
    })?;
    let updated = database
        .execute(
            "UPDATE source SET connection=?1 WHERE id=?2",
            params![connection, deterministic_id(1, 0).to_vec()],
        )
        .map_err(|error| format!("point local source at analysis fixture: {error}"))?;
    if updated != 1 {
        return Err(format!(
            "analysis fixture expected one local source update, got {updated}"
        ));
    }

    let mut targets = Vec::with_capacity(profile.upsert_sample);
    for index in 0..profile.asset_count {
        if !index.is_multiple_of(profile.source_count) || !index.is_multiple_of(5) {
            continue;
        }
        let relative = fixture_asset_path(index, profile.max_depth);
        let target = root.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!("create analysis fixture path {}: {error}", parent.display())
            })?;
        }
        fs::hard_link(&master, &target)
            .or_else(|_| fs::copy(&master, &target).map(|_| ()))
            .map_err(|error| {
                format!("materialize analysis fixture {}: {error}", target.display())
            })?;
        targets.push(AssetId::from_bytes(deterministic_id(3, index as u64)));
        if targets.len() == profile.upsert_sample {
            break;
        }
    }
    if targets.len() != profile.upsert_sample {
        return Err(format!(
            "analysis fixture found {} local image targets, expected {}",
            targets.len(),
            profile.upsert_sample
        ));
    }
    Ok(targets)
}

fn fixture_asset_path(index: usize, max_depth: usize) -> String {
    let (_, extension) = media(index);
    let filename = if index.is_multiple_of(10) {
        format!("hero_texture_{index:09}.{extension}")
    } else {
        format!("asset_{index:09}.{extension}")
    };
    let depth = 2 + index % (max_depth - 1);
    let folders = (0..depth)
        .map(|level| format!("level{level:02}_{}", (index / (level + 1)) % 97))
        .collect::<Vec<_>>()
        .join("/");
    format!("{folders}/{filename}")
}

/// Minimal RGB PNG encoder using stored DEFLATE blocks. Avoiding an image dependency keeps xtask's
/// fixture generator small; the production decoder validates these bytes during the smoke run.
fn deterministic_png(edge: u32) -> Vec<u8> {
    let mut scanlines = Vec::with_capacity((edge as usize * 3 + 1) * edge as usize);
    for y in 0..edge {
        scanlines.push(0); // PNG filter: none
        for x in 0..edge {
            scanlines.extend_from_slice(&[
                x.wrapping_mul(13).wrapping_add(y * 3) as u8,
                x.wrapping_mul(5).wrapping_add(y * 11) as u8,
                (x ^ y).wrapping_mul(7) as u8,
            ]);
        }
    }
    let mut deflate = vec![0x78, 0x01]; // zlib header: no compression/fastest
    let chunks = scanlines.chunks(u16::MAX as usize);
    let chunk_count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        deflate.push(u8::from(index + 1 == chunk_count)); // BFINAL + stored BTYPE
        let length = chunk.len() as u16;
        deflate.extend_from_slice(&length.to_le_bytes());
        deflate.extend_from_slice(&(!length).to_le_bytes());
        deflate.extend_from_slice(chunk);
    }
    deflate.extend_from_slice(&adler32(&scanlines).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&edge.to_be_bytes());
    header.extend_from_slice(&edge.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, standard compression/filter
    append_png_chunk(&mut png, b"IHDR", &header);
    append_png_chunk(&mut png, b"IDAT", &deflate);
    append_png_chunk(&mut png, b"IEND", &[]);
    png
}

fn append_png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut checksum_input = Vec::with_capacity(kind.len() + data.len());
    checksum_input.extend_from_slice(kind);
    checksum_input.extend_from_slice(data);
    output.extend_from_slice(&crc32(&checksum_input).to_be_bytes());
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in bytes {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn suspend_triggers(conn: &Connection, prefix: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT name, sql FROM sqlite_schema
              WHERE type='trigger' AND substr(name, 1, length(?1)) = ?1 ORDER BY name",
        )
        .map_err(|error| error.to_string())?;
    let triggers = stmt
        .query_map([prefix], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    drop(stmt);
    for (name, _) in &triggers {
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(format!(
                "refusing to suspend unexpected trigger name {name:?}"
            ));
        }
        conn.execute_batch(&format!("DROP TRIGGER {name};"))
            .map_err(|error| error.to_string())?;
    }
    Ok(triggers.into_iter().map(|(_, sql)| sql).collect())
}

fn restore_triggers(conn: &Connection, triggers: &[String]) -> Result<(), String> {
    for sql in triggers {
        conn.execute_batch(sql).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn rebuild_folders(conn: &mut Connection) -> Result<(), String> {
    let tx = conn.transaction().map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM folder", [])
        .map_err(|error| error.to_string())?;
    tx.execute_batch(
        "INSERT INTO folder(source_id, path, parent_path, name)
             SELECT id, '', '', '' FROM source;
         WITH RECURSIVE hierarchy(source_id, path, parent_path, name, rest) AS (
             SELECT source_id, '', '', '', path FROM asset
             UNION ALL
             SELECT source_id,
                    path || substr(rest, 1, instr(rest, '/')),
                    path,
                    substr(rest, 1, instr(rest, '/') - 1),
                    substr(rest, instr(rest, '/') + 1)
               FROM hierarchy WHERE instr(rest, '/') > 0
         )
         INSERT INTO folder(source_id, path, parent_path, name,
                            direct_asset_count, descendant_asset_count)
             SELECT source_id, path, min(parent_path), min(name),
                    sum(CASE WHEN instr(rest, '/') = 0 THEN 1 ELSE 0 END), count(*)
               FROM hierarchy GROUP BY source_id, path
             ON CONFLICT(source_id, path) DO UPDATE SET
                 parent_path = excluded.parent_path,
                 name = excluded.name,
                 direct_asset_count = excluded.direct_asset_count,
                 descendant_asset_count = excluded.descendant_asset_count;",
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

fn deterministic_id(namespace: u8, index: u64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x3d;
    bytes[1] = namespace;
    bytes[2..8].copy_from_slice(&FIXED_SEED.to_be_bytes()[2..]);
    bytes[8..].copy_from_slice(&index.to_be_bytes());
    bytes
}

fn deterministic_hash(index: u64) -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    for chunk in bytes.as_chunks_mut::<8>().0 {
        chunk.copy_from_slice(&index.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_le_bytes());
    }
    bytes
}

fn deterministic_embedding(index: usize, dimension: usize) -> Vec<u8> {
    let mut values = vec![0.0_f32; dimension];
    values[index % dimension] = 0.8;
    values[(index * 7 + 1) % dimension] += 0.6;
    let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    values
        .into_iter()
        .flat_map(|value| (value / norm).to_le_bytes())
        .collect()
}

fn benchmark_store(
    store: &Store,
    profile: &Profile,
    database_path: &Path,
    data_dir: &Path,
    analysis_targets: Vec<AssetId>,
) -> Result<BTreeMap<String, Metric>, String> {
    let visibility = Visibility::Full;
    let mut metrics = BTreeMap::new();
    let first_request = QueryRequest::default();
    let (first_samples, first_page) = sample_result(profile.samples, || {
        store.query_assets_semantic(&first_request, None, &visibility)
    })?;
    insert_timing(&mut metrics, "first_page_ms", first_samples)?;
    let initial_payload = serde_json::to_vec(&first_page)
        .map_err(|e| e.to_string())?
        .len() as f64;
    insert_metric(
        &mut metrics,
        "browser_initial_payload_bytes",
        initial_payload,
        "bytes",
        Direction::LowerIsBetter,
        vec![initial_payload],
    )?;

    // Derive the near-end boundary through the same opaque keyset cursors a client receives. The
    // old harness fabricated a numeric OFFSET cursor, which both bypassed the production path and
    // became invalid when browse pagination moved to versioned keysets in issue #133. Walking in
    // untimed 500-row pages keeps setup bounded (2,000 cheap index probes for the 1M profile), then
    // uses a short final step to position the measured page exactly 100 rows from the end.
    let late_after = cursor_near_end(store, profile.asset_count, &visibility)?;
    let late_request = QueryRequest {
        include_total: Some(false),
        page: PageParams {
            after: late_after,
            limit: 100,
        },
        ..QueryRequest::default()
    };
    insert_timing(
        &mut metrics,
        "late_page_ms",
        sample(profile.samples, || {
            store
                .query_assets_semantic(&late_request, None, &visibility)
                .map(|_| ())
        })?,
    )?;

    let search_request = QueryRequest {
        text: Some("hero".into()),
        ..QueryRequest::default()
    };
    insert_timing(
        &mut metrics,
        "search_ms",
        sample(profile.samples, || {
            store
                .query_assets_semantic(&search_request, None, &visibility)
                .map(|_| ())
        })?,
    )?;

    let faceted_request = QueryRequest {
        include_facets: true,
        ..QueryRequest::default()
    };
    insert_timing(
        &mut metrics,
        "faceted_query_ms",
        sample(profile.samples, || {
            store
                .query_assets_semantic(&faceted_request, None, &visibility)
                .map(|_| ())
        })?,
    )?;
    insert_timing(
        &mut metrics,
        "stats_ms",
        sample(profile.samples, || {
            store.stats(None, &visibility).map(|_| ())
        })?,
    )?;
    insert_timing(
        &mut metrics,
        "tag_facets_ms",
        sample(profile.samples, || {
            store.list_tags(None, 50, &visibility).map(|_| ())
        })?,
    )?;
    insert_timing(
        &mut metrics,
        "analysis_plan_ms",
        sample(profile.samples, || {
            store.list_analysis_targets(1, false, &[]).map(|_| ())
        })?,
    )?;
    insert_timing(
        &mut metrics,
        "duplicate_detection_ms",
        sample(profile.samples, || {
            store
                .duplicates(&DupRequest::default(), &visibility)
                .map(|_| ())
        })?,
    )?;

    let export_started = Instant::now();
    let export_stats = store
        .stream_export_rows(
            &ExportSelection::Query(QueryRequest::default()),
            &visibility,
            1_000,
            |_| Ok(()),
        )
        .map_err(|e| e.to_string())?;
    let export_rate = export_stats.rows as f64 / export_started.elapsed().as_secs_f64();
    insert_metric(
        &mut metrics,
        "export_assets_per_second",
        export_rate,
        "assets_per_second",
        Direction::HigherIsBetter,
        vec![export_rate],
    )?;

    // One workload, two metrics (issue #137). The upsert loop is the only write workload in the
    // harness; a sampler thread browses the catalog *while* it runs, so the reads contend with a
    // real sustained writer rather than with a synthetic lock. With one writer connection and a
    // pooled read-only connection per checkout the two never serialise, so the browse samples
    // should land within noise of `first_page_ms`. Put reads back behind the writer mutex and a
    // browse has to wait out whichever upsert is in flight — see [`insert_tail_timing`] for why
    // that shows up in the tail rather than the median, and for the measured numbers. The sampler's
    // own cost is charged to `scan_upsert_assets_per_second` too, deliberately: a scan that only
    // goes fast when nobody is looking is not the property we want to record.
    let browse_request = QueryRequest {
        // Deliberately no exact total: that count is a whole-catalog aggregate (already gated by
        // `first_page_ms` and `stats_ms`) and would make this threshold grow with row count instead
        // of with contention. The rest is the default first page a client asks for.
        include_total: Some(false),
        ..QueryRequest::default()
    };
    let writing = std::sync::atomic::AtomicBool::new(true);
    let upsert_started = Instant::now();
    let browse_samples = std::thread::scope(|scope| {
        let sampler =
            scope.spawn(|| browse_while_writing(store, &browse_request, &visibility, &writing));
        let written = run_upsert_workload(store, profile);
        // Release the sampler before propagating a write failure, or the scope blocks forever.
        writing.store(false, std::sync::atomic::Ordering::Release);
        let sampled = sampler
            .join()
            .unwrap_or_else(|_| Err("browse sampler panicked".into()));
        written.and(sampled)
    })?;
    let upsert_rate = profile.upsert_sample as f64 / upsert_started.elapsed().as_secs_f64();
    insert_metric(
        &mut metrics,
        "scan_upsert_assets_per_second",
        upsert_rate,
        "assets_per_second",
        Direction::HigherIsBetter,
        vec![upsert_rate],
    )?;
    insert_tail_timing(&mut metrics, "browse_under_write_ms", browse_samples)?;
    let overhead = aggregate_write_overhead(database_path, profile.upsert_sample.min(5_000))?;
    insert_metric(
        &mut metrics,
        "aggregate_write_overhead_ratio",
        overhead,
        "ratio",
        Direction::LowerIsBetter,
        vec![overhead],
    )?;

    // One production analysis pass, two views of the same interval (issue #184). The service sends
    // image feature extraction to its named, bounded Rayon pool. A paced thread queries through the
    // service for the pass's entire lifetime, so its contention is charged honestly to both the
    // p95 browse latency and the worker utilization. Reading Linux's per-thread scheduler runtime
    // isolates `dam-bg-*` CPU from the query sampler, Tokio, SQLite, and the harness itself.
    let analysis =
        benchmark_analysis_under_browse(data_dir, profile, analysis_targets, &browse_request)?;
    insert_tail_timing(
        &mut metrics,
        "browse_under_analyze_ms",
        analysis.browse_samples,
    )?;
    insert_metric(
        &mut metrics,
        "analysis_rayon_core_utilization_pct",
        analysis.rayon_utilization_pct,
        "percent",
        Direction::HigherIsBetter,
        vec![analysis.rayon_utilization_pct],
    )?;
    let rss = peak_rss_bytes().ok_or("peak RSS is unavailable on this platform")? as f64;
    insert_metric(
        &mut metrics,
        "peak_rss_bytes",
        rss,
        "bytes",
        Direction::LowerIsBetter,
        vec![rss],
    )?;
    Ok(metrics)
}

/// The harness's only write workload: `upsert_sample` assets through the production upsert path,
/// aggregate and folder triggers live. Times `scan_upsert_assets_per_second` at the call site and
/// supplies the sustained writer that `browse_under_write_ms` measures against.
fn run_upsert_workload(store: &Store, profile: &Profile) -> Result<(), String> {
    for index in 0..profile.upsert_sample {
        let source = index % profile.source_count;
        let (media, extension) = media(index);
        let filename = if index % 10 == 0 {
            format!("hero_texture_{index:09}.{extension}")
        } else {
            format!("asset_{index:09}.{extension}")
        };
        let depth = 2 + index % (profile.max_depth - 1);
        let folders = (0..depth)
            .map(|level| format!("level{level:02}_{}", (index / (level + 1)) % 97))
            .collect::<Vec<_>>()
            .join("/");
        store
            .upsert_asset(&NewAsset {
                source_id: SourceId::from_bytes(deterministic_id(1, source as u64)),
                path: format!("{folders}/{filename}"),
                filename,
                content_hash: Some(ContentHash(deterministic_hash(index as u64))),
                size_bytes: Some(1024 + (index % 1_000_000) as i64),
                source_modified_at: Some(1_700_000_000_000 + index as i64),
                scanned_at: 1_700_000_000_000,
                media_type: media,
                format: extension.into(),
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Sample a browse page repeatedly until `writing` clears, returning per-sample milliseconds.
///
/// Paced rather than spun: a browse is a user action arriving at an arbitrary point in the write
/// stream, and a hot loop would both take a core off the writer and stop being representative. The
/// loop is do-while, so however short the workload the metric always has at least one sample.
fn browse_while_writing(
    store: &Store,
    request: &QueryRequest,
    visibility: &Visibility,
    writing: &std::sync::atomic::AtomicBool,
) -> Result<Vec<f64>, String> {
    /// Gap between browses. Short enough that even the one-second smoke workload yields hundreds of
    /// samples, long enough that the sampler stays a reader rather than becoming a load generator
    /// that competes with the writer for a CI runner's two cores.
    const PACE: std::time::Duration = std::time::Duration::from_millis(1);
    /// Reading continues for the whole workload — that is the point — but the report keeps every
    /// raw sample, and the 1M profile writes for tens of seconds. Past this many the distribution
    /// has long settled, so stop growing the JSON rather than stop contending.
    const RECORD_LIMIT: usize = 4_096;
    let mut samples = Vec::new();
    loop {
        let started = Instant::now();
        store
            .query_assets_semantic(request, None, visibility)
            .map_err(|error| error.to_string())?;
        let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
        if samples.len() < RECORD_LIMIT {
            samples.push(elapsed);
        }
        if !writing.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(samples);
        }
        std::thread::sleep(PACE);
    }
}

struct AnalysisMeasurements {
    browse_samples: Vec<f64>,
    rayon_utilization_pct: f64,
}

fn benchmark_analysis_under_browse(
    data_dir: &Path,
    profile: &Profile,
    targets: Vec<AssetId>,
    request: &QueryRequest,
) -> Result<AnalysisMeasurements, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build analysis runtime: {error}"))?;
    let requested_workers = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .saturating_sub(2)
        .clamp(1, 4);
    let library = runtime
        .block_on(EmbeddedLibrary::open_with(
            data_dir,
            ResourceOptions {
                io: dam_core::IoOptions {
                    max_mib_per_sec: Some(1024),
                    concurrency: Some(requested_workers),
                    ..Default::default()
                },
                background_threads: Some(requested_workers),
                min_free_memory_mb: Some(0),
                max_io_stall_pct: Some(f64::INFINITY),
            },
        ))
        .map_err(|error| format!("open fixture for analysis: {error}"))?;
    // The profile-sized derivative inventory uses the same pool. Join it before taking worker CPU
    // snapshots so this metric contains only the analysis pass named in the report.
    runtime
        .block_on(library.wait_for_cache_inventory())
        .map_err(|error| format!("join derivative inventory before analysis: {error}"))?;

    let before = rayon_worker_cpu_nanos()?;
    if before.is_empty() {
        return Err("analysis Rayon pool exposed no dam-bg worker threads".into());
    }
    let analyzing = AtomicBool::new(true);
    let started = Instant::now();
    let browse_samples = std::thread::scope(|scope| {
        let handle = runtime.handle().clone();
        let sampler_library = &library;
        let sampler_analyzing = &analyzing;
        let sampler = scope.spawn(move || {
            browse_while_analyzing(&handle, sampler_library, request, sampler_analyzing)
        });
        let analyzed = runtime.block_on(async {
            let ctx = AuthContext::embedded();
            let job = library
                .submit_analyze(
                    &ctx,
                    AnalyzeRequest {
                        assets: targets,
                        force: false,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            wait_for_analysis(&library, &ctx, job, profile.upsert_sample as u64).await
        });
        // Release the sampler before propagating analysis failure, or the scope blocks forever.
        analyzing.store(false, Ordering::Release);
        let sampled = sampler
            .join()
            .unwrap_or_else(|_| Err("analysis browse sampler panicked".into()));
        analyzed.and(sampled)
    })?;
    let elapsed = started.elapsed();
    let after = rayon_worker_cpu_nanos()?;
    let rayon_utilization_pct = worker_utilization_pct(&before, &after, elapsed)?;
    Ok(AnalysisMeasurements {
        browse_samples,
        rayon_utilization_pct,
    })
}

async fn wait_for_analysis(
    library: &EmbeddedLibrary,
    ctx: &AuthContext,
    job: dam_api::id::JobId,
    expected: u64,
) -> Result<(), String> {
    loop {
        let status = library
            .get_job(ctx, &job)
            .await
            .map_err(|error| error.to_string())?;
        match status.state {
            JobState::Done if status.progress.done == expected => return Ok(()),
            JobState::Done => {
                return Err(format!(
                    "analysis completed {} targets, expected {expected}",
                    status.progress.done
                ));
            }
            JobState::Failed => {
                return Err(status.error.unwrap_or_else(|| "analysis failed".into()));
            }
            JobState::Cancelled => return Err("analysis was cancelled".into()),
            JobState::Queued | JobState::Running | JobState::Paused => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }
}

fn browse_while_analyzing(
    runtime: &tokio::runtime::Handle,
    library: &EmbeddedLibrary,
    request: &QueryRequest,
    analyzing: &AtomicBool,
) -> Result<Vec<f64>, String> {
    const PACE: Duration = Duration::from_millis(1);
    const RECORD_LIMIT: usize = 4_096;
    let ctx = AuthContext::embedded();
    let mut samples = Vec::new();
    loop {
        let started = Instant::now();
        runtime
            .block_on(library.query(&ctx, request.clone()))
            .map_err(|error| error.to_string())?;
        let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
        if samples.len() < RECORD_LIMIT {
            samples.push(elapsed);
        }
        if !analyzing.load(Ordering::Acquire) {
            return Ok(samples);
        }
        std::thread::sleep(PACE);
    }
}

/// Linux scheduler runtime for each production Rayon worker, keyed by stable kernel thread id.
/// `/proc/.../schedstat`'s first field is nanoseconds actually executing, excluding runnable wait.
fn rayon_worker_cpu_nanos() -> Result<BTreeMap<String, u64>, String> {
    let tasks = fs::read_dir("/proc/self/task")
        .map_err(|error| format!("list process threads for analysis utilization: {error}"))?;
    let mut workers = BTreeMap::new();
    for task in tasks.flatten() {
        let tid = task.file_name().to_string_lossy().into_owned();
        let comm = fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        if !comm.trim().starts_with("dam-bg-") {
            continue;
        }
        let schedstat = fs::read_to_string(task.path().join("schedstat"))
            .map_err(|error| format!("read scheduler runtime for dam-bg thread {tid}: {error}"))?;
        let runtime = schedstat
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| format!("invalid scheduler runtime for dam-bg thread {tid}"))?;
        workers.insert(tid, runtime);
    }
    Ok(workers)
}

fn worker_utilization_pct(
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
    elapsed: Duration,
) -> Result<f64, String> {
    if before.is_empty() || elapsed.is_zero() {
        return Err("analysis utilization needs workers and non-zero elapsed time".into());
    }
    let busy_nanos = before.iter().try_fold(0_u128, |total, (tid, start)| {
        let end = after
            .get(tid)
            .ok_or_else(|| format!("analysis Rayon worker {tid} disappeared during measurement"))?;
        Ok::<_, String>(total + u128::from(end.saturating_sub(*start)))
    })?;
    let capacity_nanos = elapsed.as_nanos() * before.len() as u128;
    Ok(busy_nanos as f64 / capacity_nanos as f64 * 100.0)
}

/// Measure the incremental V26 trigger cost against the same indexed no-op asset update without an
/// aggregate-relevant column. Both loops run inside rolled-back transactions against production
/// schema and identical deterministic rows; the ratio therefore records aggregate write overhead
/// without changing the fixture used by later diagnostics.
fn aggregate_write_overhead(database_path: &Path, samples: usize) -> Result<f64, String> {
    let mut conn = Connection::open(database_path).map_err(|error| error.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(15))
        .map_err(|error| error.to_string())?;
    let samples = samples.max(100);
    let timed = |conn: &mut Connection, sql: &str| -> Result<f64, String> {
        let tx = conn.transaction().map_err(|error| error.to_string())?;
        let started = Instant::now();
        {
            let mut update = tx.prepare_cached(sql).map_err(|error| error.to_string())?;
            for index in 0..samples {
                update
                    .execute([deterministic_id(3, index as u64).to_vec()])
                    .map_err(|error| error.to_string())?;
            }
        }
        let seconds = started.elapsed().as_secs_f64();
        tx.rollback().map_err(|error| error.to_string())?;
        Ok(seconds)
    };
    const BASELINE: &str = "UPDATE asset SET scanned_at=scanned_at WHERE id=?1";
    const MAINTAINED: &str = "UPDATE asset SET analysed_at=analysed_at WHERE id=?1";
    // Warm both statement/page paths before collecting paired rounds; otherwise whichever loop runs
    // first pays SQLite's cold-cache cost and can make the incremental trigger ratio misleading.
    let _ = timed(&mut conn, BASELINE)?;
    let _ = timed(&mut conn, MAINTAINED)?;
    let mut baseline_samples = Vec::with_capacity(5);
    let mut maintained_samples = Vec::with_capacity(5);
    for round in 0..5 {
        if round % 2 == 0 {
            maintained_samples.push(timed(&mut conn, MAINTAINED)?);
            baseline_samples.push(timed(&mut conn, BASELINE)?);
        } else {
            baseline_samples.push(timed(&mut conn, BASELINE)?);
            maintained_samples.push(timed(&mut conn, MAINTAINED)?);
        }
    }
    baseline_samples.sort_by(f64::total_cmp);
    maintained_samples.sort_by(f64::total_cmp);
    let baseline = baseline_samples[baseline_samples.len() / 2];
    let maintained = maintained_samples[maintained_samples.len() / 2];
    if baseline <= f64::EPSILON {
        return Err("aggregate write-overhead baseline timer had zero duration".into());
    }
    Ok(maintained / baseline)
}

fn cursor_near_end(
    store: &Store,
    asset_count: usize,
    visibility: &Visibility,
) -> Result<Option<Cursor>, String> {
    const WALK_LIMIT: usize = 500;
    let target = asset_count.saturating_sub(100);
    let mut consumed = 0_usize;
    let mut after = None;
    while consumed < target {
        let limit = (target - consumed).min(WALK_LIMIT) as u32;
        let request = QueryRequest {
            include_total: Some(false),
            page: PageParams {
                after: after.clone(),
                limit,
            },
            ..QueryRequest::default()
        };
        let page = store
            .query_assets_semantic(&request, None, visibility)
            .map_err(|error| error.to_string())?;
        if page.items.is_empty() {
            return Err(format!(
                "catalog ended after {consumed} rows while positioning late-page benchmark at {target}"
            ));
        }
        consumed += page.items.len();
        after = page.cursor;
        if consumed < target && after.is_none() {
            return Err(format!(
                "catalog ended after {consumed} rows while positioning late-page benchmark at {target}"
            ));
        }
    }
    Ok(after)
}

fn media(index: usize) -> (dam_api::dto::MediaType, &'static str) {
    use dam_api::dto::MediaType::*;
    match index % 5 {
        0 => (Image, "png"),
        1 => (Audio, "wav"),
        2 => (Model, "glb"),
        3 => (Video, "mp4"),
        _ => (Document, "pdf"),
    }
}

fn sample<T>(
    count: usize,
    mut operation: impl FnMut() -> Result<T, dam_api::LibError>,
) -> Result<Vec<f64>, String> {
    let (samples, _) = sample_result(count, &mut operation)?;
    Ok(samples)
}

fn sample_result<T>(
    count: usize,
    mut operation: impl FnMut() -> Result<T, dam_api::LibError>,
) -> Result<(Vec<f64>, T), String> {
    let mut samples = Vec::with_capacity(count);
    let mut last = None;
    for _ in 0..count {
        let started = Instant::now();
        last = Some(operation().map_err(|e| e.to_string())?);
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    Ok((samples, last.ok_or("sample count must not be zero")?))
}

fn insert_timing(
    metrics: &mut BTreeMap<String, Metric>,
    name: &str,
    mut samples: Vec<f64>,
) -> Result<(), String> {
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    insert_metric(
        metrics,
        name,
        median,
        "ms",
        Direction::LowerIsBetter,
        samples,
    )
}

/// Like [`insert_timing`], but the *gated* value is the p95, not the median.
///
/// The median is the right summary for an uncontended query and the wrong one for a contention
/// probe. When reads serialise behind the writer the distribution goes bimodal rather than shifting:
/// most browses still slip into the gap between two writes and stay fast, while the unlucky ones
/// wait out a whole write — or, because `std::sync::Mutex` is not fair, several in a row. Measured
/// on the smoke profile with `Db::read` forced back onto the writer mutex, the median moved
/// 0.6 ms → 1.5 ms (which no honest threshold can separate from machine noise) while the p95 moved
/// 1.2 ms → 289 ms. The tail is both what a user actually feels and the only statistic that catches
/// the regression, so it is what the baseline compares. `Metric::p95` reports the same number.
fn insert_tail_timing(
    metrics: &mut BTreeMap<String, Metric>,
    name: &str,
    samples: Vec<f64>,
) -> Result<(), String> {
    let tail = p95(&samples);
    insert_metric(metrics, name, tail, "ms", Direction::LowerIsBetter, samples)
}

fn p95(samples: &[f64]) -> f64 {
    let mut ordered = samples.to_vec();
    ordered.sort_by(f64::total_cmp);
    let index = (ordered.len() * 95).div_ceil(100).saturating_sub(1);
    ordered[index]
}

fn insert_metric(
    metrics: &mut BTreeMap<String, Metric>,
    name: &str,
    value: f64,
    unit: &'static str,
    direction: Direction,
    samples: Vec<f64>,
) -> Result<(), String> {
    if !value.is_finite() || samples.iter().any(|sample| !sample.is_finite()) {
        return Err(format!("metric {name} is missing, NaN, or infinite"));
    }
    metrics.insert(
        name.into(),
        Metric {
            value,
            p95: p95(&samples),
            unit,
            direction,
            samples,
        },
    );
    Ok(())
}

fn compare(
    metrics: &BTreeMap<String, Metric>,
    baseline: &BaselineProfile,
    browser_skipped: bool,
    browser_failed: bool,
) -> Result<(BTreeMap<String, Comparison>, bool), String> {
    let mut comparisons = BTreeMap::new();
    let mut passed = true;
    for (name, reference) in &baseline.metrics {
        if browser_skipped && BROWSER_METRICS.contains(&name.as_str()) {
            comparisons.insert(
                name.clone(),
                Comparison {
                    reference: reference.reference,
                    ratio: None,
                    max_ratio: reference.max_ratio,
                    direction: reference.direction,
                    status: "skipped",
                },
            );
            continue;
        }
        if browser_failed && BROWSER_METRICS.contains(&name.as_str()) {
            passed = false;
            comparisons.insert(
                name.clone(),
                Comparison {
                    reference: reference.reference,
                    ratio: None,
                    max_ratio: reference.max_ratio,
                    direction: reference.direction,
                    status: "failed",
                },
            );
            continue;
        }
        let measured = metrics
            .get(name)
            .ok_or_else(|| format!("required metric '{name}' is missing"))?;
        if measured.direction != reference.direction {
            return Err(format!(
                "baseline direction for '{name}' disagrees with the harness"
            ));
        }
        let ratio = match reference.direction {
            Direction::LowerIsBetter => measured.value / reference.reference,
            Direction::HigherIsBetter => reference.reference / measured.value,
        };
        if !ratio.is_finite() {
            return Err(format!("comparison ratio for '{name}' is not finite"));
        }
        let metric_passed = ratio <= reference.max_ratio;
        passed &= metric_passed;
        comparisons.insert(
            name.clone(),
            Comparison {
                reference: reference.reference,
                ratio: Some(ratio),
                max_ratio: reference.max_ratio,
                direction: reference.direction,
                status: if metric_passed { "passed" } else { "failed" },
            },
        );
    }
    let unknown_measured: Vec<_> = metrics
        .keys()
        .filter(|name| !baseline.metrics.contains_key(*name))
        .collect();
    if !unknown_measured.is_empty() {
        return Err(format!(
            "metrics have no baseline entries: {unknown_measured:?}"
        ));
    }
    Ok((comparisons, passed))
}

fn benchmark_browser(asset_count: usize) -> Result<BrowserResult, String> {
    let output = Command::new("node")
        .args([
            "--expose-gc",
            "--experimental-strip-types",
            "web/scripts/profile-scale.mts",
            &asset_count.to_string(),
        ])
        .output()
        .map_err(|e| format!("could not launch Node: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "Node profile exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("invalid browser profile JSON: {e}"))
}

fn peak_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let kb = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(kb * 1024)
}

fn machine_metadata() -> Machine {
    Machine {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        logical_cpus: std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1),
        cpu_model: fs::read_to_string("/proc/cpuinfo").ok().and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("model name\t: ").map(str::to_owned))
        }),
        total_memory_bytes: fs::read_to_string("/proc/meminfo").ok().and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("MemTotal:"))
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|kb| kb * 1024)
        }),
        rustc: command_line("rustc", &["--version"]),
        git_sha: command_line("git", &["rev-parse", "HEAD"]),
    }
}

fn command_line(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_recipe_is_stable() {
        assert_eq!(deterministic_id(3, 42), deterministic_id(3, 42));
        assert_ne!(deterministic_id(3, 42), deterministic_id(3, 43));
        assert_eq!(deterministic_hash(8), deterministic_hash(8));
        let png = deterministic_png(8);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(png.windows(4).any(|chunk| chunk == b"IEND"));
    }

    #[test]
    fn analysis_tail_gate_exposes_bimodal_counterfactual() {
        let mut samples = vec![1.0; 94];
        samples.extend([100.0; 6]);
        let mut metrics = BTreeMap::new();
        insert_tail_timing(&mut metrics, "browse_under_analyze_ms", samples).unwrap();
        let metric = &metrics["browse_under_analyze_ms"];
        assert_eq!(metric.value, 100.0, "the gated value must be p95");
        assert_eq!(metric.p95, 100.0);
        assert_eq!(metric.samples[metric.samples.len() / 2], 1.0);
    }

    #[test]
    fn rayon_utilization_exposes_serial_or_async_worker_counterfactual() {
        let before = BTreeMap::from([
            ("1".into(), 0),
            ("2".into(), 0),
            ("3".into(), 0),
            ("4".into(), 0),
        ]);
        let parallel = BTreeMap::from([
            ("1".into(), 900_000_000),
            ("2".into(), 900_000_000),
            ("3".into(), 900_000_000),
            ("4".into(), 900_000_000),
        ]);
        let serial = BTreeMap::from([
            ("1".into(), 900_000_000),
            ("2".into(), 0),
            ("3".into(), 0),
            ("4".into(), 0),
        ]);
        let misplaced_async = before.clone();
        let elapsed = Duration::from_secs(1);
        assert_eq!(
            worker_utilization_pct(&before, &parallel, elapsed).unwrap(),
            90.0
        );
        assert_eq!(
            worker_utilization_pct(&before, &serial, elapsed).unwrap(),
            22.5
        );
        assert_eq!(
            worker_utilization_pct(&before, &misplaced_async, elapsed).unwrap(),
            0.0
        );
    }

    #[test]
    fn checked_in_configuration_parses_and_validates() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let profiles: ProfilesFile = load_json(&root.join("perf/profiles.json")).unwrap();
        assert_eq!(profiles.schema_version, 1);
        for profile in profiles.profiles.values() {
            validate_profile(profile).unwrap();
        }
        let baselines: BaselinesFile = load_json(&root.join("perf/baselines.json")).unwrap();
        for baseline in baselines.profiles.values() {
            validate_baseline(baseline).unwrap();
        }
    }

    #[test]
    fn rejects_destructive_or_nested_report_paths() {
        let current = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let options = |work_dir: PathBuf, output: PathBuf| Options {
            profile: "smoke".into(),
            profiles_path: "perf/profiles.json".into(),
            baseline_path: "perf/baselines.json".into(),
            output,
            work_dir,
            skip_browser: false,
            keep_catalog: false,
        };
        assert!(validate_paths(options(current.clone(), current.join("report.json"))).is_err());
        assert!(validate_paths(options(
            current.parent().unwrap().to_path_buf(),
            current.join("report.json")
        ))
        .is_err());
        let safe_work = current.join("target/perf-safety-test");
        assert!(validate_paths(options(safe_work.clone(), safe_work.join("report.json"))).is_err());
        assert!(validate_paths(options(
            safe_work,
            current.join("target/perf-safety-report.json")
        ))
        .is_ok());
    }

    #[test]
    fn removes_only_harness_owned_work_directories() {
        let path = std::env::current_dir()
            .unwrap()
            .join(format!("target/perf-ownership-test-{}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        assert!(remove_owned_work_dir(&path).is_err());
        fs::write(path.join(WORK_DIR_MARKER), RECIPE_VERSION).unwrap();
        remove_owned_work_dir(&path).unwrap();
        assert!(!path.exists());
    }
}
