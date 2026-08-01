//! The `admin` subcommand: talks to `/admin/api` over `--connect` or the local server store.
use super::*;
use crate::args::*;
use crate::support::human_size;
use dam_api::accounts::{
    AccountInfo, GroupInfo, GroupMembers, NewAccount, NewGroup, NewShare, Role, ShareAccess,
    ShareInfo, ShareResource, UpdateAccount,
};

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
        AdminCmd::Accounts { cmd } => match cmd {
            AccountCmd::List => print_accounts(&client.admin_accounts().await?, json)?,
            AccountCmd::Add {
                username,
                password,
                role,
                display_name,
            } => {
                let req = NewAccount {
                    username,
                    password,
                    display_name,
                    role: parse_role(&role)?,
                };
                print_account(&client.admin_create_account(&req).await?, json)?;
            }
            AccountCmd::Update {
                id,
                role,
                name,
                disabled,
                password,
            } => {
                let req = build_update_account(role, name, disabled, password)?;
                print_account(&client.admin_update_account(&id, &req).await?, json)?;
            }
            AccountCmd::Remove { id } => {
                client.admin_delete_account(&id).await?;
                println!("removed account {id}");
            }
            AccountCmd::RevokeSessions { id } => {
                let n = client.admin_revoke_account_sessions(&id).await?;
                println!("revoked {n} session(s) for account {id}");
            }
        },
        AdminCmd::Groups { cmd } => match cmd {
            GroupCmd::List => print_groups(&client.admin_groups().await?, json)?,
            GroupCmd::Add { name } => {
                print_group(&client.admin_create_group(&NewGroup { name }).await?, json)?
            }
            GroupCmd::Remove { id } => {
                client.admin_delete_group(&id).await?;
                println!("removed group {id}");
            }
            GroupCmd::Members { id, account_ids } => print_group(
                &client
                    .admin_set_group_members(&id, &GroupMembers { account_ids })
                    .await?,
                json,
            )?,
        },
        AdminCmd::Share { cmd } => match cmd {
            ShareCmd::List => print_shares(&client.admin_shares().await?, json)?,
            ShareCmd::Add {
                source,
                collection,
                account,
                group,
                write,
            } => {
                let req = build_new_share(source, collection, account, group, write)?;
                print_share(&client.admin_create_share(&req).await?, json)?;
            }
            ShareCmd::Remove { id } => {
                client.admin_delete_share(&id).await?;
                println!("removed share {id}");
            }
        },
        AdminCmd::Oidc { cmd } => match cmd {
            OidcCmd::Show => print_oidc(&client.admin_oidc().await?, json)?,
            OidcCmd::Set {
                issuer,
                client_id,
                redirect_url,
                client_secret,
                scope,
                provisioning,
            } => {
                let req = build_set_oidc(
                    issuer,
                    client_id,
                    redirect_url,
                    client_secret,
                    scope,
                    &provisioning,
                )?;
                print_oidc(&client.admin_set_oidc(&req).await?, json)?;
            }
            OidcCmd::Identities => {
                print_oidc_identities(&client.admin_oidc_identities().await?, json)?
            }
            OidcCmd::Link {
                subject,
                account_id,
            } => {
                let req = LinkOidcIdentity {
                    subject,
                    account_id,
                };
                print_oidc_identities(&client.admin_link_oidc_identity(&req).await?, json)?;
            }
            OidcCmd::Unlink { subject, issuer } => print_oidc_identities(
                &client
                    .admin_unlink_oidc_identity(&subject, issuer.as_deref())
                    .await?,
                json,
            )?,
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
        // Accounts/groups/shares mirror the HTTP routes' flag guard: off means the surface does
        // not exist (ADR 0004), embedded included — the store methods would happily write rows.
        AdminCmd::Accounts { cmd } => {
            require_accounts(store)?;
            match cmd {
                AccountCmd::List => print_accounts(&store.list_accounts()?, json)?,
                AccountCmd::Add {
                    username,
                    password,
                    role,
                    display_name,
                } => {
                    let req = NewAccount {
                        username,
                        password,
                        display_name,
                        role: parse_role(&role)?,
                    };
                    print_account(&store.create_account(&req, "cli")?, json)?;
                }
                AccountCmd::Update {
                    id,
                    role,
                    name,
                    disabled,
                    password,
                } => {
                    let req = build_update_account(role, name, disabled, password)?;
                    print_account(&store.update_account(&id, &req, "cli")?, json)?;
                }
                AccountCmd::Remove { id } => {
                    store.delete_account(&id, "cli")?;
                    println!("removed account {id}");
                }
                AccountCmd::RevokeSessions { id } => {
                    let n = store.revoke_account_sessions(&id, "cli")?;
                    println!("revoked {n} session(s) for account {id}");
                }
            }
        }
        AdminCmd::Groups { cmd } => {
            require_accounts(store)?;
            match cmd {
                GroupCmd::List => print_groups(&store.list_groups()?, json)?,
                GroupCmd::Add { name } => {
                    print_group(&store.create_group(&NewGroup { name }, "cli")?, json)?
                }
                GroupCmd::Remove { id } => {
                    store.delete_group(&id, "cli")?;
                    println!("removed group {id}");
                }
                GroupCmd::Members { id, account_ids } => {
                    print_group(&store.set_group_members(&id, &account_ids, "cli")?, json)?
                }
            }
        }
        AdminCmd::Share { cmd } => {
            require_accounts(store)?;
            match cmd {
                ShareCmd::List => print_shares(&store.list_shares()?, json)?,
                ShareCmd::Add {
                    source,
                    collection,
                    account,
                    group,
                    write,
                } => {
                    // Unlike the HTTP route, the embedded path doesn't check that the source/
                    // collection is live in library.db — the id is a soft reference and orphans are
                    // GC'd, so a dangling grant is clutter, never an escalation. Not worth opening
                    // the engine here.
                    let req = build_new_share(source, collection, account, group, write)?;
                    print_share(&store.create_share(&req, "cli")?, json)?;
                }
                ShareCmd::Remove { id } => {
                    store.delete_share(&id, "cli")?;
                    println!("removed share {id}");
                }
            }
        }
        // No flag guard, matching the routes: the accounts gate would hide the surface an operator
        // needs *before* turning `oidc` on, and the `oidc` flag gates logins, not configuration.
        AdminCmd::Oidc { cmd } => match cmd {
            OidcCmd::Show => print_oidc(&store.oidc_config_info()?, json)?,
            OidcCmd::Set {
                issuer,
                client_id,
                redirect_url,
                client_secret,
                scope,
                provisioning,
            } => {
                let req = build_set_oidc(
                    issuer,
                    client_id,
                    redirect_url,
                    client_secret,
                    scope,
                    &provisioning,
                )?;
                // Validation lives in the store, so both paths reject the same inputs identically.
                store.set_oidc_config(&req, "cli")?;
                print_oidc(&store.oidc_config_info()?, json)?;
            }
            OidcCmd::Identities => print_oidc_identities(&store.list_oidc_identities()?, json)?,
            OidcCmd::Link {
                subject,
                account_id,
            } => {
                // The issuer is the configured one, never a CLI argument — same rule as the route.
                let issuer = configured_issuer(
                    store,
                    "configure the OIDC provider before linking identities to it",
                )?;
                // Resolve the account first so a bad id is a clean not-found, not a FK error.
                let account = store.get_account(&account_id)?;
                store.link_oidc_identity(&issuer, subject.trim(), &account.account_id, "cli")?;
                print_oidc_identities(&store.list_oidc_identities()?, json)?;
            }
            OidcCmd::Unlink { subject, issuer } => {
                // `--issuer` names a link left behind by a previous issuer; without it, the
                // configured one, which is what an ordinary unlink means. Same rule as the route.
                let issuer = match issuer {
                    Some(i) => i,
                    None => configured_issuer(
                        store,
                        "no OIDC provider is configured — pass --issuer to remove a link left \
                         behind by a previous one",
                    )?,
                };
                store.unlink_oidc_identity(&issuer, &subject, "cli")?;
                print_oidc_identities(&store.list_oidc_identities()?, json)?;
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

/// The CLI end of the one `UserAccounts` guard. The predicate itself lives on the store
/// (`ServerStore::require_user_accounts`, also the HTTP router's single `route_layer`); this only
/// translates its `NotFound` into the CLI's error channel with an actionable hint.
fn require_accounts(store: &dam_server::ServerStore) -> anyhow::Result<()> {
    store.require_user_accounts().map_err(|_| {
        anyhow::anyhow!("user accounts are disabled (enable the user_accounts flag first)")
    })
}

fn parse_role(s: &str) -> anyhow::Result<Role> {
    Role::parse(&s.trim().to_ascii_lowercase())
        .ok_or_else(|| anyhow::anyhow!("role must be admin|editor|viewer"))
}

/// Assemble the partial update, rejecting a no-op up front so `update <id>` alone doesn't
/// round-trip just to change nothing.
fn build_update_account(
    role: Option<String>,
    name: Option<String>,
    disabled: Option<bool>,
    password: Option<String>,
) -> anyhow::Result<UpdateAccount> {
    let req = UpdateAccount {
        display_name: name,
        role: role.as_deref().map(parse_role).transpose()?,
        disabled,
        password,
    };
    if req.display_name.is_none()
        && req.role.is_none()
        && req.disabled.is_none()
        && req.password.is_none()
    {
        anyhow::bail!("nothing to update — pass --role, --name, --disabled, or --password");
    }
    Ok(req)
}

/// Fold the four share flags into the wire shape, enforcing the exactly-one rules the CLI can
/// state more helpfully than a server 400.
fn build_new_share(
    source: Option<String>,
    collection: Option<String>,
    account: Option<String>,
    group: Option<String>,
    write: bool,
) -> anyhow::Result<NewShare> {
    let (resource, resource_id) = match (source, collection) {
        (Some(id), None) => (ShareResource::Source, id),
        (None, Some(id)) => (ShareResource::Collection, id),
        _ => anyhow::bail!("share exactly one resource: --source <id> or --collection <id>"),
    };
    if account.is_some() == group.is_some() {
        anyhow::bail!("share to exactly one target: --account <id> or --group <id>");
    }
    Ok(NewShare {
        resource,
        resource_id,
        account_id: account,
        group_id: group,
        access: if write {
            ShareAccess::Write
        } else {
            ShareAccess::Read
        },
    })
}

/// The configured provider's issuer, which is the other half of an identity link's key. Embedded
/// only: over `--connect` the route reads it server-side. `missing` is the route's own wording for
/// the unconfigured case, and it is raised as the route's own `LibError` variant rather than a bare
/// string, so the two paths fail with the same rendered message and not just the same sentence.
fn configured_issuer(store: &dam_server::ServerStore, missing: &str) -> anyhow::Result<String> {
    match store.oidc_config_info()? {
        Some(cfg) => Ok(cfg.config.issuer),
        None => Err(LibError::BadRequest(missing.to_string()).into()),
    }
}

/// Fold the `oidc set` flags into the wire shape. An absent `--client-secret` stays `None`, which
/// the store reads as "keep the stored one" — the whole reason the field is optional.
fn build_set_oidc(
    issuer: String,
    client_id: String,
    redirect_url: String,
    client_secret: Option<String>,
    scopes: Vec<String>,
    provisioning: &str,
) -> anyhow::Result<SetOidcConfig> {
    Ok(SetOidcConfig {
        config: OidcConfig {
            issuer,
            client_id,
            redirect_url,
            scopes: scopes
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            provisioning: parse_provisioning(provisioning)?,
        },
        client_secret,
    })
}

fn parse_provisioning(s: &str) -> anyhow::Result<OidcProvisioning> {
    Ok(
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "linked" => OidcProvisioning::Linked,
            "auto_viewer" | "viewer" => OidcProvisioning::AutoViewer,
            "auto_editor" | "editor" => OidcProvisioning::AutoEditor,
            _ => anyhow::bail!("provisioning must be linked|auto_viewer|auto_editor"),
        },
    )
}

fn show_provisioning(p: OidcProvisioning) -> &'static str {
    match p {
        OidcProvisioning::Linked => "linked",
        OidcProvisioning::AutoViewer => "auto_viewer",
        OidcProvisioning::AutoEditor => "auto_editor",
    }
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
        FlagKey::UserAccounts => FlagValue::Bool(parse_bool("user_accounts")?),
        FlagKey::Upload => FlagValue::Bool(parse_bool("upload")?),
        FlagKey::Oidc => FlagValue::Bool(parse_bool("oidc")?),
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
    // The only surface that writes into a registered source (issue #80) — worth a line of its own
    // in the "am I safe to expose?" view rather than being inferred from `network writes`.
    println!(
        "uploads:        {}",
        if s.upload_enabled { "on" } else { "off" }
    );
    println!("tokens:         {}", s.token_count);
    println!(
        "accounts:       {}",
        if s.accounts_enabled { "on" } else { "off" }
    );
    if s.accounts_enabled {
        println!("account count:  {}", s.account_count);
    }
    if s.exposed_without_auth {
        println!("⚠ exposed beyond localhost with no auth and no TLS");
    }
    if s.unclaimed {
        println!("⚠ instance unclaimed — the next loopback signup becomes admin");
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

fn print_accounts(accounts: &[AccountInfo], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(accounts)?);
        return Ok(());
    }
    if accounts.is_empty() {
        println!("(no accounts)");
        return Ok(());
    }
    for a in accounts {
        // Timestamps stay epoch-ms like the audit listing — `--json` is the machine surface.
        let state = if a.disabled { "disabled" } else { "" };
        let last = a
            .last_login
            .map(|t| t.to_string())
            .unwrap_or_else(|| "never".into());
        println!(
            "{}  {:<20} {:<7} {:<9} created {}  last login {}",
            a.account_id, a.username, a.role, state, a.created, last
        );
    }
    Ok(())
}

fn print_account(a: &AccountInfo, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(a)?);
        return Ok(());
    }
    print_accounts(std::slice::from_ref(a), false)
}

