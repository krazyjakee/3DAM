//! `dam-cli` — the CLI front-end (tech-spec 13). Rides the same `LibraryService` seam as every
//! other front-end via `open_backend`, so `--connect` transparently swaps the embedded engine for
//! a remote server. Also owns the `serve`/`mcp` entry points (they dispatch into `3dam-server`).

use clap::{Args, Parser, Subcommand, ValueEnum};
use dam_api::admin::{
    AdminStatus, AuditEntry, AuthMode, CacheTarget, ClearAnalysisReport, ClearCacheReport,
    FactoryResetReport, FlagInfo, FlagKey, FlagValue, McpMode, NewToken, NewTokenReply, SetFlag,
    SetFlagReply, StorageUsage, TokenInfo, VacuumReport, WipeReport,
};
use dam_api::dto::*;
use dam_api::id::{AssetId, CollectionId, JobId, SourceId};
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService, Scope, Scopes};
use dam_api::LibError;
use dam_frontend::{default_data_dir, open_backend, Backend};
use dam_server::ServeConfig;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use url::Url;
mod admin;
mod args;
mod dispatch;
mod support;

use args::*;
use dispatch::dispatch;
use support::{init_tracing, parse_endpoint};

/// Entry point for the CLI role (run-and-exit verbs).
pub async fn run(args: Vec<OsString>) -> ExitCode {
    init_tracing();
    let cli = match Cli::try_parse_from(std::iter::once(OsString::from("3dam")).chain(args)) {
        Ok(c) => c,
        Err(e) => {
            // clap prints help/usage itself; propagate its exit code sense.
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match dispatch(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            // A bare "unauthorized" from a `--connect`ed server tells the user nothing actionable.
            // Point them at the fix: the server is in token mode and wants a credential.
            if matches!(e.downcast_ref::<LibError>(), Some(LibError::Unauthorized)) {
                eprintln!(
                    "hint: this server requires a token — pass --token <secret> \
                     (an operator mints one with `3dam admin token add`)."
                );
            }
            ExitCode::FAILURE
        }
    }
}

#[derive(Parser)]
#[command(name = "serve")]
struct ServeArgs {
    /// Address to bind (overrides the config file). Default: 127.0.0.1:7878.
    #[arg(long)]
    addr: Option<String>,
    /// Library data directory.
    #[arg(long)]
    data: Option<PathBuf>,
    /// Serve config file (TOML) — seeds flags and may set the bind (tech-spec 09 §A.1).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Allow binding beyond localhost without TLS (refused by default — ADR 0009 §4).
    #[arg(long)]
    insecure: bool,
    /// PEM certificate chain for in-process TLS (issue #75). With `--tls-key`, serve HTTPS — and a
    /// non-localhost bind then needs no `--insecure`.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// PEM private key paired with `--tls-cert`.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
}

/// Entry point for the `serve` role. Dispatches into `3dam-server`.
pub async fn serve(args: Vec<OsString>) -> ExitCode {
    init_tracing();
    let parsed =
        match ServeArgs::try_parse_from(std::iter::once(OsString::from("serve")).chain(args)) {
            Ok(a) => a,
            Err(e) => {
                let _ = e.print();
                return ExitCode::from(2);
            }
        };
    let addr: Option<SocketAddr> = match parsed.addr {
        Some(s) => match s.parse() {
            Ok(a) => Some(a),
            Err(_) => {
                eprintln!("error: invalid --addr '{s}'");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let cfg = ServeConfig {
        addr,
        data_dir: parsed.data.unwrap_or_else(default_data_dir),
        config: parsed.config,
        insecure: parsed.insecure,
        tls_cert: parsed.tls_cert,
        tls_key: parsed.tls_key,
    };
    match dam_server::serve(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("serve error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Parser)]
#[command(name = "mcp")]
struct McpArgs {
    #[arg(long)]
    data: Option<PathBuf>,
}

/// Entry point for the `mcp` stdio role. Dispatches into `dam_server::mcp_stdio`, the hand-rolled
/// stdio MCP server (`crates/3dam-server/src/mcp.rs`).
pub async fn mcp(args: Vec<OsString>) -> ExitCode {
    let parsed = match McpArgs::try_parse_from(std::iter::once(OsString::from("mcp")).chain(args)) {
        Ok(a) => a,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(2);
        }
    };
    match dam_server::mcp_stdio(parsed.data.unwrap_or_else(default_data_dir)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mcp error: {e}");
            ExitCode::FAILURE
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

async fn open(global: &Global) -> anyhow::Result<Box<dyn LibraryService>> {
    let backend = match &global.connect {
        Some(c) => Backend::Connected {
            endpoint: parse_endpoint(c)?,
            token: global.token.clone(),
        },
        None => Backend::Embedded {
            data_dir: global.data.clone().unwrap_or_else(default_data_dir),
        },
    };
    Ok(open_backend(backend).await?)
}
