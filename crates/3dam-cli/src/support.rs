//! Shared CLI parsing and output formatting helpers.
use super::*;

pub(crate) fn print_dedup(groups: &[DupGroup]) {
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
            let keep = if m.id == g.suggested_keep {
                " ← keep"
            } else {
                ""
            };
            println!("  {}  {:>10}  {}{}", m.id, human_size(m.size), m.name, keep);
        }
    }
    println!("{} group(s)", groups.len());
}

/// Pick the media-typed target from the requested `--to` format (audio = `wav`, else image).
pub(crate) fn build_convert_target(to: &str) -> anyhow::Result<ConvertTarget> {
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

pub(crate) fn apply_image_opts(
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

pub(crate) fn parse_collision(s: &str) -> anyhow::Result<CollisionRule> {
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

pub(crate) fn print_convert(r: &ConvertReport) {
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

pub(crate) async fn wait_for_job(
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

pub(crate) fn parse_endpoint(s: &str) -> anyhow::Result<Url> {
    let with_scheme = if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    };
    Ok(Url::parse(&with_scheme)?)
}

pub(crate) fn parse_source_id(s: &str) -> anyhow::Result<SourceId> {
    s.parse()
        .map_err(|_| anyhow::anyhow!("invalid source id: {s}"))
}

pub(crate) fn parse_collection_id(s: &str) -> anyhow::Result<CollectionId> {
    s.parse()
        .map_err(|_| anyhow::anyhow!("invalid collection id: {s}"))
}

pub(crate) fn parse_asset_ids(ss: &[String]) -> anyhow::Result<Vec<AssetId>> {
    ss.iter()
        .map(|s| {
            s.parse::<AssetId>()
                .map_err(|_| anyhow::anyhow!("invalid asset id: {s}"))
        })
        .collect()
}

/// Resolve a source kind from an explicit `--kind` or the target's URL scheme (local by default).
pub(crate) fn resolve_source_kind(kind: Option<&str>, target: &str) -> anyhow::Result<SourceKind> {
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
pub(crate) fn build_query(
    text: Option<String>,
    media: Option<String>,
) -> anyhow::Result<QueryRequest> {
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

pub(crate) fn print_collections(colls: &[Collection]) {
    if colls.is_empty() {
        println!("(no collections — create one with `3dam collections create <name>`)");
        return;
    }
    for c in colls {
        let count = c.count.map(|n| n.to_string()).unwrap_or_else(|| "·".into());
        println!("{}  {:<6} {:>5}  {}", c.id, c.kind.as_str(), count, c.name);
    }
}

pub(crate) fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

pub(crate) fn print_search(page: &dam_api::page::Page<AssetSummary>) {
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

pub(crate) fn print_sources(sources: &[SourceInfo]) {
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

pub(crate) fn print_stats(s: &LibraryStats) {
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
    if !s.tags.is_empty() {
        println!("top tags:");
        for (k, v) in &s.tags {
            println!("  {k:<20} {v}");
        }
    }
}

pub(crate) fn print_asset(a: &Asset) {
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

pub(crate) fn human_size(bytes: u64) -> String {
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
