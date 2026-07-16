//! `dam-desktop` — the native desktop shell (tech-spec 12, Tauri per ADR 0013, superseding the
//! egui client of ADR 0005).
//!
//! One UI codebase, two shells: the browser and this window. In embedded mode the shell boots the
//! exact server `3dam serve` runs — loopback only, ephemeral port, embedded React client, full
//! `/api/v1` + WebSocket — on a background thread, then opens a webview on it. In hosted mode
//! (`--connect`, issue #70) the webview navigates straight to the remote server. Either way the UI
//! is the web client; web ↔ native parity is structural, not maintained by hand.
//!
//! Auth rides the front-door design unchanged (tech-spec 10): with the `Authentication` flag Off,
//! the local anonymous caller already resolves to owner trust; when it is on, the shell mints a
//! per-launch owner token in-process and seeds it into the web client's own credential store
//! (`localStorage["3dam.server"]`, see `web/src/lib/server.ts`) via a webview init script.

use std::ffi::OsString;
use std::path::PathBuf;

use dam_frontend::default_data_dir;
use tauri::{WebviewUrl, WebviewWindowBuilder};

/// Entry point for the GUI role. Bare `3dam` serves the platform-default embedded library to the
/// webview; `--connect <url> [--token <t>]` opens straight against a remote `3dam serve`, and
/// `--data <dir>` overrides the embedded location. Returns a process exit code.
pub fn run(args: Vec<OsString>) -> u8 {
    let launch = match LaunchArgs::parse(&args) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("3dam desktop: {e}");
            return 2;
        }
    };

    // Resolve what the webview loads: a remote server, or an in-process one over the local library.
    let (url, token) = match &launch.connect {
        Some(raw) => match url::Url::parse(raw) {
            Ok(endpoint) => (endpoint, launch.token.clone()),
            Err(e) => {
                eprintln!("3dam desktop: invalid --connect URL '{raw}': {e}");
                return 2;
            }
        },
        None => {
            let data_dir = launch.data.clone().unwrap_or_else(default_data_dir);
            match start_embedded_server(data_dir) {
                Ok(started) => started,
                Err(e) => {
                    eprintln!("3dam desktop: failed to start the local server: {e}");
                    return 1;
                }
            }
        }
    };

    // Seed the credential where the web client already looks (web/src/lib/server.ts): base "" keeps
    // it same-origin; the script runs before any app JS on every navigation, so the client boots
    // signed-in instead of landing on the AuthGate. `to_string` JSON-escapes the token for us.
    let init_script = token.as_deref().map(|t| {
        format!(
            "localStorage.setItem('3dam.server', JSON.stringify({{ base: '', token: {} }}));",
            serde_json::to_string(t).expect("a string always serializes")
        )
    });

    let outcome = tauri::Builder::default()
        .setup(move |app| {
            let mut win = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("3DAM")
                .inner_size(1200.0, 760.0)
                .min_inner_size(720.0, 480.0);
            if let Some(script) = &init_script {
                win = win.initialization_script(script);
            }
            win.build()?;
            Ok(())
        })
        .run(tauri::generate_context!());
    match outcome {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("3dam desktop: {e}");
            1
        }
    }
}

/// Boot the in-process server on a background thread — its own Tokio runtime, since the main thread
/// belongs to the webview event loop — and block until it reports its bound address (or fails).
/// The thread is detached: it serves until the process exits with the window.
fn start_embedded_server(data_dir: PathBuf) -> anyhow::Result<(url::Url, Option<String>)> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("3dam-server".into())
        .spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(anyhow::anyhow!("failed to start async runtime: {e}")));
                    return;
                }
            };
            rt.block_on(dam_server::serve_desktop(data_dir, tx));
        })?;
    let info = rx.blocking_recv()??;
    Ok((url::Url::parse(&info.url)?, info.token))
}

/// The launch flags the desktop shell understands — a hand parser (kept off clap so the shell needn't
/// link the CLI grammar). Mirrors the CLI global flags relevant to a front-end.
#[derive(Default)]
struct LaunchArgs {
    connect: Option<String>,
    token: Option<String>,
    data: Option<PathBuf>,
}

impl LaunchArgs {
    fn parse(args: &[OsString]) -> Result<Self, String> {
        let mut out = LaunchArgs::default();
        let mut i = 0;
        while i < args.len() {
            let tok = args[i]
                .to_str()
                .ok_or_else(|| "non-UTF-8 argument".to_string())?;
            // Support both `--flag value` and `--flag=value`.
            let (flag, inline) = match tok.split_once('=') {
                Some((f, v)) => (f, Some(v.to_string())),
                None => (tok, None),
            };
            let mut take_value = |name: &str| -> Result<String, String> {
                if let Some(v) = &inline {
                    Ok(v.clone())
                } else {
                    i += 1;
                    args.get(i)
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                        .ok_or_else(|| format!("{name} requires a value"))
                }
            };
            match flag {
                "--connect" => out.connect = Some(take_value("--connect")?),
                "--token" => out.token = Some(take_value("--token")?),
                "--data" => out.data = Some(PathBuf::from(take_value("--data")?)),
                other => return Err(format!("unknown option '{other}'")),
            }
            i += 1;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<LaunchArgs, String> {
        LaunchArgs::parse(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_connect_token_data() {
        let l = parse(&["--connect", "http://h:7878", "--token=abc", "--data", "/x"]).unwrap();
        assert_eq!(l.connect.as_deref(), Some("http://h:7878"));
        assert_eq!(l.token.as_deref(), Some("abc"));
        assert_eq!(l.data.as_deref(), Some(std::path::Path::new("/x")));
    }

    #[test]
    fn rejects_unknown_flags_and_missing_values() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--connect"]).is_err());
    }
}
