# Development

## Prerequisites

- macOS 11+ arm64, Windows 10/11 x64/ARM64 with WebView2 105+, or Ubuntu
  22.04 amd64/arm64 with WebKitGTK 4.1
- Rust stable, Node.js 22, and npm
- Xcode command-line build tools on macOS
- Microsoft C++ Build Tools on Windows with the `Desktop development with C++`
  workload, including the target-architecture tools for ARM64 builds
- Microsoft Edge WebView2 Runtime 105 or newer on Windows; the NSIS installer
  updates older runtimes
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

Package a local release-mode build with `npm exec tauri build` from
`apps/desktop`. Local bundles are not distributable release artifacts because
they do not pass the credentialed signing/notarization workflow. On macOS, for
local release-mode testing without repeated native
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
trusted native bundle step and fail there if either value is missing. Hosted
release builds map the repository variable `LASTFM_API_KEY` and repository
secret `LASTFM_API_SECRET` to those backend-only values; CI receives neither.

Native CI builds the Tauri app bundle on macOS arm64 and Ubuntu 22.04
amd64/arm64. Supported x64 Windows runners cross-build explicit x64 and ARM64
MSVC targets; native x64 and ARM64 runners then install and inspect the
resulting packages. The Windows and Linux build jobs run release Rust tests,
including local-file import/playback tests, before bundling; those jobs are the
cross-platform proof that release builds select persistent native credential
stores.

## Release automation

Merging a pull request to `main` releases automatically only when the squash
commit's title passes the pinned conventional-commit check, has the `feat` or
`fix` conventional type (including scoped forms such as `feat(scope):`), and
changes a release-worthy path (`apps/**`, `crates/**`, root Cargo files,
`packaging/**`, `scripts/**`, or a release workflow). Other conventional types
and unrelated paths skip the release gate. The automatic
workflow derives `MAJOR.MINOR` from `apps/desktop/src-tauri/tauri.conf.json`
and creates `v<major>.<minor>.<run-number>`. The automatic-release action owns
the numeric suffix; do not predict or reuse it manually.

Pushing a strict tag such as `v0.3.1` runs the native release workflow. The tag
must match the configured release line and point to a commit reachable from
`main`. It builds and publishes exactly these assets: `Retune-<version>-aarch64.zip`,
`Retune-<version>-windows-x64-setup.exe`,
`Retune-<version>-windows-arm64-setup.exe`,
`retune_<version>_amd64.deb`, `retune_<version>_arm64.deb`, and
`checksums.txt`. The tag version is passed to Tauri through its `--config`
override so package metadata matches the release tag. The automatic
workflow's `workflow_dispatch` only evaluates the release gate and reports the
computed tag; it never builds or pushes. The Release workflow's
`workflow_dispatch` builds, signs, verifies, and aggregates the same artifacts
against the selected ref without creating a release or dispatching package
channels. Every tag and manual release candidate must already be reachable from
`main`; the prepare job rejects any other ref before a build job can access
signing credentials. Merge the candidate to `main`, then dispatch the Release
workflow against that main-ancestry commit. The automatic workflow's manual
dispatch can separately validate its gate and computed tag.

To start a new release line, update the checked-in Tauri version, desktop Cargo
version, and matching `Cargo.lock` package entry together (for example,
`0.4.0`); do not add a `version.txt` file. The automatic workflow then uses
that line for subsequent tags.

Run the local release contract check with:

```sh
node scripts/check-release.mjs
```

Tag and manual dry-run releases require the repository variable
`LASTFM_API_KEY` and the following secrets in a protected GitHub Actions
environment named `release`:

- Native packaging: `LASTFM_API_SECRET`.
- macOS: `MACOS_CERT_P12` (base64 Developer ID Application `.p12`),
  `MACOS_CERT_PASSWORD`, `MACOS_CERT_CN`, `MACOS_CERT_LEAF_SHA`,
  `MACOS_TEAM_ID`, `APPLE_API_ISSUER`, `APPLE_API_KEY`, and
  `APPLE_API_KEY_P8_BASE64` (base64 App Store Connect API `.p8` key).
- Windows Artifact Signing: `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`,
  `AZURE_CLIENT_SECRET`, `AZURE_ARTIFACT_SIGNING_ENDPOINT`,
  `AZURE_ARTIFACT_SIGNING_ACCOUNT`,
  `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE`, and
  `WINDOWS_SIGNING_SUBJECT` (the exact expected Authenticode subject).
- Publication: `TAP_GITHUB_TOKEN` and `WINGET_GITHUB_TOKEN`.

`LINUX_PACKAGES_DISPATCH_TOKEN` is optional; when present it dispatches the APT
repository update and also belongs in the `release` environment. Configure that
environment to allow deployments only from `main` and `v*` tags, require
maintainer review, and prevent self-review or administrator bypass where the
repository plan supports those controls. Remove repository-scoped copies after
the environment secrets have been validated. The five-target build, two-target
Windows install-smoke, release aggregate, automatic tag, Homebrew, Linux
dispatch, and Winget jobs all declare the environment, so an edited branch
workflow cannot read credentials or publish a GitHub release without satisfying
its deployment rules.

The macOS build uses Tauri's Developer ID path, hardened
runtime, secure timestamp, Apple notarization, and stapling. Windows builds
install `artifact-signing-cli` at the workflow-pinned version and configure it
as Tauri's object-form signing command. Tauri patches the target executable and
then signs that payload, the NSIS uninstaller, and the outer installer in one
bundle invocation. The Microsoft Artifact Signing client authenticates only
through `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, and `AZURE_CLIENT_SECRET`; the endpoint,
account, and profile are validated before being written to the runner-local
release override. The command explicitly selects SHA-256 file and timestamp
digests and Microsoft's RFC 3161 timestamp service. Release verification checks
the exact identities, timestamps, macOS Gatekeeper assessment/stapled ticket,
and Windows Authenticode trust before upload.

After each Windows installer is uploaded as an immutable workflow artifact, a
downstream matrix downloads it onto native x64 and ARM64 Windows runners. The
job verifies the outer signature, silently installs into a fresh temporary
directory, verifies the installed payload's signer, timestamp, and SignTool
policy, and reads its PE header to require the matching x64 or ARM64 machine
type. Aggregation and GitHub release publication wait for both checks, and the
write-capable aggregate job is protected by the `release` environment. The
smoke does not launch Retune because a headless hosted runner cannot reliably
validate its window/tray lifecycle, OAuth browser handoff, media controls, or
audio-device behavior; those journeys remain in the manual native pass.
`TAP_GITHUB_TOKEN` needs write access to
`open-cli-collective/homebrew-tap`; `WINGET_GITHUB_TOKEN` needs release-asset
read access and Winget submission access; `LINUX_PACKAGES_DISPATCH_TOKEN` needs
repository-dispatch access to `open-cli-collective/linux-packages`.

Release credentials are available only to jobs that declare the protected
`release` environment and are never written into the repository or frontend.
The Apple API key is decoded to an owner-only runner-temporary file and removed
after packaging. The release contract deliberately fails if either platform
loses its production trust requirements.

## Checks

Run the same checks used by CI:

```sh
node scripts/check-docs.mjs
node scripts/check-release.mjs
node scripts/check-tauri-acl.mjs
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/desktop
npm ci
npx tauri permission list
npm run test
npm run lint
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
