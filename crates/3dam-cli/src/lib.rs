//! `dam-cli` — the CLI front-end (tech-spec 13). Rides the same `LibraryService` seam as every
//! other front-end via `open_backend`, so `--connect` transparently swaps the embedded engine for
//! a remote server. Also owns the `serve`/`mcp` entry points (they dispatch into `3dam-server`).

use clap::{Args, Parser, Subcommand};
use dam_api::dto::*;
use dam_api::id::{AssetId, JobId, SourceId};
use dam_api::service::{AuthContext, LibraryService};
use dam_frontend::{default_data_dir, open_backend, Backend};
use dam_server::ServeConfig;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use url::Url;

#[derive(Parser)]
#[command(name = "3dam", about = "3DAM — game-asset manager (CLI)", version)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct Global {
    /// Connect to a remote `3dam serve` instead of the local library.
    #[arg(long, global = true, value_name = "URL")]
    connect: Option<String>,
    /// Override the library data directory (embedded mode).
    #[arg(long, global = true, value_name = "DIR")]
    data: Option<PathBuf>,
    /// Emit machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan configured sources for assets.
    Scan {
        /// Limit to one source id (default: all local sources).
        #[arg(long)]
        source: Option<String>,
        /// Wait for the scan to finish and print a summary.
        #[arg(long)]
        wait: bool,
    },
    /// Search the library.
    Search {
        /// Free text over filename.
        text: Option<String>,
        #[arg(long)]
        media: Option<String>,
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Manage sources.
    Sources {
        #[command(subcommand)]
        cmd: SourceCmd,
    },
    /// Show library statistics.
    Stats,
    /// Show one asset's full record.
    Get {
        /// Asset id (UUID).
        id: String,
    },
    /// List background jobs.
    Jobs,
    /// Show one job's status.
    Job { id: String },
    /// Convert/optimise assets non-destructively into an output directory (tech-spec 08).
    Convert {
        /// Asset ids to convert.
        #[arg(required = true)]
        ids: Vec<String>,
        /// Target format: `png`|`jpg`|`webp`|`bmp`|`tga`|`tiff`|`gif` (image) or `wav` (audio).
        #[arg(long)]
        to: String,
        /// Output directory (never a source tree — convert is non-destructive).
        #[arg(long)]
        out: PathBuf,
        /// Plan only: resolve outputs + report, write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Images: fit within this box on the long edge (aspect preserved).
        #[arg(long)]
        max_edge: Option<u32>,
        /// JPEG quality 1..=100 (lossy image targets only).
        #[arg(long)]
        quality: Option<u8>,
        /// Collision handling: `fail` (default) | `suffix` | `skip` | `overwrite`.
        #[arg(long, default_value = "fail")]
        on_collision: String,
    },
}

#[derive(Subcommand)]
enum SourceCmd {
    /// Add a local filesystem source.
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        watch: bool,
    },
    /// List sources.
    List,
    /// Remove a source.
    Remove {
        id: String,
        /// Keep cached asset rows (mark offline instead of deleting).
        #[arg(long)]
        keep_metadata: bool,
    },
}

