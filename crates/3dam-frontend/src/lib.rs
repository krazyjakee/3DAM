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
    /// Thin client: talk to a remote `3dam serve` over its API. `token` is the bearer credential
    /// presented on every request (tech-spec 10 §1.2) — required when the peer runs in Token mode.
    Connected {
        endpoint: Url,
        token: Option<String>,
    },
}

/// Resolve a `LibraryService` from how the tool was invoked. The front-end code above the trait is
/// identical in both modes — that is the whole point of the seam (DG §1.4).
pub async fn open_backend(b: Backend) -> Result<Box<dyn LibraryService>, LibError> {
    match b {
        Backend::Embedded { data_dir } => {
            let engine = EmbeddedLibrary::open(&data_dir).await?;
            Ok(Box::new(engine))
        }
        Backend::Connected { endpoint, token } => {
            let client = ApiClient::connect_with_token(endpoint, token).await?;
            Ok(Box::new(client))
        }
    }
}

/// The four roles the single binary dispatches to (tech-spec 01 §5).
#[derive(Debug)]
pub enum Role {
    /// Bare `3dam` → desktop shell. Carries any leading global flags (`--connect`/`--token`/
    /// `--data`) so the GUI can launch straight into hosted (connected) mode (issue #70).
    Gui(Vec<OsString>),
    /// `3dam serve …` → long-lived axum service.
    Serve(Vec<OsString>),
    /// `3dam mcp …` → stdio MCP server over an embedded engine.
    Mcp(Vec<OsString>),
    /// `3dam <verb> …` → run-and-exit CLI.
    Cli(Vec<OsString>),
}

/// Peek at `argv` to choose a role before heavy clap parsing. Keeps dispatch rules in one place.
///
/// A bare `3dam` opens the GUI. So does `3dam --connect <url> [--token <t>]` (and `--data <dir>`)
/// with **no** subcommand verb — the desktop shell launches directly against a remote server
/// (issue #70). As soon as a verb (or `--help`/`--version`) appears, it is a CLI invocation and the
/// full grammar takes over.
pub fn classify<I: IntoIterator<Item = OsString>>(args: I) -> Role {
    let mut iter = args.into_iter();
    let _argv0 = iter.next();
    let rest: Vec<OsString> = iter.collect();
    match rest.first().and_then(|s| s.to_str()) {
        None => Role::Gui(Vec::new()),
        Some("serve") => Role::Serve(rest[1..].to_vec()),
        Some("mcp") => Role::Mcp(rest[1..].to_vec()),
        // A leading flag with no subcommand verb is the GUI in connected mode; otherwise the CLI
        // owns it (clap emits the error for an unknown verb / prints help & version).
        Some(tok) if tok.starts_with('-') && !is_cli_invocation(&rest) => Role::Gui(rest),
        Some(_) => Role::Cli(rest),
    }
}

/// Does this leading-flag `argv` actually name a CLI subcommand (or ask for help/version)? Skips the
/// known global flags — `--connect`/`--token`/`--data` each consume a following value — and returns
/// `true` the moment a bare token (a verb) or a help/version request is seen.
fn is_cli_invocation(args: &[OsString]) -> bool {
    /// Global flags that take a separate value token (space-separated form).
    const VALUE_FLAGS: &[&str] = &["--connect", "--token", "--data"];
    /// Requests that only the CLI grammar can satisfy (clap prints them).
    const FORCE_CLI: &[&str] = &["--help", "-h", "--version", "-V"];
    let mut i = 0;
    while i < args.len() {
        // Non-UTF-8 tokens can't be a global flag we recognise — let the CLI surface the error.
        let Some(tok) = args[i].to_str() else {
            return true;
        };
        if FORCE_CLI.contains(&tok) {
            return true;
        }
        if tok.starts_with('-') {
            // `--connect value` consumes the next token; `--connect=value` and bool flags do not.
            if VALUE_FLAGS.contains(&tok) {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            return true; // a bare token is a subcommand verb
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_str(args: &[&str]) -> Role {
        classify(
            std::iter::once("3dam".to_string())
                .chain(args.iter().map(|s| s.to_string()))
                .map(OsString::from),
        )
    }

    #[test]
    fn bare_launches_gui() {
        assert!(matches!(classify_str(&[]), Role::Gui(a) if a.is_empty()));
    }

    #[test]
    fn connect_flags_only_launch_gui() {
        let cases: &[&[&str]] = &[
            &["--connect", "http://host:7878"],
            &["--connect", "http://host:7878", "--token", "abc"],
            &["--connect=http://host:7878"],
            &["--data", "/tmp/lib"],
        ];
        for c in cases {
            assert!(
                matches!(classify_str(c), Role::Gui(_)),
                "expected GUI for {c:?}"
            );
        }
    }

    #[test]
    fn verb_after_flags_is_cli() {
        assert!(matches!(
            classify_str(&["--connect", "http://host:7878", "search", "brick"]),
            Role::Cli(_)
        ));
        assert!(matches!(classify_str(&["search", "brick"]), Role::Cli(_)));
    }

    #[test]
    fn help_and_version_are_cli() {
        assert!(matches!(classify_str(&["--help"]), Role::Cli(_)));
        assert!(matches!(classify_str(&["--version"]), Role::Cli(_)));
    }

    #[test]
    fn serve_and_mcp_unaffected() {
        assert!(matches!(classify_str(&["serve"]), Role::Serve(_)));
        assert!(matches!(classify_str(&["mcp"]), Role::Mcp(_)));
    }
}
