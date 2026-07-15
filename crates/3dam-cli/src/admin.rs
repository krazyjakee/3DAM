//! The `admin` subcommand: talks to `/admin/api` over `--connect` or the local server store.
use super::*;
use crate::args::*;
use crate::support::human_size;

/// Drive the admin API. Over `--connect` it calls the `/admin/api` routes; embedded it opens the
/// local server store directly (seeding it on first use, ADR 0009 §2).
pub(crate) async fn run_admin(global: &Global, cmd: AdminCmd) -> anyhow::Result<()> {
    let json = global.json;
    match &global.connect {
        Some(c) => {
            let url = parse_endpoint(c)?;
            let client =
                dam_client::ApiClient::connect_with_token(url, global.token.clone()).await?;
            run_admin_remote(&client, cmd, json).await
        }
        None => {
            let data_dir = global.data.clone().unwrap_or_else(default_data_dir);
            let store = dam_server::ServerStore::open(&data_dir.join("server.db"))?;
            run_admin_embedded(&store, &data_dir, cmd, json).await
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
                    print_set_flag(&client.admin_set_flag(fk.as_str(), &req).await?, json)?;
                }
                None => {
                    let all = client.admin_flags().await?;
                    print_flags(
                        &all.into_iter().filter(|f| f.key == fk).collect::<Vec<_>>(),
                        json,
                    )?;
                }
            }
        }
        AdminCmd::Token { cmd } => match cmd {
            TokenCmd::Add {
                label,
                scope,
                expires,
            } => {
                let req = NewToken {
                    label,
                    scopes: parse_scopes(&scope)?,
                    expires,
                };
                print_new_token(&client.admin_create_token(&req).await?, json)?;
            }
            TokenCmd::List => print_tokens(&client.admin_tokens().await?, json)?,
            TokenCmd::Revoke { id } => {
                client.admin_revoke_token(&id).await?;
                println!("revoked token {id}");
            }
        },
        AdminCmd::Audit { limit } => print_audit(&client.admin_audit(limit).await?, json)?,
        AdminCmd::Maintenance { cmd } => match cmd {
            MaintenanceCmd::Usage => print_usage(&client.admin_storage_usage().await?, json)?,
            MaintenanceCmd::ClearCache { target } => print_clear_cache(
                &client
                    .admin_clear_cache(parse_cache_target(&target)?)
                    .await?,
                json,
            )?,
            MaintenanceCmd::ClearAnalysis => {
                print_clear_analysis(&client.admin_clear_analysis().await?, json)?
            }
            MaintenanceCmd::Vacuum => print_vacuum(&client.admin_vacuum().await?, json)?,
            MaintenanceCmd::Wipe { confirm } => {
                confirm_or_bail(confirm, "reset the catalog")?;
                print_wipe(&client.admin_wipe(true).await?, json)?;
            }
            MaintenanceCmd::FactoryReset { confirm } => {
                confirm_or_bail(confirm, "factory reset")?;
                print_factory_reset(&client.admin_factory_reset(true).await?, json)?;
            }
        },
    }
    Ok(())
}

async fn run_admin_embedded(
    store: &dam_server::ServerStore,
    data_dir: &std::path::Path,
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
                    print_set_flag(&store.set_flag(fk, req, "cli")?, json)?;
                }
                None => print_flags(&[store.flag_info(fk)], json)?,
            }
        }
        AdminCmd::Token { cmd } => match cmd {
            TokenCmd::Add {
                label,
                scope,
                expires,
            } => {
                let req = NewToken {
                    label,
                    scopes: parse_scopes(&scope)?,
                    expires,
                };
                print_new_token(&store.create_token(req, "cli")?, json)?;
            }
            TokenCmd::List => print_tokens(&store.list_tokens()?, json)?,
            TokenCmd::Revoke { id } => {
                store.revoke_token(&id, "cli")?;
                println!("revoked token {id}");
            }
        },
        AdminCmd::Audit { limit } => print_audit(&store.list_audit(limit)?, json)?,
        AdminCmd::Maintenance { cmd } => {
            // Maintenance touches library.db + caches, so the embedded path opens the engine
            // (its inherent ops are off the LibraryService seam) and audits via the server store.
            let lib = dam_core::EmbeddedLibrary::open(data_dir).await?;
            match cmd {
                MaintenanceCmd::Usage => print_usage(&lib.storage_usage().await?, json)?,
                MaintenanceCmd::ClearCache { target } => {
                    let t = parse_cache_target(&target)?;
                    let report = lib.clear_caches(t).await?;
                    store.audit(
                        "cli",
                        "maintenance.clear_cache",
                        None,
                        Some(serde_json::json!({ "files_deleted": report.files_deleted })),
                    )?;
                    print_clear_cache(&report, json)?;
                }
                MaintenanceCmd::ClearAnalysis => {
                    let report = lib.clear_analysis().await?;
                    store.audit("cli", "maintenance.clear_analysis", None, None)?;
                    print_clear_analysis(&report, json)?;
                }
                MaintenanceCmd::Vacuum => {
                    let report = lib.vacuum().await?;
                    store.audit("cli", "maintenance.vacuum", None, None)?;
                    print_vacuum(&report, json)?;
                }
                MaintenanceCmd::Wipe { confirm } => {
                    confirm_or_bail(confirm, "reset the catalog")?;
                    let report = lib.wipe_catalog().await?;
                    store.audit("cli", "maintenance.wipe", None, None)?;
                    print_wipe(&report, json)?;
                }
                MaintenanceCmd::FactoryReset { confirm } => {
                    confirm_or_bail(confirm, "factory reset")?;
                    let catalog = lib.wipe_catalog().await?;
                    let cache = lib.clear_caches(CacheTarget::All).await?;
                    let tokens_removed = store.factory_reset()?;
                    store.audit("cli", "maintenance.factory_reset", None, None)?;
                    print_factory_reset(
                        &FactoryResetReport {
                            catalog,
                            cache,
                            tokens_removed,
                        },
                        json,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn parse_cache_target(s: &str) -> anyhow::Result<CacheTarget> {
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "thumbnails" | "thumbs" | "thumb" => CacheTarget::Thumbnails,
        "previews" | "preview" => CacheTarget::Previews,
        "all" | "both" => CacheTarget::All,
        _ => anyhow::bail!("cache target must be thumbnails|previews|all"),
    })
}

/// Guard the destructive maintenance verbs: the CLI's own `--confirm` gate before the request even
/// leaves (the server independently re-checks `confirm=true`).
fn confirm_or_bail(confirm: bool, what: &str) -> anyhow::Result<()> {
    if !confirm {
        anyhow::bail!("{what} is destructive and irreversible — re-run with --confirm");
    }
    Ok(())
}

fn parse_flag_key(key: &str) -> anyhow::Result<FlagKey> {
    FlagKey::parse(key).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown flag '{key}' (authentication | mcp_server | network_writes | \
             auto_thumbnail | auto_analyze)"
        )
    })
}