fn print_groups(groups: &[GroupInfo], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(groups)?);
        return Ok(());
    }
    if groups.is_empty() {
        println!("(no groups)");
        return Ok(());
    }
    for g in groups {
        println!(
            "{}  {:<20} {} member(s)",
            g.group_id,
            g.name,
            g.members.len()
        );
    }
    Ok(())
}

fn print_group(g: &GroupInfo, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(g)?);
        return Ok(());
    }
    print_groups(std::slice::from_ref(g), false)
}

fn print_shares(shares: &[ShareInfo], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(shares)?);
        return Ok(());
    }
    if shares.is_empty() {
        println!("(no shares)");
        return Ok(());
    }
    for s in shares {
        let target = match (&s.account_id, &s.group_id) {
            (Some(a), _) => format!("account {a}"),
            (_, Some(g)) => format!("group {g}"),
            // Can't happen (schema CHECK) — but a corrupt row shouldn't panic a listing.
            _ => "(no target)".into(),
        };
        println!(
            "{}  {:<10} {}  {:<5} → {}",
            s.share_id,
            s.resource.as_str(),
            s.resource_id,
            s.access.as_str(),
            target
        );
    }
    Ok(())
}

fn print_share(s: &ShareInfo, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
        return Ok(());
    }
    print_shares(std::slice::from_ref(s), false)
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