/// Entry point for the CLI role (run-and-exit verbs).
pub async fn run(args: Vec<OsString>) -> ExitCode {
    init_tracing();
    let cli = match Cli::try_parse_from(std::iter::once(OsString::from("3dam")).chain(args)) {
        Ok(c) => c,
        Err(e) => {
            // clap prints help/usage itself; propagate its exit code sense.
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match dispatch(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let ctx = AuthContext::embedded();
    let lib = open(&cli.global).await?;
    let json = cli.global.json;

    match cli.cmd {
        Cmd::Scan { source, wait } => {
            let sources = match source {
                Some(s) => vec![parse_source_id(&s)?],
                None => Vec::new(),
            };
            let job_id = lib
                .submit_scan(
                    &ctx,
                    ScanRequest {
                        sources,
                        mode: ScanMode::Full,
                    },
                )
                .await?;
            if json {
                println!("{}", serde_json::json!({ "job_id": job_id }));
            } else {
                println!("scan started: job {job_id}");
            }
            if wait {
                wait_for_job(lib.as_ref(), &ctx, &job_id, json).await?;
            }
        }

        Cmd::Search {
            text,
            media,
            format,
            limit,
        } => {
            let mut filters = Vec::new();
            if let Some(m) = media {
                filters.push(Filter {
                    field: FacetField::MediaType,
                    op: FilterOp::Eq,
                    value: FilterValue::Str(m),
                });
            }
            if let Some(f) = format {
                filters.push(Filter {
                    field: FacetField::Format,
                    op: FilterOp::Eq,
                    value: FilterValue::Str(f),
                });
            }
            let req = QueryRequest {
                text,
                filters,
                page: dam_api::page::PageParams { after: None, limit },
                ..Default::default()
            };
            let page = lib.query(&ctx, req).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                print_search(&page);
            }
        }

        Cmd::Sources { cmd } => match cmd {
            SourceCmd::Add { path, name, watch } => {
                let id = lib
                    .add_source(
                        &ctx,
                        AddSource {
                            kind: SourceKind::LocalFs,
                            uri: path.to_string_lossy().into_owned(),
                            name,
                            options: SourceOptions {
                                watch,
                                ..Default::default()
                            },
                        },
                    )
                    .await?;
                if json {
                    println!("{}", serde_json::json!({ "id": id }));
                } else {
                    println!("added source {id}");
                }
            }
            SourceCmd::List => {
                let sources = lib.list_sources(&ctx).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&sources)?);
                } else {
                    print_sources(&sources);
                }
            }
            SourceCmd::Remove { id, keep_metadata } => {
                let sid = parse_source_id(&id)?;
                lib.remove_source(&ctx, &sid, RemoveSource { keep_metadata })
                    .await?;
                println!("removed source {sid}");
            }
        },

        Cmd::Stats => {
            let stats = lib.library_stats(&ctx).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                print_stats(&stats);
            }
        }

        Cmd::Get { id } => {
            let aid: AssetId = id
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid asset id"))?;
            let asset = lib.get_asset(&ctx, &aid).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&asset)?);
            } else {
                print_asset(&asset);
            }
        }

        Cmd::Jobs => {
            let page = lib.list_jobs(&ctx, JobListRequest::default()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                for j in &page.items {
                    println!(
                        "{}  {:?}  {:?}  {}/{}",
                        j.id,
                        j.kind,
                        j.state,
                        j.progress.done,
                        j.progress
                            .total
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "?".into())
                    );
                }
            }
        }

        Cmd::Job { id } => {
            let jid: JobId = id.parse().map_err(|_| anyhow::anyhow!("invalid job id"))?;
            let job = lib.get_job(&ctx, &jid).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&job)?);
            } else {
                println!("{job:#?}");
            }
        }

        Cmd::Convert {
            ids,
            to,
            out,
            dry_run,
            max_edge,
            quality,
            on_collision,
        } => {
            let inputs: Vec<AssetId> = ids
                .iter()
                .map(|s| {
                    s.parse::<AssetId>()
                        .map_err(|_| anyhow::anyhow!("invalid asset id: {s}"))
                })
                .collect::<anyhow::Result<_>>()?;
            let target = build_convert_target(&to)?;
            let target = apply_image_opts(target, max_edge, quality);
            let req = ConvertRequest {
                inputs,
                target,
                output_dir: out.to_string_lossy().into_owned(),
                dry_run,
                on_collision: parse_collision(&on_collision)?,
            };
            let report = lib.convert(&ctx, req).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_convert(&report);
            }
        }
    }
    Ok(())
}

/// Pick the media-typed target from the requested `--to` format (audio = `wav`, else image).
fn build_convert_target(to: &str) -> anyhow::Result<ConvertTarget> {
    let to = to.to_ascii_lowercase();
    match to.as_str() {
        "wav" => Ok(ConvertTarget::Audio { format: to }),
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tga" | "tiff" | "tif" | "gif" => {
            Ok(ConvertTarget::Image {
                format: to,
                max_edge: None,
                quality: None,
            })
        }
        other => Err(anyhow::anyhow!(
            "unsupported target format '{other}' (image: png|jpg|webp|bmp|tga|tiff|gif; audio: wav)"
        )),
    }
}

fn apply_image_opts(
    target: ConvertTarget,
    max_edge: Option<u32>,
    quality: Option<u8>,
) -> ConvertTarget {
    match target {
        ConvertTarget::Image { format, .. } => ConvertTarget::Image {
            format,
            max_edge,
            quality,
        },
        other => other,
    }
}

fn parse_collision(s: &str) -> anyhow::Result<CollisionRule> {
    match s.to_ascii_lowercase().as_str() {
        "fail" => Ok(CollisionRule::Fail),
        "suffix" => Ok(CollisionRule::Suffix),
        "skip" => Ok(CollisionRule::Skip),
        "overwrite" => Ok(CollisionRule::Overwrite),
        other => Err(anyhow::anyhow!(
            "invalid --on-collision '{other}' (fail|suffix|skip|overwrite)"
        )),
    }
}

fn print_convert(r: &ConvertReport) {
    for item in &r.items {
        let name = std::path::Path::new(&item.planned_output)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| item.input.to_string());
        let detail = match &item.error {
            Some(e) => format!("  ({e})"),
            None => match (item.output_bytes, item.ratio) {
                (Some(b), Some(ratio)) => format!("  {} ({:.0}%)", human_size(b), ratio * 100.0),
                _ => String::new(),
            },
        };
        println!(
            "{:<11} {}{}",
            format!("{:?}", item.disposition).to_lowercase(),
            name,
            detail
        );
    }
    let verb = if r.dry_run { "planned" } else { "done" };
    println!(
        "{verb}: {} ok, {} failed, {} collision, {} unsupported → {}",
        r.done, r.failed, r.collisions, r.unsupported, r.output_dir
    );
    if !r.dry_run && r.total_output_bytes > 0 {
        println!(
            "bytes: {} → {}",
            human_size(r.total_input_bytes),
            human_size(r.total_output_bytes)
        );
    }
}

