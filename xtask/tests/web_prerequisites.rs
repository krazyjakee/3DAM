use std::process::Command;

fn run_without_path(task: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg(task)
        .env("PATH", "")
        .output()
        .expect("xtask should launch by absolute path")
}

#[test]
fn wasm_fails_actionably_when_wasm_pack_is_missing() {
    let output = run_without_path("wasm");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("required tool `wasm-pack` was not found"));
    assert!(stderr.contains("cargo install wasm-pack"));
}

#[cfg(unix)]
#[test]
fn wasm_rejects_an_unpinned_optimizer_toolchain() {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = std::env::temp_dir().join(format!(
        "3dam-xtask-test-wasm-version-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&bin_dir).unwrap();
    let wasm_pack = bin_dir.join("wasm-pack");
    std::fs::write(&wasm_pack, "#!/bin/sh\necho 'wasm-pack 99.0.0'\n").unwrap();
    let mut permissions = std::fs::metadata(&wasm_pack).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wasm_pack, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("wasm")
        .env("PATH", &bin_dir)
        .output()
        .expect("xtask should launch by absolute path");
    std::fs::remove_dir_all(bin_dir).unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("require wasm-pack 0.13.1"));
    assert!(stderr.contains("found `wasm-pack 99.0.0`"));
}

#[cfg(unix)]
#[test]
fn web_fails_before_install_when_wasm_pack_is_missing() {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = std::env::temp_dir().join(format!("3dam-xtask-test-pnpm-{}", std::process::id()));
    std::fs::create_dir_all(&bin_dir).unwrap();
    let pnpm = bin_dir.join("pnpm");
    std::fs::write(&pnpm, "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = std::fs::metadata(&pnpm).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&pnpm, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("web")
        .env("PATH", &bin_dir)
        .output()
        .expect("xtask should launch by absolute path");
    std::fs::remove_dir_all(bin_dir).unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("required tool `wasm-pack` was not found"));
    assert!(!stderr.contains("pnpm install"));
}
