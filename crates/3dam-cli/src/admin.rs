//! The `admin` subcommand: talks to `/admin/api` over `--connect` or the local server store.
use super::*;
use crate::args::*;

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