async fn wait_for_job(
    lib: &dyn LibraryService,
    ctx: &AuthContext,
    job_id: &JobId,
    json: bool,
) -> anyhow::Result<()> {
    loop {
        let job = lib.get_job(ctx, job_id).await?;
        let terminal = matches!(
            job.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        );
        if terminal {
            if json {
                println!("{}", serde_json::to_string_pretty(&job)?);
            } else {
                let total = job
                    .progress
                    .total
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| job.progress.done.to_string());
                println!(
                    "scan {:?}: {} assets{}",
                    job.state,
                    total,
                    job.error.map(|e| format!(" ({e})")).unwrap_or_default()
                );
            }
            return Ok(());
        }
        if !json {
            eprint!("\rscanning… {} found", job.progress.done);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ── serve / mcp roles ────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "serve")]
struct ServeArgs {
    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1:7878")]
    addr: String,
    /// Library data directory.
    #[arg(long)]
    data: Option<PathBuf>,
}

/// Entry point for the `serve` role. Dispatches into `3dam-server`.
pub async fn serve(args: Vec<OsString>) -> ExitCode {
    init_tracing();
    let parsed =
        match ServeArgs::try_parse_from(std::iter::once(OsString::from("serve")).chain(args)) {
            Ok(a) => a,
            Err(e) => {
                let _ = e.print();
                return ExitCode::from(2);
            }
        };
    let addr: SocketAddr = match parsed.addr.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("error: invalid --addr '{}'", parsed.addr);
            return ExitCode::from(2);
        }
    };
    let cfg = ServeConfig {
        addr,
        data_dir: parsed.data.unwrap_or_else(default_data_dir),
    };
    match dam_server::serve(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("serve error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Parser)]
#[command(name = "mcp")]
struct McpArgs {
    #[arg(long)]
    data: Option<PathBuf>,
}

/// Entry point for the `mcp` stdio role. Dispatches into `3dam-server` (stub).
pub async fn mcp(args: Vec<OsString>) -> ExitCode {
    let parsed = match McpArgs::try_parse_from(std::iter::once(OsString::from("mcp")).chain(args)) {
        Ok(a) => a,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(2);
        }
    };
    match dam_server::mcp_stdio(parsed.data.unwrap_or_else(default_data_dir)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mcp error: {e}");
            ExitCode::FAILURE
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

async fn open(global: &Global) -> anyhow::Result<Box<dyn LibraryService>> {
    let backend = match &global.connect {
        Some(c) => Backend::Connected {
            endpoint: parse_endpoint(c)?,
        },
        None => Backend::Embedded {
            data_dir: global.data.clone().unwrap_or_else(default_data_dir),
        },
    };
    Ok(open_backend(backend).await?)
}

fn parse_endpoint(s: &str) -> anyhow::Result<Url> {
    let with_scheme = if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    };
    Ok(Url::parse(&with_scheme)?)
}

fn parse_source_id(s: &str) -> anyhow::Result<SourceId> {
    s.parse()
        .map_err(|_| anyhow::anyhow!("invalid source id: {s}"))
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

fn print_search(page: &dam_api::page::Page<AssetSummary>) {
    if page.items.is_empty() {
        println!("(no matches)");
        return;
    }
    for a in &page.items {
        println!(
            "{}  {:<6} {:<5} {:>10}  {}",
            a.id,
            a.media.as_str(),
            a.format,
            human_size(a.size),
            a.name
        );
    }
    let shown = page.items.len();
    match page.total {
        Some(t) if t as usize > shown => println!("… {shown} of {t}"),
        _ => println!("{shown} result(s)"),
    }
}

fn print_sources(sources: &[SourceInfo]) {
    if sources.is_empty() {
        println!("(no sources — add one with `3dam sources add <path>`)");
        return;
    }
    for s in sources {
        println!(
            "{}  {:<9} {:<5} {}  ({} assets)  {}",
            s.id,
            s.kind.as_str(),
            format!("{:?}", s.state).to_lowercase(),
            s.name,
            s.stats.asset_count,
            s.uri
        );
    }
}

fn print_stats(s: &LibraryStats) {
    println!("assets:     {}", s.total);
    println!("sources:    {}", s.sources);
    println!("unanalyzed: {}", s.unanalyzed);
    if !s.by_media.is_empty() {
        println!("by media:");
        for (k, v) in &s.by_media {
            println!("  {k:<6} {v}");
        }
    }
    if !s.by_source.is_empty() {
        println!("by source:");
        for (k, v) in &s.by_source {
            println!("  {k:<20} {v}");
        }
    }
}

fn print_asset(a: &Asset) {
    println!("{}", a.summary.name);
    println!("  id:       {}", a.summary.id);
    println!(
        "  media:    {}/{}",
        a.summary.media.as_str(),
        a.summary.format
    );
    println!("  size:     {}", human_size(a.summary.size));
    println!("  path:     {}", a.path);
    if let Some(h) = &a.hash {
        println!("  hash:     {h}");
    }
    println!(
        "  license:  {} ({})",
        a.license.id.clone().unwrap_or_else(|| "—".into()),
        a.license.status.as_str()
    );
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
