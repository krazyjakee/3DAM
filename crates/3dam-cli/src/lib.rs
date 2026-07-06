//! `dam-cli` — the CLI front-end (tech-spec 13). Rides the same `LibraryService` seam as every
//! other front-end via `open_backend`, so `--connect` transparently swaps the embedded engine for
//! a remote server. Also owns the `serve`/`mcp` entry points (they dispatch into `3dam-server`).

use clap::{Args, Parser, Subcommand};
use dam_api::admin::{
    AdminStatus, AuditEntry, AuthMode, FlagInfo, FlagKey, FlagValue, McpMode, NewToken,
    NewTokenReply, SetFlag, TokenInfo,
};
use dam_api::dto::*;
use dam_api::id::{AssetId, CollectionId, JobId, SourceId};
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService, Scope, Scopes};
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
    /// Bearer token presented to a `--connect` server (required when it runs in token-auth mode).
    #[arg(long, global = true, value_name = "TOKEN")]
    token: Option<String>,
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
        /// Limit to one source id (default: all file sources).
        #[arg(long)]
        source: Option<String>,
        /// Delta re-scan: only re-open files whose size/mtime changed; mark vanished files absent.
        #[arg(long)]
        delta: bool,
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
    /// Manage collections and smart folders.
    Collections {
        #[command(subcommand)]
        cmd: CollectionCmd,
    },
    /// Export a metadata manifest (json/csv/sidecar) for use in engines and pipelines.
    Export {
        /// Explicit asset ids (default: whole library, or use --collection / --text / --media).
        ids: Vec<String>,
        /// Manifest format: `json` (default) | `csv` | `sidecar`.
        #[arg(long, default_value = "json")]
        format: String,
        /// Output file (json/csv) or directory (sidecar).
        #[arg(long)]
        out: PathBuf,
        /// Export a collection / smart folder by id.
        #[arg(long)]
        collection: Option<String>,
        /// Free-text query selector.
        #[arg(long)]
        text: Option<String>,
        /// Media-type query selector: `audio`|`image`|`model`.
        #[arg(long)]
        media: Option<String>,
        /// Only the assets that need crediting, with attribution columns (the credits list).
        #[arg(long)]
        attribution_only: bool,
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
    /// Analyse assets: embeddings, tileability/perceptual signals, auto-tag/-category, dedup (tech-spec 05).
    Analyze {
        /// Specific asset ids (default: every asset behind the current analysis version).
        ids: Vec<String>,
        /// Re-analyse even up-to-date assets (e.g. after tuning).
        #[arg(long)]
        force: bool,
        /// Wait for the job to finish and print a summary.
        #[arg(long)]
        wait: bool,
    },
    /// Find assets similar to one ("more like this") by embedding cosine.
    Similar {
        /// The query asset id.
        id: String,
        /// How many neighbours to return.
        #[arg(long, default_value_t = 12)]
        limit: u32,
    },
    /// List duplicate groups for review (never deletes anything).
    Dedup {
        /// Near-duplicates (perceptual/embedding) instead of exact (byte-identical).
        #[arg(long)]
        near: bool,
        /// Limit to one media type: `audio`|`image`|`model`.
        #[arg(long)]
        media: Option<String>,
        /// Max groups to return.
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Accept or reject an auto-suggested tag on an asset (the review lifecycle, §1.4).
    Tag {
        /// Asset id.
        id: String,
        /// Tag name (e.g. `seamless`, `rigged`).
        tag: String,
        /// Reject instead of accept (records a negative so re-analysis won't re-suggest it).
        #[arg(long)]
        reject: bool,
    },
    /// Administer the server: feature flags, API tokens, status, audit (tech-spec 10 §5).
    ///
    /// Drives the same admin surface as the web Settings area. Over `--connect` it calls the
    /// `/admin/api` routes; embedded it operates on the local server store directly (ADR 0009 §2 —
    /// seeds the store on first use; runtime-only ops need a running server).
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
}

#[derive(Subcommand)]
enum AdminCmd {
    /// Show the server posture: bind, auth mode, MCP, network-writes, exposure warnings.
    Status,
    /// List feature flags with their values and versions.
    Flags,
    /// Get a flag, or set it with `--set <value>` (auth: off|anonymous|token; mcp_server:
    /// off|read_only|read_write; network_writes: true|false).
    Flag {
        /// Flag key: `authentication` | `mcp_server` | `network_writes`.
        key: String,
        /// New value; omit to just read the flag.
        #[arg(long)]
        set: Option<String>,
        /// Confirm an exposure-increasing change (removing auth, enabling writes).
        #[arg(long)]
        confirm: bool,
    },
    /// Manage API tokens.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Show the audit log (most recent first).
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Issue a scoped API key. The secret is printed once and never retrievable again.
    Add {
        /// Human label shown in listings.
        label: String,
        /// Granted scopes (repeatable or comma-separated): read, write, admin, mcp_use, federate.
        /// Default: read + mcp_use.
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,
        /// Optional absolute expiry, epoch milliseconds (default: no expiry).
        #[arg(long)]
        expires: Option<i64>,
    },
    /// List tokens (labels, scopes, last-used — never secrets).
    List,
    /// Revoke a token by id.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum SourceCmd {
    /// Add a file source: a local path, or `sftp://user@host/path` / `smb://host/share/path`.
    Add {
        /// Local directory path, or an `sftp://` / `smb://` URL.
        target: String,
        #[arg(long)]
        name: Option<String>,
        /// Auto-rescan on change (local: filesystem events; remote: polling).
        #[arg(long)]
        watch: bool,
        /// Force the source kind: `local`|`sftp`|`smb` (default: inferred from the URL scheme).
        #[arg(long)]
        kind: Option<String>,
        /// Login user (SFTP/SMB); overrides any `user@` in the URL.
        #[arg(long)]
        username: Option<String>,
        /// Password (SFTP/SMB).
        #[arg(long)]
        password: Option<String>,
        /// Private key file for SFTP key auth.
        #[arg(long)]
        private_key: Option<String>,
        /// Passphrase for an encrypted private key.
        #[arg(long)]
        passphrase: Option<String>,
        /// SMB domain/workgroup.
        #[arg(long)]
        domain: Option<String>,
        /// Override the default port (22 SFTP / 445 SMB).
        #[arg(long)]
        port: Option<u16>,
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

#[derive(Subcommand)]
enum CollectionCmd {
    /// List collections and smart folders.
    List,
    /// Create a manual collection, or a smart folder with `--smart` + a query.
    Create {
        name: String,
        /// Make a smart folder (live saved query) instead of a manual collection.
        #[arg(long)]
        smart: bool,
        /// Smart-folder query: free text over filename.
        #[arg(long)]
        text: Option<String>,
        /// Smart-folder media filter: `audio`|`image`|`model`.
        #[arg(long)]
        media: Option<String>,
    },
    /// Delete a collection.
    Delete { id: String },
    /// Show a collection's assets (manual members, or the smart folder's live matches).
    Show {
        id: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Add assets to a manual collection.
    Add { id: String, assets: Vec<String> },
    /// Remove assets from a manual collection.
    Remove { id: String, assets: Vec<String> },
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
    let Cli { global, cmd } = cli;

    // The admin surface does not ride the LibraryService seam — dispatch it before opening the
    // engine backend (it uses the admin API over --connect, or the server store when embedded).
    if let Cmd::Admin { cmd } = cmd {
        return run_admin(&global, cmd).await;
    }

    let ctx = AuthContext::embedded();
    let lib = open(&global).await?;
    let json = global.json;

    match cmd {
        // Dispatched above (before the engine backend is opened).
        Cmd::Admin { .. } => unreachable!("admin is handled before the backend opens"),
        Cmd::Scan { source, delta, wait } => {
            let sources = match source {
                Some(s) => vec![parse_source_id(&s)?],
                None => Vec::new(),
            };
            let mode = if delta { ScanMode::Delta } else { ScanMode::Full };
            let job_id = lib.submit_scan(&ctx, ScanRequest { sources, mode }).await?;
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
            SourceCmd::Add {
                target,
                name,
                watch,
                kind,
                username,
                password,
                private_key,
                passphrase,
                domain,
                port,
            } => {
                let kind = resolve_source_kind(kind.as_deref(), &target)?;
                let id = lib
                    .add_source(
                        &ctx,
                        AddSource {
                            kind,
                            uri: target,
                            name,
                            options: SourceOptions {
                                watch,
                                username,
                                password,
                                private_key,
                                passphrase,
                                domain,
                                port,
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

        Cmd::Collections { cmd } => match cmd {
            CollectionCmd::List => {
                let colls = lib.list_collections(&ctx).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&colls)?);
                } else {
                    print_collections(&colls);
                }
            }
            CollectionCmd::Create { name, smart, text, media } => {
                let (kind, query) = if smart {
                    (CollectionKind::Smart, Some(build_query(text, media)?))
                } else {
                    (CollectionKind::Manual, None)
                };
                let id = lib
                    .create_collection(&ctx, NewCollection { name, kind, query })
                    .await?;
                if json {
                    println!("{}", serde_json::json!({ "id": id }));
                } else {
                    println!("created collection {id}");
                }
            }
            CollectionCmd::Delete { id } => {
                let cid = parse_collection_id(&id)?;
                lib.delete_collection(&ctx, &cid).await?;
                println!("deleted collection {cid}");
            }
            CollectionCmd::Show { id, limit } => {
                let cid = parse_collection_id(&id)?;
                let page = lib
                    .collection_assets(&ctx, &cid, PageParams { after: None, limit })
                    .await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&page)?);
                } else {
                    print_search(&page);
                }
            }
            CollectionCmd::Add { id, assets } => {
                let cid = parse_collection_id(&id)?;
                let add = parse_asset_ids(&assets)?;
                let n = add.len();
                lib.modify_collection_members(&ctx, &cid, CollectionMembers { add, remove: Vec::new() })
                    .await?;
                println!("added {n} asset(s) to {cid}");
            }
            CollectionCmd::Remove { id, assets } => {
                let cid = parse_collection_id(&id)?;
                let remove = parse_asset_ids(&assets)?;
                let n = remove.len();
                lib.modify_collection_members(&ctx, &cid, CollectionMembers { add: Vec::new(), remove })
                    .await?;
                println!("removed {n} asset(s) from {cid}");
            }
        },

        Cmd::Export {
            ids,
            format,
            out,
            collection,
            text,
            media,
            attribution_only,
        } => {
            let format = ExportFormat::parse(&format.to_ascii_lowercase())
                .ok_or_else(|| anyhow::anyhow!("invalid --format '{format}' (json|csv|sidecar)"))?;
            let assets = parse_asset_ids(&ids)?;
            let collection = collection.as_deref().map(parse_collection_id).transpose()?;
            // A text/media selector builds a query; otherwise leave it None (whole library / ids).
            let query = if text.is_some() || media.is_some() {
                Some(build_query(text, media)?)
            } else {
                None
            };
            let req = ExportRequest {
                assets,
                collection,
                query,
                format,
                output: out.to_string_lossy().into_owned(),
                attribution_only,
            };
            let report = lib.export(&ctx, req).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "exported {} asset(s) → {} ({} file(s))",
                    report.assets, report.output, report.files_written
                );
            }
        }

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

        Cmd::Analyze { ids, force, wait } => {
            let assets: Vec<AssetId> = ids
                .iter()
                .map(|s| {
                    s.parse::<AssetId>()
                        .map_err(|_| anyhow::anyhow!("invalid asset id: {s}"))
                })
                .collect::<anyhow::Result<_>>()?;
            let job_id = lib.submit_analyze(&ctx, AnalyzeRequest { assets, force }).await?;
            if json {
                println!("{}", serde_json::json!({ "job_id": job_id }));
            } else {
                println!("analysis started: job {job_id}");
            }
            if wait {
                wait_for_job(lib.as_ref(), &ctx, &job_id, json).await?;
            }
        }

        Cmd::Similar { id, limit } => {
            let asset: AssetId = id.parse().map_err(|_| anyhow::anyhow!("invalid asset id"))?;
            let page = lib
                .find_similar(&ctx, SimilarRequest { asset, k: limit, filters: Vec::new() })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else if page.items.is_empty() {
                println!("(no neighbours — is the asset analysed? run `3dam analyze`)");
            } else {
                for h in &page.items {
                    println!(
                        "{:>5.1}%  {}  {:<6} {:<5} {}",
                        h.score * 100.0,
                        h.asset.id,
                        h.asset.media.as_str(),
                        h.asset.format,
                        h.asset.name
                    );
                }
            }
        }

        Cmd::Dedup { near, media, limit } => {
            let kind = if near { DupKind::Near } else { DupKind::Exact };
            let media = match media.as_deref() {
                Some(m) => Some(
                    MediaType::parse(m).ok_or_else(|| anyhow::anyhow!("invalid media '{m}'"))?,
                ),
                None => None,
            };
            let groups = lib.list_duplicates(&ctx, DupRequest { kind, media, limit }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&groups)?);
            } else {
                print_dedup(&groups);
            }
        }

        Cmd::Tag { id, tag, reject } => {
            let asset: AssetId = id.parse().map_err(|_| anyhow::anyhow!("invalid asset id"))?;
            let action = if reject { ReviewAction::Reject } else { ReviewAction::Accept };
            lib.review_suggestion(&ctx, SuggestionReview { asset, tag: tag.clone(), action })
                .await?;
            let verb = if reject { "rejected" } else { "confirmed" };
            println!("{verb} tag '{tag}' on {asset}");
        }
    }
    Ok(())
}

