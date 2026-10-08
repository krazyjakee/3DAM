//! Native-only updater authority. The page can request actions, but cannot supply download URLs,
//! signatures, keys, or arbitrary versions. The Tauri updater verifies signatures before install.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use dam_api::updates::{DesktopRelease, DesktopUpdateStatus, UpdatePhase};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const UPDATE_ENDPOINT: &str = match option_env!("DAM_UPDATER_ENDPOINT") {
    Some(endpoint) => endpoint,
    None => "https://github.com/krazyjakee/3DAM/releases/latest/download/latest.json",
};

struct Inner {
    status: DesktopUpdateStatus,
    pending: Option<Update>,
}

pub struct DesktopUpdater {
    embedded: bool,
    embedded_origin: String,
    preferences_path: PathBuf,
    inner: Mutex<Inner>,
}

#[derive(Serialize, Deserialize)]
struct Preferences {
    automatic_checks: bool,
}

fn unavailable_reason(
    embedded: bool,
    key: &str,
    bundle: Option<tauri::utils::config::BundleType>,
) -> Option<String> {
    use tauri::utils::config::BundleType;
    if !embedded {
        return Some("Open 3DAM with its local library to update the desktop app.".into());
    }
    if !matches!(
        bundle,
        Some(BundleType::AppImage | BundleType::App | BundleType::Msi | BundleType::Nsis)
    ) {
        return Some("This installation is managed outside the app. Download a new release or use your package manager. Linux in-app updates require AppImage.".into());
    }
    if key.trim().is_empty() {
        return Some(
            "Signed updates are not configured in this build. Download a new release to update."
                .into(),
        );
    }
    None
}

pub fn setup(app: &AppHandle, embedded: bool, url: &url::Url) -> anyhow::Result<()> {
    let path = app.path().app_config_dir()?.join("updates.json");
    let (automatic_checks, preferences_error) = match load_preferences(&path) {
        Ok(enabled) => (enabled, None),
        Err(error) => (false, Some(format!("Could not read update preferences: {error}. Automatic checks are disabled until you save the setting again."))),
    };
    let reason = unavailable_reason(
        embedded,
        option_env!("DAM_UPDATER_PUBLIC_KEY")
            .filter(|key| !key.trim().is_empty())
            .unwrap_or_else(|| {
                app.config()
                    .plugins
                    .0
                    .get("updater")
                    .and_then(|config| config.get("pubkey"))
                    .and_then(|key| key.as_str())
                    .unwrap_or_default()
            }),
        tauri::utils::platform::bundle_type(),
    );
    let state = Arc::new(DesktopUpdater {
        embedded,
        embedded_origin: url.origin().ascii_serialization(),
        preferences_path: path,
        inner: Mutex::new(Inner {
            status: DesktopUpdateStatus {
                current_version: env!("CARGO_PKG_VERSION").into(),
                supported: reason.is_none(),
                unavailable_reason: reason,
                automatic_checks,
                phase: if preferences_error.is_some() {
                    UpdatePhase::Error
                } else {
                    UpdatePhase::Idle
                },
                release: None,
                checked_at: None,
                downloaded_bytes: 0,
                total_bytes: None,
                error: preferences_error,
            },
            pending: None,
        }),
    });
    app.manage(state.clone());
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let should_check = state.lock().is_ok_and(|inner| {
                inner.status.supported
                    && inner.status.automatic_checks
                    && !inner.status.phase.busy()
                    && inner.status.phase != UpdatePhase::Ready
            });
            if should_check && state.begin_check().is_ok() {
                check(handle.clone(), state.clone()).await;
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
    Ok(())
}

fn load_preferences(path: &Path) -> anyhow::Result<bool> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<Preferences>(&bytes)?.automatic_checks),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn save_preferences(path: &Path, automatic_checks: bool) -> anyhow::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid preferences path"))?;
    std::fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&serde_json::to_vec(&Preferences { automatic_checks })?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

