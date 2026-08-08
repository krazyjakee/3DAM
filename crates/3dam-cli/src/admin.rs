//! The `admin` subcommand: talks to `/admin/api` over `--connect` or the local server store.
use super::*;
use crate::args::*;
use crate::support::human_size;
mod parse;
mod print;

use parse::*;
use print::*;

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
