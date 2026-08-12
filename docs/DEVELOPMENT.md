# Development

## Prerequisites

- macOS arm64, Windows x64/ARM64, or Ubuntu 22.04 amd64/arm64
- Rust stable, Node.js 22, and npm
- Xcode command-line build tools on macOS
- Microsoft C++ Build Tools on Windows with the `Desktop development with C++`
  workload, including the target-architecture tools for ARM64 builds
- Microsoft Edge WebView2 Runtime on Windows; it is normally already present on
  supported Windows 10/11 systems
- Tauri's Linux build dependencies plus `libasound2-dev` and `libdbus-1-dev` on
  Ubuntu
- A Spotify Premium account and Spotify application client ID for Spotify paths

Configure the Spotify application's redirect URI to the loopback address shown
by Retune. Debug builds and `scripts/build-install.sh` use the app's development
token file; release builds encrypt tokens and use the platform-native credential
store for the encryption key.

## Run

```sh
cd apps/desktop
npm ci
npm exec tauri dev
```

Package a production-like release with `npm exec tauri build` from
`apps/desktop`. On macOS, for local release-mode testing without repeated native
credential prompts, run `scripts/build-install.sh` from the repository root.

Native CI builds the Tauri app bundle on macOS arm64, Windows x64/ARM64, and
Ubuntu 22.04 amd64/arm64. The Windows and Linux jobs run release Rust tests,
including local-file import/playback tests, before building their native bundle;
those jobs are the cross-platform proof that release builds select persistent
native credential stores.

## Release automation

Pushing a tag such as `v0.2.1` runs the native release workflow. It builds and
publishes exactly these assets: `Retune-<version>-aarch64.tar.gz`,
`Retune-<version>-windows-x64-setup.exe`,
`Retune-<version>-windows-arm64-setup.exe`,
`retune_<version>_amd64.deb`, `retune_<version>_arm64.deb`, and
`checksums.txt`. `workflow_dispatch` is a dry run: it builds, signs, verifies,
and aggregates the same artifacts without creating a release or dispatching
package channels.

Run the local release contract check with:

```sh
node scripts/check-release.mjs
```

Tag releases require these repository secrets: `MACOS_CERT_P12`,
`MACOS_CERT_PASSWORD`, `MACOS_CERT_CN`, `MACOS_CERT_LEAF_SHA` (exactly
`42e1afd02aae8666c09c15f171e1639550f301c2`), `TAP_GITHUB_TOKEN`,
and `WINGET_GITHUB_TOKEN`. `LINUX_PACKAGES_DISPATCH_TOKEN` is optional; when
present it dispatches the APT repository update. The macOS build is stable-signed
but not timestamped, hardened, or notarized; Homebrew clears
quarantine. The first access to stored Spotify credentials after an older
ad-hoc build may show one macOS Keychain prompt; choose Always Allow once.
`TAP_GITHUB_TOKEN` needs write access to
`open-cli-collective/homebrew-tap`; `WINGET_GITHUB_TOKEN` needs release-asset
read access and Winget submission access; `LINUX_PACKAGES_DISPATCH_TOKEN` needs
repository-dispatch access to `open-cli-collective/linux-packages`.

## Checks

Run the same checks used by CI:

```sh
node scripts/check-docs.mjs
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/desktop
npm ci
npm run test
npm run lint
npx tsc --noEmit -p tsconfig.app.json
npm run build
```

OAuth loopback tests need permission to bind a local socket in restricted test
environments. Tests requiring a real audio device are intentionally ignored by
default.

## Manual UI validation

Automated tests cover domain behavior and extracted UI logic, but native window,
drag, menu, scrolling, and audio behavior still need an app-level pass. For a
change in those areas:

1. Launch with `npm exec tauri dev`.
2. Exercise the smallest affected journey with both narrow and wide windows.
3. Check light and dark appearances when styling is involved.
4. Inspect the terminal and in-app error notices.
5. For Spotify changes, verify both signed-in success and disconnected/error UI.

Do not make CI depend on live Spotify credentials or an audio device.

## Documentation rule

`AGENTS.md` is the index, `ARCHITECTURE.md` is the system map, and
`docs/architecture/` contains current domain truth. If code changes an
architectural boundary, invariant, persistence format, or external contract,
update that document in the same commit. Delete completed plans after durable
decisions have been incorporated.