impl DesktopUpdater {
    fn lock(&self) -> Result<MutexGuard<'_, Inner>, String> {
        self.inner
            .lock()
            .map_err(|_| "Updater state is unavailable".into())
    }

    fn authorize(&self) -> Result<(), String> {
        if self.embedded {
            Ok(())
        } else {
            Err("Updater access requires the embedded desktop UI".into())
        }
    }

    fn authorize_window(&self, window: &tauri::WebviewWindow) -> Result<(), String> {
        self.authorize()?;
        let url = window.url().map_err(|error| error.to_string())?;
        if trusted_window(window.label(), &url, &self.embedded_origin) {
            Ok(())
        } else {
            Err("Updater access requires the original embedded desktop origin".into())
        }
    }

    fn begin_check(&self) -> Result<DesktopUpdateStatus, String> {
        self.authorize()?;
        let mut inner = self.lock()?;
        ensure_action(&inner.status)?;
        inner.status.phase = UpdatePhase::Checking;
        inner.status.error = None;
        inner.status.downloaded_bytes = 0;
        inner.status.total_bytes = None;
        // A failed check must not leave an obsolete release installable.
        inner.pending = None;
        inner.status.release = None;
        Ok(inner.status.clone())
    }

    fn fail(&self, error: String) {
        if let Ok(mut inner) = self.lock() {
            inner.status.phase = UpdatePhase::Error;
            inner.status.error = Some(error);
        }
    }
}

fn trusted_window(label: &str, url: &url::Url, origin: &str) -> bool {
    label == "main" && url.origin().ascii_serialization() == origin
}

fn ensure_action(status: &DesktopUpdateStatus) -> Result<(), String> {
    if !status.supported {
        return Err(status.unavailable_reason.clone().unwrap_or_default());
    }
    if status.phase.busy() || status.phase == UpdatePhase::Ready {
        return Err(
            "An update operation is already in progress; restart after installation".into(),
        );
    }
    Ok(())
}

async fn check(app: AppHandle, state: Arc<DesktopUpdater>) {
    let outcome = async {
        let endpoint = UPDATE_ENDPOINT.parse().map_err(|error| {
            tauri_plugin_updater::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid update endpoint: {error}"),
            ))
        })?;
        app.updater_builder()
            .endpoints(vec![endpoint])?
            .timeout(Duration::from_secs(30))
            .build()?
            .check()
            .await
    }
    .await;
    match outcome {
        Ok(update) => {
            if let Ok(mut inner) = state.lock() {
                inner.status.checked_at = Some(dam_api::now_ms());
                inner.status.release = update.as_ref().map(|update| DesktopRelease {
                    version: update.version.clone(),
                    notes: update.body.clone(),
                    date: update.date.map(|date| date.to_string()),
                });
                inner.status.phase = if update.is_some() {
                    UpdatePhase::Available
                } else {
                    UpdatePhase::UpToDate
                };
                inner.pending = update;
            }
        }
        Err(error) => state.fail(format!("Could not check for updates: {error}")),
    }
}

#[tauri::command]
pub fn desktop_update_status(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<DesktopUpdater>>,
) -> Result<DesktopUpdateStatus, String> {
    state.authorize_window(&window)?;
    Ok(state.lock()?.status.clone())
}

#[tauri::command]
pub fn desktop_update_check(
    window: tauri::WebviewWindow,
    app: AppHandle,
    state: tauri::State<'_, Arc<DesktopUpdater>>,
) -> Result<DesktopUpdateStatus, String> {
    state.authorize_window(&window)?;
    let status = state.begin_check()?;
    let state = state.inner().clone();
    tauri::async_runtime::spawn(check(app, state));
    Ok(status)
}

#[tauri::command]
pub fn desktop_update_install(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, Arc<DesktopUpdater>>,
) -> Result<DesktopUpdateStatus, String> {
    state.authorize_window(&window)?;
    let mut inner = state.lock()?;
    ensure_action(&inner.status)?;
    let mut update = inner
        .pending
        .take()
        .ok_or("Check for an available update first")?;
    update.timeout = Some(Duration::from_secs(10 * 60));
    inner.status.phase = UpdatePhase::Downloading;
    inner.status.downloaded_bytes = 0;
    inner.status.total_bytes = None;
    inner.status.error = None;
    let status = inner.status.clone();
    drop(inner);
    let state = state.inner().clone();
    tauri::async_runtime::spawn(async move {
        // Download verifies the signature before returning bytes. Installation is synchronous,
        // so run it on a blocking worker to keep the UI and embedded server responsive.
        let bytes = update
            .download(
                |chunk, total| {
                    if let Ok(mut inner) = state.lock() {
                        inner.status.downloaded_bytes += chunk as u64;
                        inner.status.total_bytes = total;
                    }
                },
                || {},
            )
            .await;
        match bytes {
            Ok(bytes) => {
                if let Ok(mut inner) = state.lock() {
                    inner.status.phase = UpdatePhase::Installing;
                }
                let result = tauri::async_runtime::spawn_blocking(move || {
                    let result = update.install(bytes);
                    (update, result)
                })
                .await;
                match result {
                    Ok((_, Ok(()))) => {
                        if let Ok(mut inner) = state.lock() {
                            inner.status.phase = UpdatePhase::Ready;
                        }
                    }
                    Ok((update, Err(error))) => {
                        if let Ok(mut inner) = state.lock() {
                            inner.pending = Some(update);
                        }
                        state.fail(format!("Could not install the update: {error}"));
                    }
                    Err(error) => state.fail(format!("Installer failed: {error}")),
                }
            }
            Err(error) => {
                if let Ok(mut inner) = state.lock() {
                    inner.pending = Some(update);
                }
                state.fail(format!("Could not download or verify the update: {error}"));
            }
        }
    });
    Ok(status)
}