fn print_dedup(groups: &[DupGroup]) {
    if groups.is_empty() {
        println!("(no duplicate groups)");
        return;
    }
    for (i, g) in groups.iter().enumerate() {
        println!(
            "group {} — {} {} members ({})",
            i + 1,
            g.members.len(),
            g.media.as_str(),
            g.signal
        );
        for m in &g.members {
            let keep = if m.id == g.suggested_keep { " ← keep" } else { "" };
            println!("  {}  {:>10}  {}{}", m.id, human_size(m.size), m.name, keep);
        }
    }
    println!("{} group(s)", groups.len());
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
                    "{} {:?}: {} assets{}",
                    format!("{:?}", job.kind).to_lowercase(),
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

// ── admin surface (tech-spec 10 §5) ───────────────────────────────────────────

/// Drive the admin API. Over `--connect` it calls the `/admin/api` routes; embedded it opens the
/// local server store directly (seeding it on first use, ADR 0009 §2).
async fn run_admin(global: &Global, cmd: AdminCmd) -> anyhow::Result<()> {
    let json = global.json;
    match &global.connect {
        Some(c) => {
            let url = parse_endpoint(c)?;
            let client = dam_client::ApiClient::connect_with_token(url, global.token.clone()).await?;
            run_admin_remote(&client, cmd, json).await
        }
        None => {
            let data_dir = global.data.clone().unwrap_or_else(default_data_dir);
            let store = dam_server::ServerStore::open(&data_dir.join("server.db"))?;
            run_admin_embedded(&store, cmd, json)
        }
    }
}

async fn run_admin_remote(
    client: &dam_client::ApiClient,
    cmd: AdminCmd,
    json: bool,
) -> anyhow::Result<()> {
    match cmd {
        AdminCmd::Status => print_status(&client.admin_status().await?, json)?,
        AdminCmd::Flags => print_flags(&client.admin_flags().await?, json)?,
        AdminCmd::Flag { key, set, confirm } => {
            let fk = parse_flag_key(&key)?;
            match set {
                Some(val) => {
                    let req = SetFlag {
                        value: parse_flag_value(fk, &val)?,
                        expected_version: None,
                        confirm,
                    };
                    print_flags(&[client.admin_set_flag(fk.as_str(), &req).await?], json)?;
                }
                None => {
                    let all = client.admin_flags().await?;
                    print_flags(&all.into_iter().filter(|f| f.key == fk).collect::<Vec<_>>(), json)?;
                }
            }
        }
        AdminCmd::Token { cmd } => match cmd {
            TokenCmd::Add { label, scope, expires } => {
                let req = NewToken { label, scopes: parse_scopes(&scope)?, expires };
                print_new_token(&client.admin_create_token(&req).await?, json)?;
            }
            TokenCmd::List => print_tokens(&client.admin_tokens().await?, json)?,
            TokenCmd::Revoke { id } => {
                client.admin_revoke_token(&id).await?;
                println!("revoked token {id}");
            }
        },
        AdminCmd::Audit { limit } => print_audit(&client.admin_audit(limit).await?, json)?,
    }
    Ok(())
}

fn run_admin_embedded(
    store: &dam_server::ServerStore,
    cmd: AdminCmd,
    json: bool,
) -> anyhow::Result<()> {
    match cmd {
        AdminCmd::Status => print_status(&store.status("(embedded)", true, false), json)?,
        AdminCmd::Flags => print_flags(&store.all_flags(), json)?,
        AdminCmd::Flag { key, set, confirm } => {
            let fk = parse_flag_key(&key)?;
            match set {
                Some(val) => {
                    let req = SetFlag {
                        value: parse_flag_value(fk, &val)?,
                        expected_version: None,
                        confirm,
                    };
                    print_flags(&[store.set_flag(fk, req, "cli")?], json)?;
                }
                None => print_flags(&[store.flag_info(fk)], json)?,
            }
        }
        AdminCmd::Token { cmd } => match cmd {
            TokenCmd::Add { label, scope, expires } => {
                let req = NewToken { label, scopes: parse_scopes(&scope)?, expires };
                print_new_token(&store.create_token(req, "cli")?, json)?;
            }
            TokenCmd::List => print_tokens(&store.list_tokens()?, json)?,
            TokenCmd::Revoke { id } => {
                store.revoke_token(&id, "cli")?;
                println!("revoked token {id}");
            }
        },
        AdminCmd::Audit { limit } => print_audit(&store.list_audit(limit)?, json)?,
    }
    Ok(())
}

fn parse_flag_key(key: &str) -> anyhow::Result<FlagKey> {
    FlagKey::parse(key).ok_or_else(|| {
        anyhow::anyhow!("unknown flag '{key}' (authentication | mcp_server | network_writes)")
    })
}

fn parse_flag_value(key: FlagKey, s: &str) -> anyhow::Result<FlagValue> {
    let v = s.trim().to_ascii_lowercase();
    Ok(match key {
        FlagKey::Authentication => FlagValue::Auth(match v.as_str() {
            "off" => AuthMode::Off,
            "anonymous" | "anon" => AuthMode::Anonymous,
            "token" => AuthMode::Token,
            _ => anyhow::bail!("authentication must be off|anonymous|token"),
        }),
        FlagKey::McpServer => FlagValue::Mcp(match v.replace('-', "_").as_str() {
            "off" => McpMode::Off,
            "read_only" | "readonly" | "read" => McpMode::ReadOnly,
            "read_write" | "readwrite" | "writes" => McpMode::ReadWrite,
            _ => anyhow::bail!("mcp_server must be off|read_only|read_write"),
        }),
        FlagKey::NetworkWrites => FlagValue::Bool(match v.as_str() {
            "true" | "on" | "yes" | "1" => true,
            "false" | "off" | "no" | "0" => false,
            _ => anyhow::bail!("network_writes must be true|false"),
        }),
    })
}

fn parse_scopes(list: &[String]) -> anyhow::Result<Scopes> {
    if list.is_empty() {
        return Ok(Scopes::none().with(Scope::Read).with(Scope::McpUse));
    }
    let mut s = Scopes::none();
    for item in list {
        s = s.with(match item.trim().to_ascii_lowercase().as_str() {
            "read" => Scope::Read,
            "write" => Scope::Write,
            "admin" => Scope::Admin,
            "mcp_use" | "mcp" => Scope::McpUse,
            "federate" => Scope::Federate,
            other => anyhow::bail!("unknown scope '{other}' (read|write|admin|mcp_use|federate)"),
        });
    }
    Ok(s)
}

fn show_flag_value(v: &FlagValue) -> String {
    match v {
        FlagValue::Auth(m) => format!("{m:?}").to_lowercase(),
        FlagValue::Mcp(m) => format!("{m:?}").to_lowercase(),
        FlagValue::Bool(b) => b.to_string(),
    }
}

fn print_status(s: &AdminStatus, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
        return Ok(());
    }
    println!("bind:           {}", s.bind);
    println!("localhost only: {}", s.localhost_only);
    println!("tls:            {}", s.tls);
    println!("auth:           {:?}", s.auth);
    println!("mcp:            {:?}", s.mcp);
    println!("network writes: {}", s.network_writes);
    println!("tokens:         {}", s.token_count);
    if s.exposed_without_auth {
        println!("⚠ exposed beyond localhost with no auth and no TLS");
    }
    Ok(())
}

