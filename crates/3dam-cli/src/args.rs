//! CLI argument schema (clap): the command tree and global flags. Parsed in `lib::run`.
use super::*;

#[derive(Parser)]
#[command(name = "3dam", about = "3DAM — game-asset manager (CLI)", version)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) global: Global,
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Args)]
pub(crate) struct Global {
    /// Connect to a remote `3dam serve` instead of the local library.
    #[arg(long, global = true, value_name = "URL")]
    pub(crate) connect: Option<String>,
    /// Bearer token presented to a `--connect` server (required when it runs in token-auth mode).
    #[arg(long, global = true, value_name = "TOKEN")]
    pub(crate) token: Option<String>,
    /// Override the library data directory (embedded mode).
    #[arg(long, global = true, value_name = "DIR")]
    pub(crate) data: Option<PathBuf>,
    /// Emit machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    pub(crate) json: bool,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
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
        /// Limit to one source id.
        #[arg(long)]
        source: Option<String>,
        /// Scope to a folder subtree: a source-relative path prefix (issue #66). Paths are
        /// source-relative, so this requires `--source`.
        #[arg(long, requires = "source")]
        path: Option<String>,
        /// With `--path`, list only that folder's own files and not its subfolders.
        #[arg(long, requires = "path")]
        no_subfolders: bool,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Match strategy (semantic-search M5): `lexical` (FTS + synonyms), `hybrid` (also pulls in
        /// embedding neighbours of the matches), or `semantic` (rank by that neighbourhood).
        #[arg(long, value_enum, default_value_t = SearchModeArg::Lexical)]
        mode: SearchModeArg,
    },
    /// Manage sources.
    Sources {
        #[command(subcommand)]
        cmd: SourceCmd,
    },
    /// List the immediate subfolders of a source directory, with subtree asset counts (issue #66).
    Folders {
        /// Source id (see `sources list`).
        source: String,
        /// Source-relative folder to list (default: the source root).
        prefix: Option<String>,
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
        /// Media-type query selector: `audio`|`image`|`model`|`video`|`document`.
        #[arg(long)]
        media: Option<String>,
        /// Only the assets that need crediting, with attribution columns (the credits list).
        #[arg(long)]
        attribution_only: bool,
    },
    /// Show library statistics.
    Stats {
        /// Scope the numbers to one source id; a federated source reports the peer's own counts.
        #[arg(long)]
        source: Option<String>,
    },
    /// Show one asset's full record.
    Get {
        /// Asset id (UUID).
        id: String,
    },
    /// Remove an asset from the catalog. Non-destructive to the source file; optionally block its
    /// content hash so a later scan/watch/auto-rescan never re-imports it (issue #21).
    Remove {
        /// Asset id (UUID).
        id: String,
        /// Also block the content hash from re-import by any future scan.
        #[arg(long)]
        block: bool,
    },
    /// Inspect or clear the rescan blocklist — content hashes removed with `remove --block`.
    Blocklist {
        #[command(subcommand)]
        cmd: BlocklistCmd,
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
        /// Limit to one media type: `audio`|`image`|`model`|`video`|`document`.
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
    /// Read, write, or clear an asset's free-text note (issue #81).
    ///
    /// With no flag it prints the current note (nothing at all if there isn't one, so it pipes
    /// cleanly). `--set` replaces it; `--clear` removes it.
    Note {
        /// Asset id.
        id: String,
        /// Replace the note with this text.
        #[arg(long, value_name = "TEXT", conflicts_with = "clear")]
        set: Option<String>,
        /// Clear the note.
        #[arg(long)]
        clear: bool,
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
pub(crate) enum AdminCmd {
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
    /// Manage user accounts (requires the `user_accounts` flag; issue #42).
    Accounts {
        #[command(subcommand)]
        cmd: AccountCmd,
    },
    /// Manage groups — flat share targets ("Audio Team"), never permission levels.
    Groups {
        #[command(subcommand)]
        cmd: GroupCmd,
    },
    /// Manage shares: grant an account or group access to a source or collection.
    Share {
        #[command(subcommand)]
        cmd: ShareCmd,
    },
    /// Configure the OIDC login provider and link provider subjects to accounts (issue #41).
    ///
    /// Deliberately *not* gated on the `oidc` flag, mirroring the admin routes: an operator
    /// configures and links first, then switches the exposure-increasing flag on.
    Oidc {
        #[command(subcommand)]
        cmd: OidcCmd,
    },
    /// Show the audit log (most recent first).
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Storage & maintenance: report usage, clear caches/analysis, compact, or reset the library.
    Maintenance {
        #[command(subcommand)]
        cmd: MaintenanceCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum MaintenanceCmd {
    /// Report library.db/server.db sizes, cache sizes/counts, and catalog counts.
    Usage,
    /// Delete regenerable derivative caches (regenerated on next view).
    ClearCache {
        /// Which tier to clear: thumbnails | previews | all (default: all).
        #[arg(long, default_value = "all")]
        target: String,
    },
    /// Drop analysis suggestions + embeddings and mark assets for re-analysis (keeps confirmed tags).
    ClearAnalysis,
    /// Compact library.db (VACUUM), reclaiming space freed by deletes.
    Vacuum,
    /// Reset the catalog to empty. Files in sources are never touched. Requires `--confirm`.
    Wipe {
        #[arg(long)]
        confirm: bool,
    },
    /// Factory reset: also erase caches, tokens, flags, and audit. Requires `--confirm`.
    FactoryReset {
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum TokenCmd {
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
pub(crate) enum AccountCmd {
    /// List accounts (username, role, state — never credentials).
    List,
    /// Create an account.
    Add {
        /// Login username (1–64 chars: letters, digits, '-', '_', '.', '@').
        username: String,
        /// Initial password (min 8 chars).
        #[arg(long)]
        password: String,
        /// Permission level: admin | editor | viewer.
        #[arg(long, default_value = "viewer")]
        role: String,
        /// Display name shown in listings.
        #[arg(long = "name")]
        display_name: Option<String>,
    },
    /// Update an account — only the fields you pass change.
    Update {
        /// Account id.
        id: String,
        /// New role: admin | editor | viewer.
        #[arg(long)]
        role: Option<String>,
        /// New display name.
        #[arg(long)]
        name: Option<String>,
        /// Disable (true) or re-enable (false) the account. Disabling signs it out everywhere.
        #[arg(long)]
        disabled: Option<bool>,
        /// New password — replaces the credential and revokes the account's other sessions.
        #[arg(long)]
        password: Option<String>,
    },
    /// Delete an account (its sessions, memberships, and direct shares cascade away).
    Remove {
        /// Account id.
        id: String,
    },
    /// Sign an account out everywhere by revoking all of its sessions.
    RevokeSessions {
        /// Account id.
        id: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum GroupCmd {
    /// List groups with their member counts.
    List,
    /// Create a group.
    Add {
        /// Group name (1–64 chars).
        name: String,
    },
    /// Delete a group (its shares and memberships cascade away).
    Remove {
        /// Group id.
        id: String,
    },
    /// Replace a group's membership with the given account ids (whole-set semantics).
    Members {
        /// Group id.
        id: String,
        /// The full new member set (account ids); pass none to empty the group.
        account_ids: Vec<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ShareCmd {
    /// List shares.
    List,
    /// Grant one account or group access to one source or collection.
    Add {
        /// Share a source by id (exactly one of --source / --collection).
        #[arg(long)]
        source: Option<String>,
        /// Share a collection by id.
        #[arg(long)]
        collection: Option<String>,
        /// Grant to an account by id (exactly one of --account / --group).
        #[arg(long)]
        account: Option<String>,
        /// Grant to a group by id.
        #[arg(long)]
        group: Option<String>,
        /// Grant write access (default: read). Writing also needs Write scope on the identity.
        #[arg(long)]
        write: bool,
    },
    /// Revoke a share by id.
    Remove {
        /// Share id.
        id: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum OidcCmd {
    /// Show the configured provider. Never prints the client secret — only whether one is on file
    /// (the read type has no field for it; tech-spec 10 §5).
    Show,
    /// Set the provider config (a full replace of the single row).
    Set {
        /// Issuer URL — `{issuer}/.well-known/openid-configuration` supplies the rest. Must be
        /// https (http allowed only for localhost).
        #[arg(long)]
        issuer: String,
        /// The OAuth2 client id registered with the provider.
        #[arg(long)]
        client_id: String,
        /// Where the issuer sends the browser back; must match the provider's registration exactly.
        #[arg(long)]
        redirect_url: String,
        /// Client secret (write-only, never readable back). Omit to keep the stored one; pass an
        /// empty string to clear it (a public client).
        #[arg(long)]
        client_secret: Option<String>,
        /// Extra scopes beyond `openid`, which is always requested (repeatable or comma-separated).
        /// `email` and `profile` are the usual additions.
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,
        /// What to do with a verified subject no account is linked to: linked (refuse, the default)
        /// | auto_viewer | auto_editor.
        #[arg(long, default_value = "linked")]
        provisioning: String,
    },
    /// List provider subjects linked to local accounts.
    Identities,
    /// Link a provider subject to an existing account. The issuer comes from the configured
    /// provider, so a link can only name an issuer this instance accepts tokens from.
    Link {
        /// The provider's stable subject claim (`sub`).
        subject: String,
        /// The local account id to link it to (see `admin accounts list`).
        #[arg(long)]
        account_id: String,
    },
    /// Remove a link, revoking the provider's ability to sign in as that account. The account
    /// itself is untouched.
    Unlink {
        /// The provider's subject claim (`sub`), as shown by `admin oidc identities`.
        subject: String,
        /// Which issuer's link to drop. Defaults to the configured issuer, which is what an
        /// ordinary unlink means. Needed only to clear a link left behind by a *previous* issuer:
        /// links are keyed on (issuer, subject), so changing the issuer strands the old ones, and
        /// without naming it they could never be removed. `admin oidc identities` prints it.
        #[arg(long)]
        issuer: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum BlocklistCmd {
    /// List blocked content hashes (removed with `remove --block`), newest first.
    List,
    /// Lift a block by content hash so the content can be re-imported by a later scan.
    Unblock {
        /// The 64-char hex content hash (as shown by `blocklist list`).
        hash: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum SourceCmd {
    /// Add a source: a local path, `sftp://user@host/path`, `smb://host/share/path`, or a
    /// federated 3DAM peer (`3dam://host:7878` / `https://…`) whose catalog merges into queries.
    Add {
        /// Local directory path, an `sftp://` / `smb://` URL, or a `3dam://` / `http(s)://` peer.
        target: String,
        #[arg(long)]
        name: Option<String>,
        /// Auto-rescan on change (local: filesystem events; remote: polling).
        #[arg(long)]
        watch: bool,
        /// Force the source kind: `local`|`sftp`|`smb`|`federated` (default: inferred from the
        /// URL scheme).
        #[arg(long)]
        kind: Option<String>,
        /// Login user (SFTP/SMB); overrides any `user@` in the URL.
        #[arg(long)]
        username: Option<String>,
        /// Password (SFTP/SMB) — or the bearer token for a federated peer.
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
pub(crate) enum CollectionCmd {
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
        /// Smart-folder media filter: `audio`|`image`|`model`|`video`|`document`.
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

/// CLI spelling of `dam_api::dto::SearchMode` (semantic-search M5). Kept local so the clap layer
/// owns its own `ValueEnum` without `dam-api` taking a clap dependency.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum SearchModeArg {
    Lexical,
    Hybrid,
    Semantic,
}

impl From<SearchModeArg> for SearchMode {
    fn from(m: SearchModeArg) -> Self {
        match m {
            SearchModeArg::Lexical => SearchMode::Lexical,
            SearchModeArg::Hybrid => SearchMode::Hybrid,
            SearchModeArg::Semantic => SearchMode::Semantic,
        }
    }
}