#[tauri::command]
pub fn desktop_update_preferences(
    window: tauri::WebviewWindow,
    automatic_checks: bool,
    state: tauri::State<'_, Arc<DesktopUpdater>>,
) -> Result<DesktopUpdateStatus, String> {
    state.authorize_window(&window)?;
    let mut inner = state.lock()?;
    save_preferences(&state.preferences_path, automatic_checks)
        .map_err(|error| error.to_string())?;
    inner.status.automatic_checks = automatic_checks;
    if inner
        .status
        .error
        .as_deref()
        .is_some_and(|error| error.starts_with("Could not read update preferences:"))
    {
        inner.status.error = None;
        inner.status.phase = UpdatePhase::Idle;
    }
    Ok(inner.status.clone())
}

#[tauri::command]
pub fn desktop_update_restart(
    window: tauri::WebviewWindow,
    app: AppHandle,
    state: tauri::State<'_, Arc<DesktopUpdater>>,
) -> Result<(), String> {
    state.authorize_window(&window)?;
    if state.lock()?.status.phase != UpdatePhase::Ready {
        return Err("Install an update before restarting".into());
    }
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::utils::config::BundleType;

    #[test]
    fn hosted_pages_and_unmanaged_packages_cannot_update() {
        assert!(unavailable_reason(false, "key", Some(BundleType::AppImage)).is_some());
        assert!(unavailable_reason(true, "key", Some(BundleType::Deb)).is_some());
        assert!(unavailable_reason(true, "key", None).is_some());
        assert!(unavailable_reason(true, "", Some(BundleType::AppImage)).is_some());
        for bundle in [
            BundleType::AppImage,
            BundleType::App,
            BundleType::Msi,
            BundleType::Nsis,
        ] {
            assert!(unavailable_reason(true, "key", Some(bundle)).is_none());
        }
    }

    #[test]
    fn automatic_check_preference_survives_replacement_and_defaults_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/updates.json");
        assert!(load_preferences(&path).unwrap());
        save_preferences(&path, false).unwrap();
        assert!(!load_preferences(&path).unwrap());
        save_preferences(&path, true).unwrap();
        assert!(load_preferences(&path).unwrap());
    }

    #[test]
    fn checks_clear_stale_releases_and_reject_concurrent_operations() {
        let state = DesktopUpdater {
            embedded: true,
            embedded_origin: "http://127.0.0.1:1234".into(),
            preferences_path: PathBuf::new(),
            inner: Mutex::new(Inner {
                pending: None,
                status: DesktopUpdateStatus {
                    current_version: "0.1.5".into(),
                    supported: true,
                    unavailable_reason: None,
                    automatic_checks: true,
                    phase: UpdatePhase::Available,
                    release: Some(DesktopRelease {
                        version: "0.2.0".into(),
                        notes: None,
                        date: None,
                    }),
                    checked_at: None,
                    downloaded_bytes: 0,
                    total_bytes: None,
                    error: None,
                },
            }),
        };
        assert!(state.begin_check().unwrap().release.is_none());
        assert!(state.begin_check().is_err());
        for phase in [
            UpdatePhase::Downloading,
            UpdatePhase::Installing,
            UpdatePhase::Ready,
        ] {
            state.lock().unwrap().status.phase = phase;
            assert!(state.begin_check().is_err());
        }
        state.fail("Offline".into());
        assert_eq!(state.begin_check().unwrap().phase, UpdatePhase::Checking);
    }

    #[test]
    fn navigation_to_another_loopback_server_loses_updater_access() {
        let origin = "http://127.0.0.1:1234";
        assert!(trusted_window(
            "main",
            &url::Url::parse("http://127.0.0.1:1234/updates").unwrap(),
            origin
        ));
        for endpoint in [
            "http://127.0.0.1:5678/updates",
            "http://localhost:1234/updates",
            "https://example.com/updates",
        ] {
            assert!(!trusted_window(
                "main",
                &url::Url::parse(endpoint).unwrap(),
                origin
            ));
        }
        assert!(!trusted_window(
            "other",
            &url::Url::parse(origin).unwrap(),
            origin
        ));
    }
}
