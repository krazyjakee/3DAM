//! `dam-frontend` — the small helper both front-ends share (tech-spec 01 §4–§5), so the GUI can
//! reach the backend constructor without linking the clap grammar.
//!
//! Two orthogonal decisions live here:
//! - **Role dispatch** ([`classify`]): which front-end a given `argv` selects (GUI / CLI / serve / mcp).
//! - **Backend selection** ([`open_backend`]): embedded in-process engine vs a connected API client.

use dam_api::service::LibraryService;
use dam_api::LibError;
use dam_client::ApiClient;
use dam_core::EmbeddedLibrary;
use std::ffi::OsString;
use std::path::PathBuf;
use url::Url;

/// The platform-default library location (tech-spec 02 §9), re-exported so front-ends need not
/// link `3dam-core` just for the path helper.
pub use dam_core::default_data_dir;

/// Which `LibraryService` a front-end will hold.
pub enum Backend {
    /// Standalone: the engine is linked in-process. No network.
    Embedded { data_dir: PathBuf },
    /// Thin client: talk to a remote `3dam serve` over its API.
    Connected { endpoint: Url },
}

/// Resolve a `LibraryService` from how the tool was invoked. The front-end code above the trait is
/// identical in both modes — that is the whole point of the seam (DG §1.4).
pub async fn open_backend(b: Backend) -> Result<Box<dyn LibraryService>, LibError> {
    match b {
        Backend::Embedded { data_dir } => {
            let engine = EmbeddedLibrary::open(&data_dir).await?;
            Ok(Box::new(engine))
        }
        Backend::Connected { endpoint } => {
            let client = ApiClient::connect(endpoint).await?;
            Ok(Box::new(client))
        }
    }
}

/// The four roles the single binary dispatches to (tech-spec 01 §5).
#[derive(Debug)]
pub enum Role {
    /// Bare `3dam` → desktop shell.
    Gui,
    /// `3dam serve …` → long-lived axum service.
    Serve(Vec<OsString>),
    /// `3dam mcp …` → stdio MCP server over an embedded engine.
    Mcp(Vec<OsString>),
    /// `3dam <verb> …` → run-and-exit CLI.
    Cli(Vec<OsString>),
}

/// Peek at `argv` to choose a role before heavy clap parsing. Keeps dispatch rules in one place.
pub fn classify<I: IntoIterator<Item = OsString>>(args: I) -> Role {
    let mut iter = args.into_iter();
    let _argv0 = iter.next();
    let rest: Vec<OsString> = iter.collect();
    match rest.first().and_then(|s| s.to_str()) {
        None => Role::Gui,
        Some("serve") => Role::Serve(rest[1..].to_vec()),
        Some("mcp") => Role::Mcp(rest[1..].to_vec()),
        // Every other leading token is handled by the CLI (clap emits the error for an unknown verb).
        Some(_) => Role::Cli(rest),
    }
}
