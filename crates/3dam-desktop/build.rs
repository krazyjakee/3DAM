fn main() {
    println!("cargo:rerun-if-env-changed=DAM_UPDATER_PUBLIC_KEY");
    println!("cargo:rerun-if-env-changed=DAM_UPDATER_ENDPOINT");
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "desktop_update_status",
            "desktop_update_check",
            "desktop_update_install",
            "desktop_update_preferences",
            "desktop_update_restart",
        ]),
    ))
    .expect("desktop build configuration");
}
