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
//! per-launch owner token in-process. Hosted tokens live in the OS keychain; a pre-page fetch
//! wrapper attaches native credentials only to the selected server without exposing them to page
//! JavaScript (issues #128/#129).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;

use dam_frontend::default_data_dir;
use tauri::menu::{AboutMetadata, Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_window_state::StateFlags;

/// Webview zoom bounds and step for the View menu — browser conventions (25%–300%, 10% steps).
/// The factor is shell-side state: `set_zoom` is write-only, so the menu handler tracks it.
const ZOOM_MIN: f64 = 0.25;
const ZOOM_MAX: f64 = 3.0;
const ZOOM_STEP: f64 = 0.1;
const KEYCHAIN_SERVICE: &str = "io.github.krazyjakee.threedam.connected-server";

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
    let hosted = launch.connect.is_some();
    let (url, mut token) = match &launch.connect {
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

    let keychain_account = hosted.then(|| credential_account(&url));
    if let Some(account) = keychain_account.as_deref() {
        let entry = match keyring::Entry::new(KEYCHAIN_SERVICE, account) {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("3dam desktop: OS credential store is unavailable: {error}");
                return 1;
            }
        };
        if let Some(supplied) = token.as_deref() {
            if let Err(error) = entry.set_password(supplied) {
                eprintln!("3dam desktop: could not save the server credential: {error}");
                return 1;
            }
        } else {
            token = match entry.get_password() {
                Ok(saved) => Some(saved),
                Err(keyring::Error::NoEntry) => None,
                Err(error) => {
                    eprintln!("3dam desktop: could not read the server credential: {error}");
                    return 1;
                }
            };
        }
    }

    // Runs before page scripts. The secret remains captured in native-installed plumbing and is
    // attached only to this server's origin+mount; the app sees only a non-secret capability bit.
    let mut init_script = shell_mode_script(hosted);
    if let Some(secret) = token.as_deref() {
        init_script.push_str(&native_credential_script(&url, secret, hosted));
    }

    // The webview zoom factor lives in the menu-event closure: `set_zoom` is write-only, so the
    // shell is the source of truth for Zoom In/Out stepping.
    let zoom = Mutex::new(1.0_f64);

    let outcome = tauri::Builder::default()
        // Native path dialogs and completed-output opening. The webview loads a remote
        // `http://127.0.0.1:<port>` URL, so IPC is off by default; the capability in
        // `capabilities/loopback-dialog.json` grants only open/save dialogs and path opening to the
        // loopback origin — a `--connect` remote server's page never gets IPC.
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // Persist exactly what the window-geometry contract promises — size, position, maximized —
        // and nothing surprising (no VISIBLE, so a crash while hidden can't persist a ghost window;
        // no FULLSCREEN, so F11 is per-session). Restore is automatic: the plugin's
        // `on_window_ready` hook fires for the `.setup()`-built window as well.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED)
                .build(),
        )
        .menu(app_menu)
        .on_menu_event(move |app, event| {
            handle_menu_event(app, event.id().as_ref(), &zoom, keychain_account.as_deref())
        })
        .setup(move |app| {
            let mut win = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("3DAM")
                // A fresh renderer partition means a service worker from an earlier compromised
                // page can never sit below the initialization wrapper and observe Authorization.
                .incognito(true)
                .inner_size(1200.0, 760.0)
                .min_inner_size(720.0, 480.0)
                // Let OS file drops reach the *page* (issue #80). Tauri enables this on every
                // window by default, and while enabled it intercepts drops at the native layer and
                // the webview never fires an HTML5 `drop` event — so the shared Upload drop zone
                // would work in a browser and do nothing here, silently and only in the desktop
                // build. We deliberately do not want the native drag-drop events either: golden
                // rule 1 says the web client *is* the UI, so the drop zone is web code and this
                // shell's job is only to stop swallowing its events.
                .disable_drag_drop_handler();
            win = win.initialization_script(&init_script);
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

/// The native app menu — small and honest: only entries the shell can actually service. Quit and
/// the View items are custom ids handled in [`handle_menu_event`]; About is the predefined item
/// (native About dialog on every platform, no dialog plugin needed). The layout is the
/// Windows/Linux convention; a macOS app-menu arrangement can come with the bundle work.
fn app_menu<R: Runtime>(handle: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let file = SubmenuBuilder::new(handle, "File")
        .item(
            &MenuItemBuilder::with_id("forget-server-credential", "Forget Server Credential")
                .build(handle)?,
        )
        .separator()
        .item(
            &MenuItemBuilder::with_id("quit", "Quit")
                .accelerator("CmdOrCtrl+Q")
                .build(handle)?,
        )
        .build()?;
    let view = SubmenuBuilder::new(handle, "View")
        .item(
            &MenuItemBuilder::with_id("reload", "Reload")
                .accelerator("CmdOrCtrl+R")
                .build(handle)?,
        )
        .separator()
        .item(
            &MenuItemBuilder::with_id("zoom-reset", "Actual Size")
                .accelerator("CmdOrCtrl+0")
                .build(handle)?,
        )
        .item(
            // "=" is the unshifted key under "+" — the accelerator browsers actually bind.
            &MenuItemBuilder::with_id("zoom-in", "Zoom In")
                .accelerator("CmdOrCtrl+=")
                .build(handle)?,
        )
        .item(
            &MenuItemBuilder::with_id("zoom-out", "Zoom Out")
                .accelerator("CmdOrCtrl+-")
                .build(handle)?,
        )
        .separator()
        .item(
            // Custom rather than `PredefinedMenuItem::fullscreen` (that one is macOS-only).
            &MenuItemBuilder::with_id("fullscreen", "Toggle Fullscreen")
                .accelerator("F11")
                .build(handle)?,
        );
    // The Web Inspector only exists in debug builds (`open_devtools` is compiled out of release
    // unless the `devtools` feature is enabled), so the menu item follows it.
    #[cfg(debug_assertions)]
    let view = view.separator().item(
        &MenuItemBuilder::with_id("devtools", "Toggle DevTools")
            .accelerator("CmdOrCtrl+Shift+I")
            .build(handle)?,
    );
    let view = view.build()?;
    let help = SubmenuBuilder::new(handle, "Help")
        .about_with_text(
            "About 3DAM",
            Some(AboutMetadata {
                name: Some("3DAM".into()),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                ..Default::default()
            }),
        )
        .build()?;
    MenuBuilder::new(handle)
        .items(&[&file, &view, &help])
        .build()
}

/// Service a menu click. Fail-soft throughout (golden rule 6): a webview call that errors — or a
/// window that has already gone away — drops the click rather than crashing the shell.
fn handle_menu_event<R: Runtime>(
    app: &AppHandle<R>,
    id: &str,
    zoom: &Mutex<f64>,
    keychain_account: Option<&str>,
) {
    if id == "quit" {
        app.exit(0);
        return;
    }
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    match id {
        "forget-server-credential" => {
            let Some(account) = keychain_account else {
                return;
            };
            match keyring::Entry::new(KEYCHAIN_SERVICE, account)
                .and_then(|entry| entry.delete_credential())
            {
                Ok(()) | Err(keyring::Error::NoEntry) => {
                    // The current page's closure is deliberately immutable; exit ensures the next
                    // launch starts without it and gives an honest, complete forget operation.
                    app.exit(0);
                }
                Err(error) => eprintln!("3dam desktop: could not forget the credential: {error}"),
            }
        }
        "reload" => {
            let _ = win.reload();
        }
        "zoom-reset" | "zoom-in" | "zoom-out" => {
            let Ok(mut factor) = zoom.lock() else {
                return;
            };
            let next = match id {
                "zoom-in" => (*factor + ZOOM_STEP).min(ZOOM_MAX),
                "zoom-out" => (*factor - ZOOM_STEP).max(ZOOM_MIN),
                _ => 1.0,
            };
            if win.set_zoom(next).is_ok() {
                *factor = next;
            }
        }
        "fullscreen" => {
            let now = win.is_fullscreen().unwrap_or(false);
            let _ = win.set_fullscreen(!now);
        }
        #[cfg(debug_assertions)]
        "devtools" => {
            if win.is_devtools_open() {
                win.close_devtools();
            } else {
                win.open_devtools();
            }
        }
        _ => {}
    }
}

fn credential_account(url: &url::Url) -> String {
    // Origin + reverse-proxy mount keep credentials for distinct instances separate. Userinfo,
    // query, and fragment never become secret-store identifiers.
    format!(
        "{}://{}{}{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default(),
        url.path().trim_end_matches('/'),
    )
}

fn native_credential_script(url: &url::Url, secret: &str, forgettable: bool) -> String {
    let origin = url.origin().ascii_serialization();
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let base = path.trim_end_matches('/');
    include_str!("native-credential.js")
        .replace(
            "__3DAM_CREDENTIAL_JSON__",
            &serde_json::to_string(secret).expect("string JSON"),
        )
        .replace(
            "__3DAM_ORIGIN_JSON__",
            &serde_json::to_string(&origin).expect("string JSON"),
        )
        .replace(
            "__3DAM_PATH_JSON__",
            &serde_json::to_string(path).expect("string JSON"),
        )
        .replace(
            "__3DAM_BASE_JSON__",
            &serde_json::to_string(base).expect("string JSON"),
        )
        .replace(
            "__3DAM_FORGETTABLE_JSON__",
            if forgettable { "true" } else { "false" },
        )
}

fn shell_mode_script(hosted: bool) -> String {
    format!(
        "Object.defineProperty(window,'__3DAM_EMBEDDED_SERVER__',{{value:{},writable:false,configurable:false}});",
        !hosted
    )
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

    #[test]
    fn launch_mode_distinguishes_embedded_from_connect_even_at_a_remote_root() {
        assert!(shell_mode_script(false).contains("value:true"));
        assert!(shell_mode_script(true).contains("value:false"));
        assert!(shell_mode_script(true).contains("writable:false"));
    }
}
