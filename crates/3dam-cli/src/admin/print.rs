//! Human-readable and JSON presentation for admin command results.

use super::*;

pub(super) fn print_status(s: &AdminStatus, json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_flags(flags: &[FlagInfo], json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_tokens(tokens: &[TokenInfo], json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_accounts(accounts: &[AccountInfo], json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_account(a: &AccountInfo, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(a)?);
        return Ok(());
    }
    print_accounts(std::slice::from_ref(a), false)
}

pub(super) fn print_groups(groups: &[GroupInfo], json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_group(g: &GroupInfo, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(g)?);
        return Ok(());
    }
    print_groups(std::slice::from_ref(g), false)
}

pub(super) fn print_shares(shares: &[ShareInfo], json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_share(s: &ShareInfo, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
        return Ok(());
    }
    print_shares(std::slice::from_ref(s), false)
}

pub(super) fn print_set_flag(reply: &SetFlagReply, json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_new_token(t: &NewTokenReply, json: bool) -> anyhow::Result<()> {
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
pub(super) fn print_oidc(cfg: &Option<OidcConfigInfo>, json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_oidc_identities(ids: &[OidcIdentity], json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_audit(entries: &[AuditEntry], json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_usage(u: &StorageUsage, json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_clear_cache(r: &ClearCacheReport, json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_clear_analysis(r: &ClearAnalysisReport, json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_vacuum(r: &VacuumReport, json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_wipe(r: &WipeReport, json: bool) -> anyhow::Result<()> {
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

pub(super) fn print_factory_reset(r: &FactoryResetReport, json: bool) -> anyhow::Result<()> {
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
