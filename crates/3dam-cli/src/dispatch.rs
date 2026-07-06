//! Command dispatch: opens the `LibraryService` backend and routes each subcommand.
use super::*;
use crate::admin::run_admin;
use crate::args::*;
use crate::support::*;

pub(crate) async fn dispatch(cli: Cli) -> anyhow::Result<()> {
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
        Cmd::Scan {
            source,
            delta,
            wait,
        } => {
            let sources = match source {
                Some(s) => vec![parse_source_id(&s)?],
                None => Vec::new(),
            };
            let mode = if delta {
                ScanMode::Delta
            } else {
                ScanMode::Full
            };
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
            CollectionCmd::Create {
                name,
                smart,
                text,
                media,
            } => {
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
                lib.modify_collection_members(
                    &ctx,
                    &cid,
                    CollectionMembers {
                        add,
                        remove: Vec::new(),
                    },
                )
                .await?;
                println!("added {n} asset(s) to {cid}");
            }
            CollectionCmd::Remove { id, assets } => {
                let cid = parse_collection_id(&id)?;
                let remove = parse_asset_ids(&assets)?;
                let n = remove.len();
                lib.modify_collection_members(
                    &ctx,
                    &cid,
                    CollectionMembers {
                        add: Vec::new(),
                        remove,
                    },
                )
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
            let job_id = lib
                .submit_analyze(&ctx, AnalyzeRequest { assets, force })
                .await?;
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
            let asset: AssetId = id
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid asset id"))?;
            let page = lib
                .find_similar(
                    &ctx,
                    SimilarRequest {
                        asset,
                        k: limit,
                        filters: Vec::new(),
                    },
                )
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
            let groups = lib
                .list_duplicates(&ctx, DupRequest { kind, media, limit })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&groups)?);
            } else {
                print_dedup(&groups);
            }
        }

        Cmd::Tag { id, tag, reject } => {
            let asset: AssetId = id
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid asset id"))?;
            let action = if reject {
                ReviewAction::Reject
            } else {
                ReviewAction::Accept
            };
            lib.review_suggestion(
                &ctx,
                SuggestionReview {
                    asset,
                    tag: tag.clone(),
                    action,
                },
            )
            .await?;
            let verb = if reject { "rejected" } else { "confirmed" };
            println!("{verb} tag '{tag}' on {asset}");
        }
    }
    Ok(())
}