fn print_flags(flags: &[FlagInfo], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(flags)?);
        return Ok(());
    }
    for f in flags {
        let live = if f.live { "live" } else { "restart" };
        println!(
            "{:<16} {:<12} v{}  ({live})",
            f.key.as_str(),
            show_flag_value(&f.value),
            f.version
        );
    }
    Ok(())
}

fn print_tokens(tokens: &[TokenInfo], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(tokens)?);
        return Ok(());
    }
    if tokens.is_empty() {
        println!("(no tokens)");
        return Ok(());
    }
    for t in tokens {
        let scopes: Vec<String> = t.scopes.to_vec().iter().map(|s| format!("{s:?}")).collect();
        println!("{}  {:<20} [{}]", t.token_id, t.label, scopes.join(","));
    }
    Ok(())
}

fn print_new_token(t: &NewTokenReply, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(t)?);
        return Ok(());
    }
    println!("token '{}' created ({})", t.label, t.token_id);
    println!("secret (shown once): {}", t.secret);
    Ok(())
}

fn print_audit(entries: &[AuditEntry], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(entries)?);
        return Ok(());
    }
    for e in entries {
        println!(
            "{}  {:<16} {:<14} {}",
            e.at,
            e.actor,
            e.action,
            e.target.clone().unwrap_or_default()
        );
    }
    Ok(())
}