/// The provider config. `None` — no provider configured — is a normal state, not an error, so it
/// prints a sentence rather than `None`; `--json` emits the DTO (`null`) verbatim.
///
/// There is no client-secret line beyond "is one on file": [`OidcConfigInfo`] has no field to hold
/// one, so the never-print rule (tech-spec 10 §5) is a property of the type, not of this function.
fn print_oidc(cfg: &Option<OidcConfigInfo>, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(cfg)?);
        return Ok(());
    }
    let Some(c) = cfg else {
        println!("(no OIDC provider configured)");
        return Ok(());
    };
    println!("issuer:        {}", c.config.issuer);
    println!("client id:     {}", c.config.client_id);
    println!("redirect url:  {}", c.config.redirect_url);
    println!(
        "scopes:        {}",
        if c.config.scopes.is_empty() {
            "(openid only)".to_string()
        } else {
            c.config.scopes.join(",")
        }
    );
    println!(
        "provisioning:  {}",
        show_provisioning(c.config.provisioning)
    );
    println!(
        "client secret: {}",
        if c.client_secret_set {
            "set"
        } else {
            "not set"
        }
    );
    Ok(())
}

fn print_oidc_identities(ids: &[OidcIdentity], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(ids)?);
        return Ok(());
    }
    if ids.is_empty() {
        println!("(no identities linked)");
        return Ok(());
    }
    for i in ids {
        // Timestamps stay epoch-ms like the other admin listings — `--json` is the machine surface.
        println!(
            "{}  {:<24} {:<20} {}  linked {}",
            i.account_id, i.subject, i.username, i.issuer, i.linked_at
        );
    }
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