fn parse_flag_value(key: FlagKey, s: &str) -> anyhow::Result<FlagValue> {
    let v = s.trim().to_ascii_lowercase();
    let parse_bool = |name: &str| -> anyhow::Result<bool> {
        match v.as_str() {
            "true" | "on" | "yes" | "1" => Ok(true),
            "false" | "off" | "no" | "0" => Ok(false),
            _ => anyhow::bail!("{name} must be true|false"),
        }
    };
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
        FlagKey::NetworkWrites => FlagValue::Bool(parse_bool("network_writes")?),
        FlagKey::AutoThumbnail => FlagValue::Bool(parse_bool("auto_thumbnail")?),
        FlagKey::AutoAnalyze => FlagValue::Bool(parse_bool("auto_analyze")?),
        FlagKey::Federation => FlagValue::Bool(parse_bool("federation")?),
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

fn print_set_flag(reply: &SetFlagReply, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(reply)?);
        return Ok(());
    }
    print_flags(std::slice::from_ref(&reply.flag), false)?;
    if let Some(t) = &reply.bootstrap_token {
        println!("authentication is on and no admin credential existed — minted the owner token:");
        print_new_token(t, false)?;
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

fn print_usage(u: &StorageUsage, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(u)?);
        return Ok(());
    }
    println!("data dir:      {}", u.data_dir);
    println!("library.db:    {}", human_size(u.library_db_bytes));
    println!("server.db:     {}", human_size(u.server_db_bytes));
    println!(
        "thumbnails:    {} ({} files)",
        human_size(u.thumbnails.bytes),
        u.thumbnails.files
    );
    println!(
        "previews:      {} ({} files)",
        human_size(u.previews.bytes),
        u.previews.files
    );
    println!("assets:        {}", u.asset_count);
    println!("sources:       {}", u.source_count);
    Ok(())
}

fn print_clear_cache(r: &ClearCacheReport, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    println!(
        "cleared {} files, freed {}",
        r.files_deleted,
        human_size(r.bytes_freed)
    );
    Ok(())
}

fn print_clear_analysis(r: &ClearAnalysisReport, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    println!(
        "removed {} suggestions, {} embeddings; assets marked for re-analysis",
        r.suggestions_removed, r.embeddings_removed
    );
    Ok(())
}

fn print_vacuum(r: &VacuumReport, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    println!(
        "compacted library.db: {} → {} (reclaimed {})",
        human_size(r.before_bytes),
        human_size(r.after_bytes),
        human_size(r.reclaimed_bytes)
    );
    Ok(())
}

fn print_wipe(r: &WipeReport, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    println!(
        "catalog reset: {} assets, {} sources, {} collections, {} tags removed (files untouched)",
        r.assets_removed, r.sources_removed, r.collections_removed, r.tags_removed
    );
    Ok(())
}

fn print_factory_reset(r: &FactoryResetReport, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    println!(
        "factory reset complete: {} assets removed, {} tokens revoked, {} freed from caches",
        r.catalog.assets_removed,
        r.tokens_removed,
        human_size(r.cache.bytes_freed)
    );
    println!("flags reset to defaults; audit log cleared");
    Ok(())
}

// ── serve / mcp roles ────────────────────────────────────────────────────────