// ── serve / mcp roles ────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "serve")]
struct ServeArgs {
    /// Address to bind (overrides the config file). Default: 127.0.0.1:7878.
    #[arg(long)]
    addr: Option<String>,
    /// Library data directory.
    #[arg(long)]
    data: Option<PathBuf>,
    /// Serve config file (TOML) — seeds flags and may set the bind (tech-spec 09 §A.1).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Allow binding beyond localhost without TLS (refused by default — ADR 0009 §4).
    #[arg(long)]
    insecure: bool,
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
    let addr: Option<SocketAddr> = match parsed.addr {
        Some(s) => match s.parse() {
            Ok(a) => Some(a),
            Err(_) => {
                eprintln!("error: invalid --addr '{s}'");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let cfg = ServeConfig {
        addr,
        data_dir: parsed.data.unwrap_or_else(default_data_dir),
        config: parsed.config,
        insecure: parsed.insecure,
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
            token: global.token.clone(),
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

fn parse_collection_id(s: &str) -> anyhow::Result<CollectionId> {
    s.parse()
        .map_err(|_| anyhow::anyhow!("invalid collection id: {s}"))
}

fn parse_asset_ids(ss: &[String]) -> anyhow::Result<Vec<AssetId>> {
    ss.iter()
        .map(|s| {
            s.parse::<AssetId>()
                .map_err(|_| anyhow::anyhow!("invalid asset id: {s}"))
        })
        .collect()
}

/// Resolve a source kind from an explicit `--kind` or the target's URL scheme (local by default).
fn resolve_source_kind(kind: Option<&str>, target: &str) -> anyhow::Result<SourceKind> {
    if let Some(k) = kind {
        return match k.to_ascii_lowercase().as_str() {
            "local" | "local_fs" | "localfs" | "fs" => Ok(SourceKind::LocalFs),
            "sftp" | "ssh" => Ok(SourceKind::Sftp),
            "smb" | "cifs" => Ok(SourceKind::Smb),
            other => Err(anyhow::anyhow!("invalid --kind '{other}' (local|sftp|smb)")),
        };
    }
    if target.starts_with("sftp://") {
        Ok(SourceKind::Sftp)
    } else if target.starts_with("smb://") {
        Ok(SourceKind::Smb)
    } else {
        Ok(SourceKind::LocalFs)
    }
}

/// Build a `QueryRequest` from optional free text + a media filter (smart folders / export).
fn build_query(text: Option<String>, media: Option<String>) -> anyhow::Result<QueryRequest> {
    let mut filters = Vec::new();
    if let Some(m) = media {
        MediaType::parse(&m).ok_or_else(|| anyhow::anyhow!("invalid media '{m}'"))?;
        filters.push(Filter {
            field: FacetField::MediaType,
            op: FilterOp::Eq,
            value: FilterValue::Str(m),
        });
    }
    Ok(QueryRequest {
        text,
        filters,
        ..Default::default()
    })
}

fn print_collections(colls: &[Collection]) {
    if colls.is_empty() {
        println!("(no collections — create one with `3dam collections create <name>`)");
        return;
    }
    for c in colls {
        let count = c
            .count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "·".into());
        println!(
            "{}  {:<6} {:>5}  {}",
            c.id,
            c.kind.as_str(),
            count,
            c.name
        );
    }
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
