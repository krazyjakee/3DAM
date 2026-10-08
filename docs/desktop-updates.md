# Desktop updates

3DAM uses Tauri 2's updater for its desktop shell. `/updates` is the shared React screen, accessible
from the local desktop sidebar and Help menu. It shows the installed **desktop** version, release
notes, download progress, errors with retry, and an explicit restart action after installation.
The installed app checks the stable release feed at launch and every six hours. A persistent setting
turns automatic checks off. Checking never downloads or installs software without a user action.

Supported installations are Linux AppImage, macOS `.app` (including apps installed from DMG), and
Windows MSI/NSIS. MSI updates use MSI and NSIS updates use NSIS. Linux deb/rpm, unbundled binaries,
and source builds direct users to their package manager or release downloads. Builds without an
embedded updater public key also show manual update instructions.

Updater commands are native IPC, outside `LibraryService` and the server REST API. DTOs live in
`dam-api::updates` and `web/src/api/types.ts`. The capability grants only the five updater commands
to the main loopback webview. The native state independently rejects **all** hosted `--connect`
launches, including connections to loopback, checks the requesting window against the original
embedded server origin (including its ephemeral port), and never accepts URLs, signatures, or keys from page
JavaScript. The updater plugin's generic IPC commands have no capability grant. Browser users update
the web client by updating their server.

## Release signing setup

Updater signing is separate from Apple notarization and Windows Authenticode. The release pipeline
requires updater signatures even though OS signing remains outside its current scope.

1. Generate a long-lived signing pair outside the repository:

   ```sh
   cargo tauri signer generate -w ~/.tauri/3dam-updater.key
   ```

2. Back up the private key securely. Keep it out of version control. Set GitHub repository secret
   `TAURI_SIGNING_PRIVATE_KEY` to the **contents** of that key (not a path on your workstation),
   and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` to its password, if encrypted.
3. Set GitHub repository variable `DAM_UPDATER_PUBLIC_KEY` to the **contents** of the generated
   `.pub` file. Set `plugins.updater.pubkey` in `crates/3dam-desktop/tauri.conf.json` to the same
   public key for local builds. Public keys are safe to commit; private keys and passwords are not.
   The release build embeds the repository variable at compilation and passes it to the bundler.
   Ordinary builds use the committed public key. Remote content cannot replace either trust key.
4. Publish a version tag matching the workspace version. Tag builds fail early without signing
   configuration. All platform jobs must succeed before the manifest and installers are published.

The release pipeline pins Tauri CLI 2.12.1, enables `bundle.createUpdaterArtifacts` for tag builds,
passes the same public key to compilation and signing, collects the `.sig`
files and macOS `.app.tar.gz`, then runs `scripts/generate_updater_manifest.py` to create `latest.json`
from the exact published names. The manifest includes release notes and installer-specific Windows
keys. Missing artifacts or empty signatures prevent publication. `SHA256SUMS` covers the feed too.
Prerelease tags publish as prereleases, so GitHub's `releases/latest` feed continues to select stable
releases. Manual workflow runs and ordinary local bundles do not require a private signing key.

`requireSignedVersion` also binds the announced version to the version signed into the artifact,
so a modified manifest cannot advertise an older signed artifact as a newer version. Local signed
bundles need Tauri CLI 2.12 or newer.

The default feed is `https://github.com/krazyjakee/3DAM/releases/latest/download/latest.json`.
It and its download URLs must be publicly accessible to installed clients. Private GitHub releases
require a public distribution endpoint; do not embed GitHub credentials in the desktop app. Change
the compile-time `DAM_UPDATER_ENDPOINT` environment variable when using another feed. It defaults
to the repository's public release feed, matching `plugins.updater.endpoints` in the Tauri config.

For a local signed release build, export `DAM_UPDATER_PUBLIC_KEY` before compiling `3dam`, and export
`TAURI_SIGNING_PRIVATE_KEY` plus its password before bundling with
`cargo tauri bundle --config '{"bundle":{"createUpdaterArtifacts":true}}'`. The public key must match
the private key used for all subsequent releases. Existing unsigned versions need one manual install
of an updater-enabled release before they can receive updates.

Preferences live in the OS app-config directory as `updates.json`, independently of library data.
Failed downloads or signature checks leave the installed app in place and can be retried. A corrupt
preferences file disables automatic checks and reports the problem without preventing app launch;
saving the setting replaces the file atomically. Windows may exit during installer launch; macOS
and Linux keep running until the user restarts. Finish active jobs before installing or restarting.

## Verification

```sh
cargo test -p dam-desktop
cargo clippy -p dam-desktop --all-targets -- -D warnings
python3 -m unittest discover -s scripts -p test_updater_manifest.py
pnpm --dir web test
pnpm --dir web lint
pnpm --dir web build
```

A real upgrade smoke test needs two signed versioned bundles on each supported platform. Launch
the older bundle, publish the newer manifest, review the update, install, restart, and verify the
new desktop version and retained library data. Repeat with a modified artifact or incorrect
signature and confirm installation is rejected. This cross-platform installer test cannot be
replaced by the mocked UI tests.

UI previews (using a mocked available release): [desktop](images/desktop-updates/updates-desktop.png)
and [narrow screen](images/desktop-updates/updates-narrow.png). Both layouts were checked for
horizontal overflow.

Reference: [Tauri updater documentation](https://v2.tauri.app/plugin/updater/).
