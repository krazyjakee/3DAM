//! Parsing and validation for admin command arguments and request DTOs.

use super::*;

pub(super) fn parse_cache_target(s: &str) -> anyhow::Result<CacheTarget> {
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "thumbnails" | "thumbs" | "thumb" => CacheTarget::Thumbnails,
        "previews" | "preview" => CacheTarget::Previews,
        "all" | "both" => CacheTarget::All,
        _ => anyhow::bail!("cache target must be thumbnails|previews|all"),
    })
}

/// Guard the destructive maintenance verbs: the CLI's own `--confirm` gate before the request even
/// leaves (the server independently re-checks `confirm=true`).
pub(super) fn confirm_or_bail(confirm: bool, what: &str) -> anyhow::Result<()> {
    if !confirm {
        anyhow::bail!("{what} is destructive and irreversible — re-run with --confirm");
    }
    Ok(())
}

/// The CLI end of the one `UserAccounts` guard. The predicate itself lives on the store
/// (`ServerStore::require_user_accounts`, also the HTTP router's single `route_layer`); this only
/// translates its `NotFound` into the CLI's error channel with an actionable hint.
pub(super) fn require_accounts(store: &dam_server::ServerStore) -> anyhow::Result<()> {
    store.require_user_accounts().map_err(|_| {
        anyhow::anyhow!("user accounts are disabled (enable the user_accounts flag first)")
    })
}

pub(super) fn parse_role(s: &str) -> anyhow::Result<Role> {
    Role::parse(&s.trim().to_ascii_lowercase())
        .ok_or_else(|| anyhow::anyhow!("role must be admin|editor|viewer"))
}

/// Assemble the partial update, rejecting a no-op up front so `update <id>` alone doesn't
/// round-trip just to change nothing.
pub(super) fn build_update_account(
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
pub(super) fn build_new_share(
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
pub(super) fn configured_issuer(
    store: &dam_server::ServerStore,
    missing: &str,
) -> anyhow::Result<String> {
    match store.oidc_config_info()? {
        Some(cfg) => Ok(cfg.config.issuer),
        None => Err(LibError::BadRequest(missing.to_string()).into()),
    }
}

/// Fold the `oidc set` flags into the wire shape. An absent `--client-secret` stays `None`, which
/// the store reads as "keep the stored one" — the whole reason the field is optional.
pub(super) fn build_set_oidc(
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

pub(super) fn parse_provisioning(s: &str) -> anyhow::Result<OidcProvisioning> {
    Ok(
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "linked" => OidcProvisioning::Linked,
            "auto_viewer" | "viewer" => OidcProvisioning::AutoViewer,
            "auto_editor" | "editor" => OidcProvisioning::AutoEditor,
            _ => anyhow::bail!("provisioning must be linked|auto_viewer|auto_editor"),
        },
    )
}

pub(super) fn show_provisioning(p: OidcProvisioning) -> &'static str {
    match p {
        OidcProvisioning::Linked => "linked",
        OidcProvisioning::AutoViewer => "auto_viewer",
        OidcProvisioning::AutoEditor => "auto_editor",
    }
}

pub(super) fn parse_flag_key(key: &str) -> anyhow::Result<FlagKey> {
    FlagKey::parse(key).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown flag '{key}' (authentication | mcp_server | network_writes | \
             auto_thumbnail | auto_analyze)"
        )
    })
}

pub(super) fn parse_flag_value(key: FlagKey, s: &str) -> anyhow::Result<FlagValue> {
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

pub(super) fn parse_scopes(list: &[String]) -> anyhow::Result<Scopes> {
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

pub(super) fn show_flag_value(v: &FlagValue) -> String {
    match v {
        FlagValue::Auth(m) => format!("{m:?}").to_lowercase(),
        FlagValue::Mcp(m) => format!("{m:?}").to_lowercase(),
        FlagValue::Bool(b) => b.to_string(),
    }
}
