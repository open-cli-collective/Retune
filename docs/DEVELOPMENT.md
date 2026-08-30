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
store for the encryption key. Last.fm is optional: local builds can read
`RETUNE_LASTFM_API_KEY` and `RETUNE_LASTFM_SHARED_SECRET` from the ignored,
owner-only repo-root `.env.lastfm.local` file. Do not put real credentials in
tracked files or frontend/Vite environment variables.

## Run

```sh
cd apps/desktop
npm ci
npm exec tauri dev
```

Package a production-like release with `npm exec tauri build` from
`apps/desktop`. On macOS, for local release-mode testing without repeated native
credential prompts, run `scripts/build-install.sh` from the repository root.

For direct local Tauri development, create `.env.lastfm.local` with those two
`RETUNE_*` assignments, run `chmod 600 .env.lastfm.local`, then source it before
starting Tauri:

```sh
set -a
. ./.env.lastfm.local
set +a
cd apps/desktop
npm exec tauri dev
```

If the file is absent or either value is empty, Retune remains usable and shows
Last.fm as unavailable. Release builds receive the credentials only on the
trusted native bundle step and fail there if either value is missing.

Native CI builds the Tauri app bundle on macOS arm64, Windows x64/ARM64, and
Ubuntu 22.04 amd64/arm64. The Windows and Linux jobs run release Rust tests,
including local-file import/playback tests, before building their native bundle;
those jobs are the cross-platform proof that release builds select persistent
native credential stores.

## Release automation

Merging a pull request to `main` releases automatically only when the squash
commit's title passes the pinned conventional-commit check, has the `feat` or
`fix` conventional type (including scoped forms such as `feat(scope):`), and
changes a release-worthy path (`apps/**`, `crates/**`, root Cargo files,
`packaging/**`, `scripts/**`, or a release workflow). Other conventional types
and unrelated paths skip the release gate. The automatic
workflow derives `MAJOR.MINOR` from `apps/desktop/src-tauri/tauri.conf.json`
and creates `v<major>.<minor>.<run-number>`; with the current `0.3.0` baseline,
the first live tag is expected to be `v0.3.1`.

Pushing a strict tag such as `v0.3.1` runs the native release workflow. The tag
must match the configured release line and point to a commit reachable from
`main`. It builds and publishes exactly these assets: `Retune-<version>-aarch64.tar.gz`,
`Retune-<version>-windows-x64-setup.exe`,
`Retune-<version>-windows-arm64-setup.exe`,
`retune_<version>_amd64.deb`, `retune_<version>_arm64.deb`, and
`checksums.txt`. The tag version is passed to Tauri through its `--config`
override so package metadata matches the release tag. The automatic
workflow's `workflow_dispatch` only evaluates the release gate and reports the
computed tag; it never builds or pushes. The Release workflow's
`workflow_dispatch` builds, signs, verifies, and aggregates the same artifacts
against the selected ref without creating a release or dispatching package
channels. Before merging, dispatch the existing Release workflow against the
feature branch for packaging validation. Once this automatic workflow exists
on `main`, its manual dispatch can validate the gate and computed tag.

To start a new release line, update the checked-in Tauri version, desktop Cargo
version, and matching `Cargo.lock` package entry together (for example,
`0.4.0`); do not add a `version.txt` file. The automatic workflow then uses
that line for subsequent tags.

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
